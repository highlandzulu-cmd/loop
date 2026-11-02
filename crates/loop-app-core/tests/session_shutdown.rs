//! `SessionShutdown` releases a standalone Surface's leases only once its
//! turn, with the tools, hooks and writes it runs, has finished.
#![cfg(unix)]

use loop_agent::harness::{
    create_session_repository,
    session::{sqlite::SqliteSessionStore, SessionStore},
    AgentHarness, AgentHarnessOptions, AgentHarnessResources, HostExecutionEnv, SandboxMode,
};
use loop_ai::{
    providers::{faux_provider, FauxResponse, FauxScript},
    Models, ToolCall,
};
use loop_app_core::runtime::SessionShutdown;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

/// A tool that ignores cancellation and finishes only when told.
struct Stubborn {
    started: Arc<Notify>,
    finish: Arc<Notify>,
    finished: Arc<AtomicBool>,
}

fn stubborn() -> (loop_agent::AgentTool, Stubborn) {
    let parts = Stubborn {
        started: Arc::new(Notify::new()),
        finish: Arc::new(Notify::new()),
        finished: Arc::new(AtomicBool::new(false)),
    };
    let (started, finish, finished) = (
        parts.started.clone(),
        parts.finish.clone(),
        parts.finished.clone(),
    );
    let tool = loop_agent::AgentTool::simple(
        "slow",
        "slow",
        "takes a while",
        json!({"type": "object", "properties": {}}),
        move |_, _, _, _| {
            let (started, finish, finished) = (started.clone(), finish.clone(), finished.clone());
            async move {
                started.notify_one();
                finish.notified().await;
                finished.store(true, Ordering::SeqCst);
                Ok(loop_agent::AgentToolResult::text("done"))
            }
        },
    );
    (tool, parts)
}

async fn setup(
    dir: &std::path::Path,
) -> (
    Arc<AgentHarness>,
    Arc<dyn SessionStore>,
    SqliteSessionStore,
    String,
    Stubborn,
) {
    let path = dir.join("sessions.db");
    let base = SqliteSessionStore::open(&path).unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(base.standalone_writer());
    let repo = create_session_repository(Arc::clone(&store), None);
    let session = repo.create(None, None).await.unwrap();
    let id = session.metadata().id.clone();
    let script = FauxScript::new();
    script.push(FauxResponse::ToolCalls(vec![ToolCall {
        id: "c1".into(),
        name: "slow".into(),
        arguments: json!({}),
        thought_signature: None,
    }]));
    script.push(FauxResponse::Text("after".into()));
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let (tool, parts) = stubborn();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        model: models.get_model("faux", "faux-model"),
        models,
        session,
        host_env: Arc::new(HostExecutionEnv::new(dir)),
        tools: vec![tool],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: AgentHarnessResources::default(),
    }));
    (harness, store, base, id, parts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leases_stay_while_a_turn_ignores_cancellation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (harness, store, base, id, tool) = setup(dir.path()).await;
        let turn = tokio::spawn({
            let harness = harness.clone();
            async move { harness.prompt_with_images("go", vec![]).await }
        });
        tool.started.notified().await;
        let shutdown = SessionShutdown::new(harness.clone(), Some(store))
            .with_grace(Duration::from_millis(200));
        let error = shutdown.run().await.unwrap_err().to_string();
        assert!(error.contains("did not stop"), "{error}");
        assert!(
            base.lease(&id).await.unwrap().is_some(),
            "released while the turn was still running"
        );
        // Once the turn has really finished, shutdown completes.
        tool.finish.notify_one();
        let _ = turn.await.unwrap();
        shutdown.run().await.unwrap();
        assert!(base.lease(&id).await.unwrap().is_none());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_only_after_the_turn_has_finished() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let (harness, store, base, id, tool) = setup(dir.path()).await;
        let turn = tokio::spawn({
            let harness = harness.clone();
            async move { harness.prompt_with_images("go", vec![]).await }
        });
        tool.started.notified().await;
        let finish = tool.finish.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            finish.notify_one();
        });
        SessionShutdown::new(harness.clone(), Some(store))
            .run()
            .await
            .unwrap();
        assert!(
            tool.finished.load(Ordering::SeqCst),
            "released before the tool finished"
        );
        assert!(base.lease(&id).await.unwrap().is_none());
        let _ = turn.await.unwrap();
    })
    .await
    .unwrap();
}
