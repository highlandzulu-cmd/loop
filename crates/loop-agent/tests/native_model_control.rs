//! NativeBackend model control: Session-scoped model and effort, recorded in
//! the Session tree, announced as `config_changed`, restored on load.

use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode, SessionTreeEntry,
};
use loop_agent::native_backend::NativeBackend;
use loop_agent::{AgentThinkingLevel, AgentTool, AgentToolResult};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Model, Models, ToolCall};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, SessionUpdate, TurnOutcome, UpdateEnvelope,
};
use serde_json::json;
use tokio::sync::broadcast;

fn reasoning(id: &str) -> Model {
    let mut model = faux_provider(FauxScript::new())
        .get_model("faux-model")
        .unwrap();
    model.id = id.into();
    model.name = id.into();
    model.reasoning = true;
    model
}

async fn harness(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Arc<AgentHarness> {
    let script = FauxScript::new();
    script.extend(responses);
    let provider = faux_provider(script);
    provider.set_models(vec![reasoning("one"), reasoning("two/with-slash")]);
    let models = Arc::new(Models::new());
    models.set_provider(provider);
    let model = models.get_model("faux", "one").unwrap();
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools,
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    harness.set_thinking_level(AgentThinkingLevel::Medium).await;
    harness
}

async fn config_changed(
    rx: &mut broadcast::Receiver<UpdateEnvelope>,
) -> (u64, loop_protocol::ConfigChanged) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a config_changed update")
            .unwrap();
        if let SessionUpdate::ConfigChanged(changed) = envelope.update {
            return (envelope.seq, changed);
        }
    }
}

