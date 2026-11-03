//! Session model and effort control on a shared Runtime and over the wire:
//! capability parity, `config_changed` to every watcher, attach snapshots,
//! restart, explicit off, busy and concurrent setters.

use loop_app_core::runtime::{BootstrapOpts, GlobalResources};
use loop_protocol::rpc::PeerInfo;
use loop_protocol::{
    AgentBackend, BackendError, ConfigChanged, ContentBlock, HostServices, InteractionOutcome,
    InteractionRequest, ModelControl, SessionKey, SessionSpec, SessionUpdate, UpdateEnvelope,
};
use loop_runtime::log::{EventKind, EventLog, MemoryEventLog, SqliteEventLog};
use loop_runtime::{Runtime, RuntimeLimits};
use loop_wire::server::{serve_with_control, ServeOptions, Served};
use loop_wire::transport::memory;
use loop_wire::RemoteBackend;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

fn opts(cwd: &std::path::Path) -> BootstrapOpts {
    BootstrapOpts {
        cwd: cwd.into(),
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
    }
}

fn reasoning(template: &loop_ai::Model, id: &str) -> loop_ai::Model {
    let mut model = template.clone();
    model.id = id.into();
    model.name = id.into();
    model.reasoning = true;
    model
}

/// A Runtime whose faux models accept reasoning, with `medium` as the saved
/// default effort. `script` answers prompts.
async fn runtime(
    dir: &std::path::Path,
    log: Arc<dyn EventLog>,
    script: loop_ai::providers::FauxScript,
) -> Arc<Runtime> {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultThinkingLevel":"medium"}"#,
    )
    .unwrap();
    let provider = loop_ai::providers::faux_provider(script);
    let template = provider.get_model("faux-model").unwrap();
    provider.set_models(vec![
        reasoning(&template, "faux-model"),
        reasoning(&template, "second"),
    ]);
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(provider);
    let globals = GlobalResources::with_models(agent, models).unwrap();
    Runtime::new(globals, opts(dir), log, RuntimeLimits::default())
        .await
        .unwrap()
        .into_shared()
}

fn info() -> PeerInfo {
    PeerInfo {
        name: "test".into(),
        version: "test".into(),
    }
}

struct Reject;
#[async_trait::async_trait]
impl HostServices for Reject {
    async fn interact(&self, _: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Cancelled
    }
}

async fn connect(rt: &Arc<Runtime>) -> (Arc<RemoteBackend>, Served) {
    let (server, client) = memory();
    let served = serve_with_control(
        rt.clone(),
        rt.clone(),
        server,
        info(),
        ServeOptions {
            owns_backend: false,
        },
    );
    let remote = RemoteBackend::connect(client, info(), Arc::new(Reject))
        .await
        .unwrap();
    (remote, served)
}

/// The next `config_changed` of `key`, with its sequence.
async fn config_changed(
    rx: &mut Receiver<UpdateEnvelope>,
    key: &SessionKey,
) -> (u64, ConfigChanged) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for config_changed")
            .unwrap();
        if &envelope.session != key {
            continue;
        }
        if let SessionUpdate::ConfigChanged(changed) = envelope.update {
            return (envelope.seq, changed);
        }
    }
}

