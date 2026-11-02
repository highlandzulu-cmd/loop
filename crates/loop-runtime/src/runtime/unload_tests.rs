//! Unloading idle Sessions at the `max_sessions` cap, and its races with
//! attach, prompt and Escalation registration.
use super::*;
use crate::log::{EventLog, LoggedEvent, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall, UserContent, UserMessageContent,
};
use std::pin::Pin;

const LIMIT: Duration = Duration::from_secs(60);

type Script =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> + Send + Sync>;

fn first_prompt(context: &Context) -> String {
    context
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User(user) => Some(match &user.content {
                UserMessageContent::Text(text) => text.clone(),
                UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}
fn ready(r: FauxResponse) -> Pin<Box<dyn Future<Output = FauxResponse> + Send>> {
    Box::pin(std::future::ready(r))
}
fn tool(name: &str, arguments: serde_json::Value) -> FauxResponse {
    static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    FauxResponse::ToolCalls(vec![ToolCall {
        id: format!("u-{}", IDS.fetch_add(1, Ordering::Relaxed)),
        name: name.into(),
        arguments,
        thought_signature: None,
    }])
}
/// Answers by the Session's first prompt: `park` asks for an approval that
/// no Surface answers, `spawn` opens a Subagent that parks, anything else
/// is answered at once.
fn default_script() -> Script {
    Arc::new(|task: String| {
        if task.starts_with("park") {
            ready(tool("bash", json!({"command": "echo parked"})))
        } else if task.starts_with("spawn") {
            ready(tool("subagent", json!({"task": "park child"})))
        } else {
            ready(FauxResponse::Text(format!("done {task}")))
        }
    })
}

async fn runtime(
    dir: &std::path::Path,
    max_sessions: usize,
) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    let script = default_script();
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(move |context: &Context| {
        // Only the first model call of a turn follows the script.
        if context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)))
        {
            return ready(FauxResponse::Text("finished".into()));
        }
        script(first_prompt(context))
    })));
    let globals = GlobalResources::with_models(dir.join("agent"), models).unwrap();
    let opts = BootstrapOpts {
        cwd: dir.into(),
        provider: Some("faux".into()),
        model: Some("faux-model".into()),
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: true,
        permission_mode: Some("ask".into()),
        agent: None,
    };
    let log = Arc::new(MemoryEventLog::new());
    let rt = Runtime::new(
        globals,
        opts,
        log.clone(),
        RuntimeLimits {
            max_sessions,
            max_active_turns: 8,
        },
    )
    .await
    .unwrap()
    .with_close_grace(Duration::from_millis(500))
    .into_shared();
    (rt, log)
}

async fn run(rt: &Runtime, session: &SessionRef, prompt: &str) -> TurnOutcome {
    rt.prompt(session, vec![ContentBlock::text(prompt)])
        .await
        .unwrap()
        .outcome()
        .await
}

fn loaded(rt: &Runtime, session: &SessionRef) -> bool {
    rt.sessions.lock().unwrap().contains_key(&session.key)
}

fn closed_reasons(events: &[LoggedEvent], key: &SessionKey) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event.session_key.as_ref() == Some(key))
        .filter_map(|e| match &e.event.kind {
            EventKind::SessionClosed(p) => Some(p["reason"].as_str().unwrap_or("").to_owned()),
            _ => None,
        })
        .collect()
}

/// Wait until `session` has a parked Escalation.
async fn parked(rt: &Runtime, session: &SessionRef) {
    let live = rt.route(session).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !live.escalations.has_pending().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no Escalation was parked");
}