#[tokio::test]
async fn effort_and_model_are_recorded_announced_and_listed() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let control = backend
            .model_control()
            .expect("native offers model control");
        assert!(backend.capabilities().set_model && backend.capabilities().efforts);
        assert!(!backend.capabilities().modes);

        let efforts = control.efforts(&session).await.unwrap();
        assert_eq!(efforts.current.as_deref(), Some("medium"));
        assert_eq!(efforts.revision, 0);
        assert!(efforts.models.iter().any(|c| c.id == "max"));

        let mut rx = backend.subscribe();
        let result = control.set_effort(&session, "off", None).await.unwrap();
        assert_eq!(result.revision, 1);
        assert!(!result.pending);
        let (seq, changed) = config_changed(&mut rx).await;
        assert_eq!(seq, result.seq);
        assert_eq!(changed.revision, 1);
        assert_eq!(changed.config.effort.as_deref(), Some("off"));
        assert_eq!(changed.effective, changed.config);
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Off);
        assert_eq!(harness.thinking_level().await, AgentThinkingLevel::Off);

        // Model ids may contain `/`; the provider is the part before the first.
        let models = control.models(&session).await.unwrap();
        assert_eq!(models.current.as_deref(), Some("faux/one"));
        assert!(models.models.iter().any(|m| m.id == "faux/two/with-slash"));
        let result = control
            .set_model(&session, "faux/two/with-slash", Some(1))
            .await
            .unwrap();
        assert_eq!(result.revision, 2);
        let (_, changed) = config_changed(&mut rx).await;
        assert_eq!(changed.config.model.as_deref(), Some("faux/two/with-slash"));
        assert_eq!(harness.model().await.unwrap().id, "two/with-slash");

        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: AgentThinkingLevel::Off,
                ..
            }
        )));
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ModelChange { provider, model_id, .. }
                if provider == "faux" && model_id == "two/with-slash"
        )));

        let error = control
            .set_effort(&session, "ultra", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("choose one of"), "{error}");
        assert!(control
            .set_model(&session, "faux/none", None)
            .await
            .is_err());
        assert!(control.set_model(&session, "nonsense", None).await.is_err());
        assert_eq!(control.efforts(&session).await.unwrap().revision, 2);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn a_stale_revision_conflicts_without_changing_anything() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let control = backend.model_control().unwrap();
        control.set_effort(&session, "low", Some(0)).await.unwrap();
        assert_eq!(
            control
                .set_effort(&session, "high", Some(0))
                .await
                .unwrap_err(),
            BackendError::Conflict { revision: 1 }
        );
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Low);
        assert_eq!(
            control
                .set_model(&session, "faux/one", Some(5))
                .await
                .unwrap_err(),
            BackendError::Conflict { revision: 1 }
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn concurrent_setters_get_distinct_revisions_and_the_last_one_wins() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let levels: [&'static str; 6] = ["off", "low", "medium", "high", "max", "minimal"];
        let tasks: Vec<_> = levels
            .into_iter()
            .map(|level| {
                let backend = Arc::clone(&backend);
                let session = session.clone();
                tokio::spawn(async move {
                    let control = backend.model_control().unwrap();
                    let result = control.set_effort(&session, level, None).await.unwrap();
                    (result.revision, level)
                })
            })
            .collect();
        let mut done = Vec::new();
        for task in tasks {
            done.push(task.await.unwrap());
        }
        done.sort();
        let revisions: Vec<u64> = done.iter().map(|(r, _)| *r).collect();
        assert_eq!(revisions, [1, 2, 3, 4, 5, 6]);
        assert_eq!(
            backend.thinking_level(),
            loop_agent::effort::parse_thinking(done.last().unwrap().1)
        );

        // With the same expected revision, exactly one setter wins.
        let tasks: Vec<_> = ["low", "high"]
            .into_iter()
            .map(|level| {
                let backend = Arc::clone(&backend);
                let session = session.clone();
                tokio::spawn(async move {
                    backend
                        .model_control()
                        .unwrap()
                        .set_effort(&session, level, Some(6))
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(result) => {
                    won += 1;
                    assert_eq!(result.revision, 7);
                }
                Err(error) => assert_eq!(error, BackendError::Conflict { revision: 7 }),
            }
        }
        assert_eq!(won, 1);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn explicit_off_is_restored_as_off_and_absent_inherits_the_default() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = harness(vec![], vec![]).await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        backend.set_default_effort(AgentThinkingLevel::Medium);
        let first = backend.current_session().await;
        let control = backend.model_control().unwrap();
        control.set_effort(&first, "off", None).await.unwrap();

        // A new Session starts from the default.
        let second = backend
            .new_session(loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Medium);
        assert_eq!(control.efforts(&second).await.unwrap().revision, 0);

        // Loading the first again restores its explicit off.
        backend
            .load_session(&first, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Off);
        assert_eq!(harness.thinking_level().await, AgentThinkingLevel::Off);
        assert_eq!(control.efforts(&first).await.unwrap().revision, 1);

        // And the second, which never chose, inherits the default again.
        backend
            .load_session(&second, loop_protocol::SessionSpec::default())
            .await
            .unwrap();
        assert_eq!(backend.thinking_level(), AgentThinkingLevel::Medium);
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn an_effort_change_during_a_turn_applies_after_it() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let wait = AgentTool::simple(
            "wait",
            "Wait",
            "Waits until cancelled.",
            json!({"type": "object", "properties": {}}),
            |_id, _args, cancel, _update| async move {
                if let Some(cancel) = cancel {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    }
                }
                Ok(AgentToolResult::text("waited"))
            },
        );
        let harness = harness(
            vec![
                FauxResponse::ToolCalls(vec![ToolCall {
                    id: "w1".into(),
                    name: "wait".into(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                FauxResponse::Text("after".into()),
            ],
            vec![wait],
        )
        .await;
        let backend = NativeBackend::new(Arc::clone(&harness)).await;
        let session = backend.current_session().await;
        let mut rx = backend.subscribe();
        let handle = backend
            .prompt(&session, vec![ContentBlock::text("wait")])
            .await
            .unwrap();
        let turn = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
                break envelope.turn.unwrap();
            }
        };
        let control = backend.model_control().unwrap();
        let result = control.set_effort(&session, "high", None).await.unwrap();
        assert!(result.pending);
        assert_eq!(result.applies_after_turn.as_ref(), Some(&turn));
        let (_, changed) = config_changed(&mut rx).await;
        assert!(changed.pending);
        assert_eq!(changed.config.effort.as_deref(), Some("high"));
        assert_eq!(changed.effective.effort.as_deref(), Some("medium"));

        backend.cancel(&session);
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome())
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            TurnOutcome::Cancelled | TurnOutcome::Completed(_)
        ));
        let mut ended = false;
        let changed = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match envelope.update {
                SessionUpdate::TurnEnded { .. } => ended = true,
                SessionUpdate::ConfigChanged(changed) => break changed,
                _ => {}
            }
        };
        assert!(ended, "the change is announced after the turn ends");
        assert!(!changed.pending);
        assert_eq!(changed.revision, result.revision);
        assert_eq!(changed.effective.effort.as_deref(), Some("high"));
        // The deferred write reached the tree once the turn ended.
        let entries = harness.read_session_entries().await.unwrap();
        assert!(entries.iter().any(|e| matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: AgentThinkingLevel::High,
                ..
            }
        )));
    })
    .await
    .expect("test timed out");
}
