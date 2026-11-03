use super::*;
use loop_agent::harness::{
    create_session_repository, AgentHarness, AgentHarnessOptions, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_ai::providers::{faux_provider, FauxScript};
use std::path::Path;
pub(crate) async fn fixture(dir: &Path) -> SurfaceClient {
    let models = Arc::new(loop_ai::Models::new());
    let provider = faux_provider(FauxScript::new());
    let mut model = provider.get_model("faux-model").unwrap();
    model.reasoning = true;
    let mut second = model.clone();
    second.id = "second".into();
    provider.set_models(vec![model.clone(), second]);
    models.set_provider(provider);
    let store: Arc<dyn loop_agent::harness::session::SessionStore> = Arc::new(
        loop_agent::harness::session::sqlite::SqliteSessionStore::open(dir.join("sessions.db"))
            .unwrap()
            .standalone_writer(),
    );
    let session = create_session_repository(store.clone(), None)
        .create(Some(dir.display().to_string()), None)
        .await
        .unwrap();
    let session_id = session.metadata().id.clone();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models: models.clone(),
        model: Some(model.clone()),
        session,
        host_env: Arc::new(HostExecutionEnv::new(dir)),
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    let native = NativeBackend::new(harness.clone()).await;
    let inner = loop_app_core::Runtime {
        agent_dir: dir.into(),
        cwd: dir.into(),
        custom_system_prompt: None,
        append_system_prompt: None,
        context_files: vec![],
        settings: Default::default(),
        models,
        credentials: Arc::new(
            crate::config::auth::FileCredentialStore::open(dir.join("auth.json")).unwrap(),
        ),
        harness,
        backend: native.clone(),
        native,
        agents: Arc::new(loop_app_core::agents::AgentManager::new(
            vec![],
            dir.into(),
            dir,
        )),
        active_agent: None,
        agent_model: None,
        agent_effort: None,
        theme: Theme::dark(),
        resources: Default::default(),
        project_trusted: false,
        trust: crate::config::trust::TrustStore::load(dir.join("trust.json")).unwrap(),
        sessions_db: dir.join("sessions.db"),
        session_id,
        resumed: false,
        needs_provider_setup: false,
        selected_model: Some(model),
        model_note: None,
        tool_approval: None,
        tool_gate: loop_app_core::tool_gate::ToolGate::new(Default::default()),
        startup_notices: vec![],
        mcp_client: Arc::new(loop_mcp::McpClientManager::new()),
        active_skills: vec![],
        web_disabled_for_run: true,
        #[cfg(feature = "telemetry")]
        telemetry: None,
        session_store: Some(store),
    };
    crate::CliRuntime {
        inner,
        keybindings: Default::default(),
        debug: false,
        debug_log_path: None,
        pending_agent: None,
        starting_agent: None,
        signing_in: None,
        forwarded: Default::default(),
        reasoning_notices: Default::default(),
        confirm_full_access: false,
        confirm_export: None,
        startup_sessions: None,
        usage_requested: false,
    }
    .into()
}

pub(crate) fn pause_provider(
    client: &SurfaceClient,
    started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
) {
    let provider = loop_ai::providers::faux_provider_fn(Arc::new(move |_| {
        let (started, finish) = (started.clone(), finish.clone());
        Box::pin(async move {
            started.notify_one();
            finish.notified().await;
            loop_ai::providers::FauxResponse::Text("finished".into())
        })
    }));
    provider.set_models(client.host.inner.models.get_models(Some("faux")));
    client.host.inner.models.set_provider(provider);
}