struct Watching;
#[async_trait]
impl EscalationRecipient for Watching {
    fn withdraw(&self, _: loop_protocol::rpc::InteractWithdrawnParams) {}
    async fn interact(
        &self,
        _: loop_protocol::rpc::InteractParams,
        withdrawn: oneshot::Receiver<()>,
    ) -> Result<loop_protocol::InteractionOutcome, ()> {
        let _ = withdrawn.await;
        Err(())
    }
}
async fn watch(rt: &Runtime, session: &SessionRef, connection: u64) {
    loop_wire::RuntimeControl::recipient(rt, session, connection, Some(Arc::new(Watching)))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_least_recently_active_idle_session_is_unloaded_and_logged() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path(), 2).await;
        let a = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        // `a` was opened first but is now the more recently active one.
        assert!(matches!(
            run(&rt, &a, "hello").await,
            TurnOutcome::Completed(_)
        ));
        let c = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(loaded(&rt, &a) && loaded(&rt, &c));
        assert!(
            !loaded(&rt, &b),
            "the least recently active Session is unloaded"
        );
        assert_eq!(rt.status().loaded, 2);
        assert!(rt.store.lease(&b.native_id).await.unwrap().is_none());
        let events = log.read_runtime_after(0).await.unwrap();
        assert_eq!(closed_reasons(&events, &b.key), ["unloaded"]);
        // Its data is kept: loading it again unloads the next idle one.
        let before = log.latest_seq(&b.key).await.unwrap();
        rt.load_session(&b, SessionSpec::default()).await.unwrap();
        assert!(matches!(
            run(&rt, &b, "again").await,
            TurnOutcome::Completed(_)
        ));
        assert!(log.latest_seq(&b.key).await.unwrap() > before);
        assert_eq!(rt.status().loaded, 2);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attached_busy_and_parked_sessions_are_never_unloaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path(), 2).await;
        let attached = rt.new_session(SessionSpec::default()).await.unwrap();
        watch(&rt, &attached, 1).await;
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&busy, vec![ContentBlock::text("park")])
            .await
            .unwrap();
        parked(&rt, &busy).await;
        assert_eq!(
            rt.new_session(SessionSpec::default()).await,
            Err(BackendError::Busy),
            "every loaded Session is protected"
        );
        assert!(loaded(&rt, &attached) && loaded(&rt, &busy));
        assert_eq!(rt.status().loaded, 2);
        // Detaching makes it eligible.
        loop_wire::RuntimeControl::recipient(rt.as_ref(), &attached, 1, None)
            .await
            .unwrap();
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(!loaded(&rt, &attached) && loaded(&rt, &busy) && loaded(&rt, &fresh));
        rt.cancel(&busy);
        tokio::time::timeout(Duration::from_secs(10), turn.outcome())
            .await
            .expect("cancelled turn never ended");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiting_parent_and_its_subagent_are_never_unloaded() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path(), 3).await;
        let idle = rt.new_session(SessionSpec::default()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let parent = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = rt
            .prompt(&parent, vec![ContentBlock::text("spawn")])
            .await
            .unwrap();
        let child = loop {
            let child = rt
                .sessions
                .lock()
                .unwrap()
                .values()
                .find(|s| s.parent.as_ref() == Some(&parent.key))
                .map(|s| s.reference.clone());
            if let Some(child) = child {
                break child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        parked(&rt, &child).await;
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(!loaded(&rt, &idle), "only the idle root is unloaded");
        assert!(loaded(&rt, &parent) && loaded(&rt, &child) && loaded(&rt, &fresh));
        // `fresh` is attached now; nothing else can go.
        watch(&rt, &fresh, 7).await;
        assert_eq!(
            rt.new_session(SessionSpec::default()).await,
            Err(BackendError::Busy)
        );
        rt.cancel(&parent);
        let _ = turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_racing_eviction_is_never_lost() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            let (opened, prompted) = tokio::join!(
                rt.new_session(SessionSpec::default()),
                rt.prompt(&a, vec![ContentBlock::text(format!("race {round}"))]),
            );
            let prompted = prompted.unwrap_or_else(|e| panic!("round {round}: prompt lost: {e}"));
            assert!(
                matches!(prompted.outcome().await, TurnOutcome::Completed(_)),
                "round {round}"
            );
            match opened {
                Ok(_) | Err(BackendError::Busy) => {}
                Err(error) => panic!("round {round}: {error}"),
            }
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_racing_eviction_ends_loaded_and_watched() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            // The wire's order: attach (snapshot), then register the Surface.
            let attach = async {
                loop_wire::RuntimeControl::attach_watching(rt.as_ref(), 3, &a, 0).await?;
                watch(&rt, &a, 3).await;
                Ok::<_, BackendError>(())
            };
            let (opened, attached) = tokio::join!(rt.new_session(SessionSpec::default()), attach);
            // Either the attach won (no room), or the Session was unloaded
            // and the attach loaded it back before succeeding: a successful
            // attach is never left with its Session unloaded.
            attached.unwrap_or_else(|e| panic!("round {round}: {e} ({opened:?})"));
            assert!(
                loaded(&rt, &a),
                "round {round}: attached Session left unloaded ({opened:?})"
            );
            let live = rt.route(&a).unwrap();
            assert_eq!(live.escalations.connections().await, [3], "round {round}");
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_escalation_raised_while_evicting_keeps_its_session() {
    tokio::time::timeout(LIMIT, async {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let (rt, _log) = runtime(dir.path(), 1).await;
            let a = rt.new_session(SessionSpec::default()).await.unwrap();
            let (opened, prompted) = tokio::join!(
                rt.new_session(SessionSpec::default()),
                rt.prompt(&a, vec![ContentBlock::text("park")]),
            );
            let turn = prompted.unwrap();
            let a = rt
                .sessions
                .lock()
                .unwrap()
                .get(&a.key)
                .map(|s| s.reference.clone())
                .unwrap_or_else(|| panic!("round {round}: prompted Session not loaded"));
            parked(&rt, &a).await;
            // The parked Session can no longer be unloaded.
            if opened.is_ok() {
                assert_eq!(
                    rt.new_session(SessionSpec::default()).await,
                    Err(BackendError::Busy)
                );
            }
            assert!(loaded(&rt, &a), "round {round}");
            rt.cancel(&a);
            let _ = turn.outcome().await;
            rt.shutdown().await;
        }
    })
    .await
    .unwrap();
}
