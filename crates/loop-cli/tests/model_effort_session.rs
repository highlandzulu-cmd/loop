//! `/model` and `/effort` in the terminal app: the Session records its
//! choice, the TUI still saves it as the default for new Sessions, and
//! resuming restores the Session's own choice, including an explicit `off`,
//! without saving it as the default (separate test binary: it mutates
//! process env).

use loop_cli::config::paths::{settings_path, ENV_AGENT_DIR, ENV_SESSION_DIR};
use loop_cli::{bootstrap_cli, BootstrapOpts, CliRuntime};
use loop_test_support::{FakeHttpServer, FakeResponse};

const MODELS: &str = r#"{"data":[{"id":"m-one","supported_parameters":["tools","reasoning"]},{"id":"m-two","supported_parameters":["tools","reasoning"]}]}"#;

async fn boot(cwd: &std::path::Path, session_id: Option<String>) -> CliRuntime {
    bootstrap_cli(BootstrapOpts {
        cwd: cwd.to_path_buf(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        no_web: true,
        permission_mode: None,
        interactive: false,
        session_id,
        agent: None,
    })
    .await
    .unwrap()
}

fn saved(agent_dir: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(settings_path(agent_dir)).unwrap()).unwrap()
}

fn set_default_effort(agent_dir: &std::path::Path, level: &str) {
    let mut settings = saved(agent_dir);
    settings["defaultThinkingLevel"] = level.into();
    std::fs::write(settings_path(agent_dir), settings.to_string()).unwrap();
}

#[tokio::test]
async fn session_choices_are_recorded_saved_as_defaults_and_restored() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let agent_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let server = FakeHttpServer::start(|req| match req.path.as_str() {
            "/v1/models" => FakeResponse::json(MODELS),
            _ => FakeResponse::new(404, "text/plain", "nope"),
        });
        std::fs::write(
            settings_path(agent_dir.path()),
            format!(
                r#"{{"providers":[{{"id":"gw","name":"Gateway","baseUrl":"{}/v1"}}]}}"#,
                server.base_url()
            ),
        )
        .unwrap();
        // SAFETY: only test in this binary; no concurrent env access.
        unsafe {
            std::env::set_var(ENV_AGENT_DIR, agent_dir.path());
            std::env::set_var(ENV_SESSION_DIR, agent_dir.path().join("sessions"));
        }

        let mut runtime = boot(cwd.path(), None).await;
        runtime.select_model("gw", "m-one").await.unwrap();
        // Today's TUI behaviour: a choice is also the default for new Sessions.
        assert_eq!(saved(agent_dir.path())["defaultModel"], "m-one");
        assert_eq!(runtime.set_effort("high").await.unwrap(), "high");
        assert_eq!(saved(agent_dir.path())["defaultThinkingLevel"], "high");
        assert_eq!(runtime.native_effort(), "high");
        assert_eq!(runtime.efforts().await.unwrap().1, "high");
        assert_eq!(runtime.cycle_effort().await.unwrap(), "off");
        assert_eq!(runtime.native_effort(), "off");
        assert_eq!(saved(agent_dir.path())["defaultThinkingLevel"], "off");
        let off_session = runtime.session_id.clone();

        runtime.new_session().await.unwrap();
        runtime.set_effort("low").await.unwrap();
        let low_session = runtime.session_id.clone();
        drop(runtime);

        // The default changes elsewhere; each Session keeps its own choice.
        set_default_effort(agent_dir.path(), "medium");
        let mut runtime = boot(cwd.path(), Some(off_session.clone())).await;
        assert_eq!(
            runtime.native_effort(),
            "off",
            "an explicit off is restored"
        );
        assert_eq!(runtime.efforts().await.unwrap().1, "off");
        assert_eq!(runtime.selected_model_spec().as_deref(), Some("gw/m-one"));

        // Switching Sessions restores each one's effort.
        runtime.open_native_session(&low_session).await.unwrap();
        assert_eq!(runtime.native_effort(), "low");
        runtime.open_native_session(&off_session).await.unwrap();
        assert_eq!(runtime.native_effort(), "off");

        // Restoring a Session never saves its choice as the default.
        runtime.save_settings().unwrap();
        assert_eq!(saved(agent_dir.path())["defaultThinkingLevel"], "medium");
        drop(runtime);
        let runtime = boot(cwd.path(), Some(low_session)).await;
        assert_eq!(runtime.native_effort(), "low");
        runtime.save_settings().unwrap();
        assert_eq!(saved(agent_dir.path())["defaultThinkingLevel"], "medium");

        // A new Session without a choice of its own uses the default.
        drop(runtime);
        let runtime = boot(cwd.path(), None).await;
        assert_eq!(runtime.native_effort(), "medium");
    })
    .await
    .expect("test timed out");
}