#[tokio::test]
async fn capabilities_and_services_agree_on_native_runtime_and_remote() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (remote, _served) = connect(&rt).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();

        let caps = rt.capabilities();
        let remote_caps = remote.capabilities();
        for (name, runtime, remote) in [
            ("set_model", caps.set_model, remote_caps.set_model),
            ("efforts", caps.efforts, remote_caps.efforts),
            ("modes", caps.modes, remote_caps.modes),
            (
                "attach_snapshot",
                caps.attach_snapshot,
                remote_caps.attach_snapshot,
            ),
        ] {
            assert_eq!(runtime, remote, "{name} differs between Runtime and Remote");
        }
        assert!(caps.set_model && caps.efforts && caps.attach_snapshot);
        assert!(!caps.modes, "native Sessions offer no backend modes yet");
        assert!(rt.model_control().is_some());
        assert!(remote.model_control().is_some());

        // The native backend underneath advertises the same model services.
        let native_caps = native().await.capabilities();
        assert_eq!(
            (
                native_caps.set_model,
                native_caps.efforts,
                native_caps.modes
            ),
            (caps.set_model, caps.efforts, caps.modes)
        );

        // Same answers through the Runtime and over the wire.
        let control = remote.model_control().unwrap();
        assert_eq!(
            control.efforts(&session).await.unwrap(),
            rt.efforts(&session).await.unwrap()
        );
        assert_eq!(
            control.models(&session).await.unwrap(),
            rt.models(&session).await.unwrap()
        );
        let models = remote.session_models(&session).await.unwrap();
        assert!(models.models.iter().all(|m| m.backend.as_str() == "loop"));
        assert_eq!(models.current.as_deref(), Some("faux/faux-model"));
        // Unsupported services answer Unsupported, not "unknown method".
        assert!(matches!(
            control.modes(&session).await,
            Err(BackendError::Unsupported(_))
        ));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_watchers_see_each_committed_change_at_the_setters_sequence() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let (owner, _s1) = connect(&rt).await;
        let (watcher, _s2) = connect(&rt).await;
        let session = owner.new_session(SessionSpec::default()).await.unwrap();
        let mut a = owner.subscribe();
        let mut b = watcher.subscribe();
        let attached = watcher.attach(&session, 0).await.unwrap();
        let snapshot = attached.snapshot.expect("the Runtime sends a snapshot");
        assert_eq!(snapshot.config.effort.as_deref(), Some("medium"));
        assert_eq!(snapshot.at_seq, attached.latest_seq);

        let control = owner.model_control().unwrap();
        let result = control
            .set_effort(&session, "high", Some(snapshot.revision))
            .await
            .unwrap();
        assert!(!result.pending);
        for rx in [&mut a, &mut b] {
            let (seq, changed) = config_changed(rx, &session.key).await;
            assert_eq!(seq, result.seq);
            assert_eq!(changed.revision, result.revision);
            assert_eq!(changed.config.effort.as_deref(), Some("high"));
        }
        let result = control
            .set_model(&session, "faux/second", Some(result.revision))
            .await
            .unwrap();
        for rx in [&mut a, &mut b] {
            let (seq, changed) = config_changed(rx, &session.key).await;
            assert_eq!(seq, result.seq);
            assert_eq!(changed.config.model.as_deref(), Some("faux/second"));
        }
        // Committed before the reply: the Event Log already has it.
        let logged = log.read_after(&session.key, 0).await.unwrap();
        assert!(logged.iter().any(|r| matches!(
            &r.event.kind,
            EventKind::Update(e) if e.seq == result.seq
                && matches!(e.update, SessionUpdate::ConfigChanged(_))
        )));
        // A stale revision conflicts over the wire with the current one.
        assert_eq!(
            control
                .set_effort(&session, "low", Some(0))
                .await
                .unwrap_err(),
            BackendError::Conflict {
                revision: result.revision
            }
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_change_survives_detach_and_reattach() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (owner, _s1) = connect(&rt).await;
        let (watcher, _s2) = connect(&rt).await;
        let session = owner.new_session(SessionSpec::default()).await.unwrap();
        let first = watcher.attach(&session, 0).await.unwrap();
        watcher.detach(&session).await.unwrap();

        // Changed while the watcher was away.
        let control = owner.model_control().unwrap();
        control.set_effort(&session, "off", None).await.unwrap();
        let last = control
            .set_model(&session, "faux/second", None)
            .await
            .unwrap();

        let mut rx = watcher.subscribe();
        let again = watcher.attach(&session, first.latest_seq).await.unwrap();
        let snapshot = again.snapshot.unwrap();
        assert_eq!(again.latest_seq, last.seq);
        assert_eq!(snapshot.at_seq, last.seq);
        assert_eq!(snapshot.revision, last.revision);
        assert_eq!(snapshot.config.effort.as_deref(), Some("off"));
        assert_eq!(snapshot.config.model.as_deref(), Some("faux/second"));
        assert!(!snapshot.pending);
        // Replay brings the missed updates, ending with the latest change.
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(changed.config.effort.as_deref(), Some("off"));
        assert!(seq > first.latest_seq);
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, last.seq);
        assert_eq!(changed.config.model.as_deref(), Some("faux/second"));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn model_and_explicit_off_survive_a_runtime_restart() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let session = {
            let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
            let rt = runtime(dir.path(), log, Default::default()).await;
            let session = rt.new_session(SessionSpec::default()).await.unwrap();
            assert_eq!(
                rt.efforts(&session).await.unwrap().current.as_deref(),
                Some("medium")
            );
            rt.set_model(&session, "faux/second", None).await.unwrap();
            rt.set_effort(&session, "off", None).await.unwrap();
            rt.shutdown().await;
            session
        };
        let log = Arc::new(SqliteEventLog::open(&db).await.unwrap());
        let rt = runtime(dir.path(), log, Default::default()).await;
        // Not loaded yet: an attach snapshot reads the Session's tree.
        let (watcher, _s) = connect(&rt).await;
        let attached = watcher.attach(&session, 0).await.unwrap();
        let recorded = attached.snapshot.unwrap();
        assert_eq!(recorded.config.effort.as_deref(), Some("off"));
        assert_eq!(recorded.config.model.as_deref(), Some("faux/second"));
        assert_eq!(recorded.revision, 2);
        watcher.detach(&session).await.unwrap();

        rt.load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let efforts = rt.efforts(&session).await.unwrap();
        assert_eq!(
            efforts.current.as_deref(),
            Some("off"),
            "an explicit off stays off"
        );
        assert_eq!(efforts.revision, 2, "revisions continue across restarts");
        assert_eq!(
            rt.models(&session).await.unwrap().current.as_deref(),
            Some("faux/second"),
            "the Session's model wins over the Runtime's startup default"
        );
        // A Session that never chose inherits the saved default.
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(
            rt.efforts(&fresh).await.unwrap().current.as_deref(),
            Some("medium")
        );
        assert_eq!(
            rt.models(&fresh).await.unwrap().current.as_deref(),
            Some("faux/faux-model")
        );
        assert_eq!(
            rt.set_effort(&session, "low", Some(1)).await.unwrap_err(),
            BackendError::Conflict { revision: 2 }
        );
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn during_a_turn_the_model_is_refused_and_the_effort_waits() {
    tokio::time::timeout(Duration::from_secs(30), async {
        use loop_ai::providers::{FauxResponse, FauxScript};
        let script = FauxScript::new();
        script.push(FauxResponse::ToolCalls(vec![loop_ai::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo waiting"}),
            thought_signature: None,
        }]));
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(MemoryEventLog::new());
        let rt = runtime(dir.path(), log.clone(), script).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let mut rx = rt.subscribe();
        // Nobody can answer, so the turn waits on its Escalation.
        let turn = rt
            .prompt(&session, vec![ContentBlock::text("run it")])
            .await
            .unwrap();
        loop {
            let raised = log
                .read_runtime_after(0)
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e.event.kind, EventKind::EscalationRaised(_)));
            if raised {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            rt.set_model(&session, "faux/second", None)
                .await
                .unwrap_err(),
            BackendError::Busy
        );
        let result = rt.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        assert!(result.applies_after_turn.is_some());
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, result.seq);
        assert!(changed.pending);
        assert_eq!(changed.config.effort.as_deref(), Some("high"));
        assert_eq!(changed.effective.effort.as_deref(), Some("medium"));

        rt.cancel(&session);
        turn.outcome().await;
        let (_, applied) = config_changed(&mut rx, &session.key).await;
        assert!(!applied.pending);
        assert_eq!(applied.revision, result.revision);
        assert_eq!(applied.effective.effort.as_deref(), Some("high"));
        // Now idle: the model can change.
        rt.set_model(&session, "faux/second", None).await.unwrap();
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_setters_over_the_wire_commit_in_revision_order() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(
            dir.path(),
            Arc::new(MemoryEventLog::new()),
            Default::default(),
        )
        .await;
        let (first, _s1) = connect(&rt).await;
        let (second, _s2) = connect(&rt).await;
        let session = first.new_session(SessionSpec::default()).await.unwrap();
        second.attach(&session, 0).await.unwrap();
        let levels = [
            "off", "low", "medium", "high", "max", "minimal", "xhigh", "low",
        ];
        let tasks: Vec<_> = levels
            .into_iter()
            .enumerate()
            .map(|(i, level)| {
                let client = if i % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let session = session.clone();
                tokio::spawn(async move {
                    let result = client
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, None)
                        .await
                        .unwrap();
                    (result, level)
                })
            })
            .collect();
        let mut done = Vec::new();
        for task in tasks {
            done.push(task.await.unwrap());
        }
        done.sort_by_key(|(result, _)| result.revision);
        let revisions: Vec<u64> = done.iter().map(|(r, _)| r.revision).collect();
        assert_eq!(revisions, (1..=levels.len() as u64).collect::<Vec<_>>());
        // Later revisions were committed later.
        assert!(done.windows(2).all(|w| w[0].0.seq < w[1].0.seq));
        let (last, level) = done.last().unwrap();
        let efforts = rt.efforts(&session).await.unwrap();
        assert_eq!(efforts.current.as_deref(), Some(*level));
        assert_eq!(efforts.revision, last.revision);

        // Racing on the same expected revision: one wins, one conflicts.
        let racers: Vec<_> = [(first.clone(), "off"), (second.clone(), "high")]
            .into_iter()
            .map(|(client, level)| {
                let session = session.clone();
                let expected = last.revision;
                tokio::spawn(async move {
                    client
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, Some(expected))
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for racer in racers {
            match racer.await.unwrap() {
                Ok(_) => won += 1,
                Err(error) => assert_eq!(
                    error,
                    BackendError::Conflict {
                        revision: last.revision + 1
                    }
                ),
            }
        }
        assert_eq!(won, 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// A standalone native backend, as the terminal app uses it.
async fn native() -> Arc<loop_agent::native_backend::NativeBackend> {
    use loop_agent::harness::{
        create_in_memory_session_store, create_session_repository, AgentHarness,
        AgentHarnessOptions, ExecutionEnv, HostExecutionEnv, SandboxMode,
    };
    let models = Arc::new(loop_ai::Models::new());
    models.set_provider(loop_ai::providers::faux_provider(Default::default()));
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        model: models.get_model("faux", "faux-model"),
        models,
        session: repo.create(None, None).await.unwrap(),
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    loop_agent::native_backend::NativeBackend::new(harness).await
}

/// Holds Update appends while `held`, to stall the Event Log.
#[derive(Default)]
struct GatedLog {
    inner: MemoryEventLog,
    held: std::sync::atomic::AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl EventLog for GatedLog {
    async fn append_batch(
        &self,
        events: Vec<loop_runtime::log::Event>,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        if events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Update(_)))
            && self.held.load(std::sync::atomic::Ordering::Acquire)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.append_batch(events).await
    }
    async fn read_after(
        &self,
        key: &SessionKey,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_after(key, seq).await
    }
    async fn read_runtime_after(
        &self,
        seq: u64,
    ) -> loop_runtime::log::Result<Vec<loop_runtime::log::LoggedEvent>> {
        self.inner.read_runtime_after(seq).await
    }
    async fn latest_seq(&self, key: &SessionKey) -> loop_runtime::log::Result<u64> {
        self.inner.latest_seq(key).await
    }
    async fn allocate_generation(
        &self,
        owner: &str,
        pid: u32,
        ms: i64,
    ) -> loop_runtime::log::Result<u64> {
        self.inner.allocate_generation(owner, pid, ms).await
    }
    async fn finish_generation(&self, generation: u64, ms: i64) -> loop_runtime::log::Result<()> {
        self.inner.finish_generation(generation, ms).await
    }
}

#[tokio::test]
async fn a_setter_waiting_on_a_stalled_event_log_holds_up_nothing_else() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(GatedLog::default());
        let rt = runtime(dir.path(), log.clone(), Default::default()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        log.held.store(true, std::sync::atomic::Ordering::Release);
        let setter = tokio::spawn({
            let rt = rt.clone();
            let session = session.clone();
            async move { rt.set_effort(&session, "high", None).await }
        });
        log.started.notified().await;

        // Attach, listing and prompt admission go ahead meanwhile.
        let quick = Duration::from_secs(5);
        let (watcher, _served) = connect(&rt).await;
        let mut rx = watcher.subscribe();
        let attached = tokio::time::timeout(quick, watcher.attach(&session, 0))
            .await
            .expect("attach waits for no setter")
            .unwrap();
        let snapshot = attached.snapshot.unwrap();
        assert_eq!(snapshot.config.effort.as_deref(), Some("medium"));
        tokio::time::timeout(quick, rt.list())
            .await
            .unwrap()
            .unwrap();
        let turn = tokio::time::timeout(
            quick,
            rt.prompt(&session, vec![ContentBlock::text("meanwhile")]),
        )
        .await
        .expect("admission waits for no setter")
        .unwrap();
        assert!(!setter.is_finished(), "the setter waits for its commit");

        log.held.store(false, std::sync::atomic::Ordering::Release);
        log.release.notify_one();
        let result = setter.await.unwrap().unwrap();
        let (seq, changed) = config_changed(&mut rx, &session.key).await;
        assert_eq!(seq, result.seq);
        assert!(seq > snapshot.at_seq);
        assert_eq!(changed.revision, result.revision);
        turn.outcome().await;
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
