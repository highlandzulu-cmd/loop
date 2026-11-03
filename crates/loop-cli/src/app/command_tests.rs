//! Commands and keys through the same handlers as the terminal loop.
use super::*;
use crate::surface_client::test_support::fixture;

#[derive(Default)]
struct Driver {
    chat: Vec<ChatItem>,
    status: String,
    step: String,
    setup: Option<SetupPrompt>,
    model: Option<ModelPickerState>,
    fork: Option<ForkPickerState>,
    skill: Option<SkillPickerState>,
    mcp: Option<McpPickerState>,
    session: Option<SessionPickerState>,
    tree: Option<TreePickerState>,
    hide: bool,
    working: bool,
    queue: VecDeque<QueuedMessage>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    redraw: bool,
    purge: bool,
    input: InputBuffer,
}
impl Driver {
    async fn effect(&mut self, runtime: &mut SurfaceClient, effect: CommandEffect) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut bar = TokenBarState::from_model(runtime);
        apply_effect(
            effect,
            runtime,
            &mut self.chat,
            &mut self.status,
            &mut self.step,
            &mut self.setup,
            &mut self.model,
            &mut self.fork,
            &mut self.skill,
            &mut self.mcp,
            &mut self.session,
            &mut self.tree,
            &mut self.hide,
            &mut self.working,
            &mut self.queue,
            &mut self.assistant,
            &mut self.thinking,
            &mut self.redraw,
            &mut self.purge,
            &mut self.input,
            &tx,
            &mut bar,
            &mut false,
        )
        .await
        .unwrap();
    }
    fn text(&self) -> String {
        self.chat
            .iter()
            .filter_map(|item| match item {
                ChatItem::System { text } | ChatItem::Error { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tokio::test]
async fn model_command_changes_this_session_and_offers_to_save_defaults() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(
            &mut runtime,
            CommandEffect::SelectModel(Some("faux/second".into())),
        )
        .await;
        assert!(ui.text().contains("model → faux/second"), "{}", ui.text());
        assert!(ui.text().contains(SAVE_DEFAULT_HINT), "{}", ui.text());
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        assert!(runtime.legacy().settings().selected_model().is_none());
        assert!(!crate::config::paths::settings_path(dir.path()).exists());
        ui.effect(&mut runtime, CommandEffect::NewSession).await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/faux-model")
        );
    })
    .await
    .unwrap();
}

impl Driver {
    async fn key(&mut self, runtime: &mut SurfaceClient, code: KeyCode, modifiers: KeyModifiers) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut bar = TokenBarState::from_model(runtime);
        handle_key(
            KeyEvent::new(code, modifiers),
            runtime,
            &mut self.input,
            &mut CommandHistory::new(),
            &mut self.chat,
            &mut self.status,
            &mut self.step,
            &mut 0,
            &mut Instant::now(),
            &mut Instant::now(),
            &mut self.assistant,
            &mut self.thinking,
            &mut false,
            &mut self.redraw,
            &mut self.purge,
            &mut self.hide,
            &mut self.setup,
            &mut self.model,
            &mut self.fork,
            &mut self.skill,
            &mut self.mcp,
            &mut self.session,
            &mut self.tree,
            &mut Attachments::default(),
            &mut None,
            &mut self.working,
            &mut self.queue,
            &mut false,
            &[],
            &[],
            None,
            &mut 0,
            &tx,
            &mut bar,
            &mut false,
        )
        .await
        .unwrap();
    }
}

async fn prompt(runtime: &SurfaceClient, text: &str) {
    let turn = runtime
        .backend()
        .prompt(&runtime.session_ref(), vec![ContentBlock::text(text)])
        .await
        .unwrap();
    assert!(matches!(
        turn.outcome().await,
        TurnOutcome::Completed { .. }
    ));
}

#[tokio::test]
async fn model_picker_and_cycle_keys_share_the_session_selection() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::SelectModel(None))
            .await;
        assert_eq!(
            ui.model.as_ref().unwrap().all,
            ["faux/faux-model", "faux/second"]
        );
        ui.key(&mut runtime, KeyCode::Esc, KeyModifiers::NONE).await;
        ui.key(&mut runtime, KeyCode::Char('l'), KeyModifiers::CONTROL)
            .await;
        ui.key(&mut runtime, KeyCode::Down, KeyModifiers::NONE)
            .await;
        ui.key(&mut runtime, KeyCode::Enter, KeyModifiers::NONE)
            .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        ui.key(&mut runtime, KeyCode::Char('p'), KeyModifiers::CONTROL)
            .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/faux-model")
        );
        ui.key(
            &mut runtime,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
        .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        assert_eq!(ui.text().matches(SAVE_DEFAULT_HINT).count(), 3);
        assert!(runtime.legacy().settings().selected_model().is_none());
        let before = runtime.selected_model_spec();
        ui.effect(
            &mut runtime,
            CommandEffect::SelectModel(Some("missing".into())),
        )
        .await;
        assert_eq!(runtime.selected_model_spec(), before);
        assert!(ui.text().contains("unknown model"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn effort_command_cycle_and_status_keep_defaults_separate() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let default = runtime.legacy().settings().default_thinking_level.clone();
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Effort(None)).await;
        assert!(ui.text().contains("reasoning effort:"));
        ui.effect(&mut runtime, CommandEffect::Effort(Some("high".into())))
            .await;
        assert_eq!(runtime.native_effort(), "high");
        assert!(
            ui.status.ends_with("faux/faux-model · high"),
            "{}",
            ui.status
        );
        ui.key(&mut runtime, KeyCode::BackTab, KeyModifiers::SHIFT)
            .await;
        assert_eq!(runtime.native_effort(), "off");
        assert!(ui.status.ends_with("faux/faux-model · off"));
        assert_eq!(ui.text().matches(SAVE_DEFAULT_HINT).count(), 2);
        assert_eq!(runtime.legacy().settings().default_thinking_level, default);
        ui.effect(&mut runtime, CommandEffect::Effort(Some("invalid".into())))
            .await;
        assert!(ui.text().contains("unknown effort"));
        assert_eq!(runtime.native_effort(), "off");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resume_picker_name_new_and_tree_keep_the_transcript_and_draft() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let first = runtime.session_id().clone();
        prompt(&runtime, "first prompt").await;
        let mut ui = Driver::default();
        ui.effect(
            &mut runtime,
            CommandEffect::SetName("  named session  ".into()),
        )
        .await;
        assert!(ui.text().contains("session name → named session"));
        ui.effect(&mut runtime, CommandEffect::NewSession).await;
        assert_ne!(*runtime.session_id(), first);
        assert!(ui
            .chat
            .iter()
            .all(|item| !matches!(item, ChatItem::User { .. })));
        ui.effect(&mut runtime, CommandEffect::Resume(None)).await;
        assert_eq!(
            ui.session.as_ref().unwrap().choices[0].title,
            "named session"
        );
        ui.key(&mut runtime, KeyCode::Enter, KeyModifiers::NONE)
            .await;
        assert_eq!(*runtime.session_id(), first);
        assert!(ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "first prompt")));
        ui.effect(&mut runtime, CommandEffect::Tree).await;
        assert_eq!(ui.tree.as_ref().unwrap().choices.len(), 2);
        ui.key(&mut runtime, KeyCode::Up, KeyModifiers::NONE).await;
        ui.key(&mut runtime, KeyCode::Enter, KeyModifiers::NONE)
            .await;
        assert_eq!(ui.input.as_str(), "first prompt");
        assert!(!ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User { .. })));
        assert!(ui.redraw && ui.purge);
        ui.effect(&mut runtime, CommandEffect::Resume(Some("missing".into())))
            .await;
        assert!(ui.text().contains("no session `missing`"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn fork_and_clone_adopt_destinations_and_preserve_the_source() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        prompt(&runtime, "first prompt").await;
        prompt(&runtime, "second prompt").await;
        let source = runtime.session_id().clone();
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Fork).await;
        assert_eq!(
            ui.fork.as_ref().unwrap().selected_point().unwrap().text,
            "second prompt"
        );
        ui.key(&mut runtime, KeyCode::Enter, KeyModifiers::NONE)
            .await;
        let fork = runtime.session_id().clone();
        assert_ne!(fork, source);
        assert_eq!(ui.input.as_str(), "second prompt");
        assert!(ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "first prompt")));
        assert!(!ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "second prompt")));
        ui.effect(&mut runtime, CommandEffect::Resume(Some(source.clone())))
            .await;
        ui.effect(&mut runtime, CommandEffect::CloneSession).await;
        assert_ne!(*runtime.session_id(), source);
        assert_ne!(*runtime.session_id(), fork);
        assert!(ui.input.as_str().is_empty());
        assert!(ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "second prompt")));
        ui.effect(&mut runtime, CommandEffect::Resume(Some(source)))
            .await;
        assert!(ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "second prompt")));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn export_confirmation_and_import_use_surface_local_files() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        prompt(&runtime, "exported prompt").await;
        let source = runtime.session_id().clone();
        let mut ui = Driver::default();
        ui.effect(
            &mut runtime,
            CommandEffect::Export(Some("nested/transcript".into())),
        )
        .await;
        let markdown = dir.path().join("nested/transcript.md");
        let jsonl = dir.path().join("nested/transcript.loop.jsonl");
        assert!(std::fs::read_to_string(&markdown)
            .unwrap()
            .contains("exported prompt"));
        std::fs::write(&markdown, "keep me").unwrap();
        ui.effect(
            &mut runtime,
            CommandEffect::Export(Some("nested/transcript".into())),
        )
        .await;
        assert_eq!(std::fs::read_to_string(&markdown).unwrap(), "keep me");
        assert!(ui.text().contains("Nothing was written."));
        ui.effect(
            &mut runtime,
            CommandEffect::Export(Some("nested/transcript".into())),
        )
        .await;
        assert!(std::fs::read_to_string(&markdown)
            .unwrap()
            .contains("exported prompt"));
        ui.effect(
            &mut runtime,
            CommandEffect::Import(jsonl.display().to_string()),
        )
        .await;
        assert_ne!(*runtime.session_id(), source);
        assert!(ui
            .chat
            .iter()
            .any(|item| matches!(item, ChatItem::User {text} if text == "exported prompt")));
        std::fs::write(&jsonl, "invalid").unwrap();
        let before = runtime.session_id().clone();
        ui.effect(
            &mut runtime,
            CommandEffect::Import(jsonl.display().to_string()),
        )
        .await;
        assert_eq!(*runtime.session_id(), before);
        assert!(ui.text().contains("could not import"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clone_keeps_source_approval_grants_out_of_the_destination() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        prompt(&runtime, "source prompt").await;
        runtime
            .legacy()
            .harness()
            .append_label(format!(
                "{}bash",
                loop_agent::harness::session::TOOL_APPROVAL_LABEL_NAMESPACE
            ))
            .await
            .unwrap();
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::CloneSession).await;
        ui.effect(&mut runtime, CommandEffect::Export(Some("clone".into())))
            .await;
        let jsonl = std::fs::read_to_string(dir.path().join("clone.loop.jsonl")).unwrap();
        assert!(
            !jsonl.contains(loop_agent::harness::session::TOOL_APPROVAL_LABEL_NAMESPACE),
            "{jsonl}"
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resuming_restores_session_model_and_explicit_off_without_saving_defaults() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(
            &mut runtime,
            CommandEffect::SelectModel(Some("second".into())),
        )
        .await;
        ui.effect(&mut runtime, CommandEffect::Effort(Some("off".into())))
            .await;
        prompt(&runtime, "off session").await;
        let off = runtime.session_id().clone();
        ui.effect(&mut runtime, CommandEffect::NewSession).await;
        ui.effect(&mut runtime, CommandEffect::Effort(Some("high".into())))
            .await;
        prompt(&runtime, "high session").await;
        let high = runtime.session_id().clone();
        ui.effect(&mut runtime, CommandEffect::Resume(Some(off)))
            .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        assert_eq!(runtime.native_effort(), "off");
        ui.effect(&mut runtime, CommandEffect::Resume(Some(high)))
            .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/faux-model")
        );
        assert_eq!(runtime.native_effort(), "high");
        assert!(!crate::config::paths::settings_path(dir.path()).exists());
        runtime.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clone_during_a_native_turn_keeps_the_source_running_without_an_orphan_copy() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        prompt(&runtime, "committed").await;
        let started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        crate::surface_client::test_support::pause_provider(
            &runtime,
            started.clone(),
            finish.clone(),
        );
        let session = runtime.session_ref();
        let turn = runtime
            .backend()
            .prompt(&session, vec![ContentBlock::text("working")])
            .await
            .unwrap();
        started.notified().await;
        let before = runtime.list_sessions().await.unwrap().len();
        let mut ui = Driver {
            working: true,
            status: "Working…".into(),
            ..Default::default()
        };
        ui.effect(&mut runtime, CommandEffect::CloneSession).await;
        assert_eq!(runtime.session_ref(), session);
        assert!(runtime.backend().is_busy(&session));
        assert_eq!(runtime.list_sessions().await.unwrap().len(), before);
        assert!(ui.working);
        assert_eq!(ui.status, "Working…");
        finish.notify_one();
        assert!(matches!(
            turn.outcome().await,
            TurnOutcome::Completed { .. }
        ));
        runtime.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn model_cycle_respects_enabled_models_without_hiding_picker_choices() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        runtime.legacy_mut().settings_mut().enabled_models = vec!["faux/second".into()];
        let mut ui = Driver::default();
        ui.key(&mut runtime, KeyCode::Char('p'), KeyModifiers::CONTROL)
            .await;
        ui.key(&mut runtime, KeyCode::Char('p'), KeyModifiers::CONTROL)
            .await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        ui.effect(&mut runtime, CommandEffect::SelectModel(None))
            .await;
        assert_eq!(ui.model.as_ref().unwrap().all.len(), 2);
        runtime.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

fn hints(ui: &Driver) -> usize {
    ui.text().matches(SAVE_DEFAULT_HINT).count()
}

#[tokio::test]
async fn default_command_saves_the_sessions_model_and_effort_for_new_sessions() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        assert_eq!(SAVE_DEFAULT_HINT, "/default saves this for new Sessions");
        let mut ui = Driver::default();
        ui.effect(
            &mut runtime,
            CommandEffect::SelectModel(Some("faux/second".into())),
        )
        .await;
        assert_eq!(hints(&ui), 1, "{}", ui.text());
        ui.effect(&mut runtime, CommandEffect::Effort(Some("high".into())))
            .await;
        assert_eq!(hints(&ui), 2, "{}", ui.text());
        ui.effect(&mut runtime, CommandEffect::SaveDefaults).await;
        assert!(
            ui.text()
                .contains("defaults for new Sessions → faux/second · high"),
            "{}",
            ui.text()
        );
        let saved: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::config::paths::settings_path(dir.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["defaultProvider"], "faux");
        assert_eq!(saved["defaultModel"], "second");
        assert_eq!(saved["defaultThinkingLevel"], "high");
        // Choosing what is already the default offers nothing to save.
        ui.effect(&mut runtime, CommandEffect::Effort(Some("high".into())))
            .await;
        assert_eq!(hints(&ui), 2, "{}", ui.text());
        ui.effect(&mut runtime, CommandEffect::NewSession).await;
        assert_eq!(
            runtime.selected_model_spec().as_deref(),
            Some("faux/second")
        );
        assert_eq!(runtime.native_effort(), "high");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn each_change_offers_the_default_hint_once() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Effort(Some("high".into())))
            .await;
        assert_eq!(hints(&ui), 1, "{}", ui.text());
        ui.key(&mut runtime, KeyCode::BackTab, KeyModifiers::SHIFT)
            .await;
        assert_eq!(hints(&ui), 2, "{}", ui.text());
        ui.key(&mut runtime, KeyCode::Char('p'), KeyModifiers::CONTROL)
            .await;
        assert_eq!(hints(&ui), 3, "{}", ui.text());
        // A new Session (its transcript starts empty) is on the defaults:
        // nothing to save.
        ui.effect(&mut runtime, CommandEffect::NewSession).await;
        assert_eq!(hints(&ui), 0, "{}", ui.text());
        // A subscription agent the user did not choose (startup, resume)
        // never offers it.
        assert!(!agent_ready_offers_default(false, false));
        assert!(!agent_ready_offers_default(true, true));
        assert!(agent_ready_offers_default(true, false));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_unknown_effort_names_the_choices() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let error = runtime.set_effort("extreme").await.unwrap_err().to_string();
        assert!(
            error.starts_with("unknown effort `extreme`; choose one of: off"),
            "{error}"
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_logged_out_provider_cannot_be_selected() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        runtime.legacy().credentials().logout("faux").unwrap();
        let before = runtime.selected_model_spec();
        let error = runtime
            .select_model("faux", "second")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "faux is logged out; run /login faux");
        assert_eq!(runtime.selected_model_spec(), before);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_model_picker_lists_connected_providers_in_picker_order() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let runtime = fixture(dir.path()).await;
        let expected = crate::config::providers::sort_models_for_picker(
            runtime.catalog().get_available().await,
        );
        let listed = runtime.available_models().await.unwrap();
        let ids = |models: &[loop_ai::Model]| -> Vec<String> {
            models
                .iter()
                .map(|m| format!("{}/{}", m.provider, m.id))
                .collect()
        };
        assert_eq!(ids(&listed), ids(&expected));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn opening_a_session_during_a_turn_is_refused_without_cancelling_it() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        prompt(&runtime, "first").await;
        let first = runtime.session_id().clone();
        runtime.new_session().await.unwrap();
        let second = runtime.session_ref();
        let started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        crate::surface_client::test_support::pause_provider(
            &runtime,
            started.clone(),
            finish.clone(),
        );
        let turn = runtime
            .backend()
            .prompt(&second, vec![ContentBlock::text("running")])
            .await
            .unwrap();
        started.notified().await;
        let error = runtime
            .open_native_session(&first)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already running"), "{error}");
        assert_eq!(runtime.session_ref(), second);
        assert!(runtime.backend().is_busy(&second));
        finish.notify_one();
        assert!(matches!(
            turn.outcome().await,
            TurnOutcome::Completed { .. }
        ));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn permissions_require_full_access_confirmation_and_review_keeps_migration_help() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Permissions(None))
            .await;
        assert!(ui.text().contains("permission mode: ask"));
        ui.effect(
            &mut runtime,
            CommandEffect::Permissions(Some("full-access".into())),
        )
        .await;
        assert!(ui.text().contains("again to confirm"));
        assert_eq!(runtime.legacy().permission_mode(), PermissionMode::Ask);
        ui.effect(
            &mut runtime,
            CommandEffect::Permissions(Some("full-access".into())),
        )
        .await;
        assert_eq!(
            runtime.legacy().permission_mode(),
            PermissionMode::FullAccess
        );
        ui.effect(&mut runtime, CommandEffect::SetFileReview(None))
            .await;
        assert!(ui.text().contains("session auto-approve: (none)"));
        ui.effect(
            &mut runtime,
            CommandEffect::SetFileReview(Some("off".into())),
        )
        .await;
        assert!(ui
            .text()
            .contains("/review no longer turns prompts on or off"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sandbox_status_and_invalid_flags_do_not_start_work() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::SetSandbox(String::new()))
            .await;
        assert!(ui.text().contains("use /sandbox status|off|local"));
        ui.effect(
            &mut runtime,
            CommandEffect::SetSandbox("local --unknown".into()),
        )
        .await;
        assert!(ui.text().contains("unknown"));
        assert!(!ui.working);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn mcp_empty_list_reload_and_missing_auth_keep_their_messages() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        for command in [McpCommand::List, McpCommand::Reload] {
            ui.effect(&mut runtime, CommandEffect::Mcp(command)).await;
            assert!(ui.text().contains("No MCP servers configured"));
        }
        for command in [
            McpCommand::Enable("missing".into()),
            McpCommand::Disable("missing".into()),
            McpCommand::Login("missing".into()),
            McpCommand::Logout("missing".into()),
        ] {
            ui.chat.clear();
            ui.effect(&mut runtime, CommandEffect::Mcp(command)).await;
            assert!(
                ui.text()
                    .contains("mcp server not found: missing (see /mcp list)"),
                "{}",
                ui.text()
            );
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn trust_command_persists_explicit_choices_and_rejects_unknown_answers() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Trust(Some("yes".into())))
            .await;
        assert!(ui.text().contains("project trust → true"));
        ui.effect(&mut runtime, CommandEffect::Trust(Some("no".into())))
            .await;
        assert!(ui.text().contains("project trust → false"));
        ui.effect(&mut runtime, CommandEffect::Trust(Some("maybe".into())))
            .await;
        assert!(ui.text().contains("usage: /trust [yes|no] (got maybe)"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn mcp_picker_toggles_a_configured_server_and_reports_connection_failure() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        runtime.legacy_mut().settings_mut().mcp_servers.insert(
            "broken".into(),
            serde_json::from_value(serde_json::json!({"command": "/nonexistent-loop-mcp-test"}))
                .unwrap(),
        );
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Mcp(McpCommand::Picker))
            .await;
        assert_eq!(ui.mcp.as_ref().unwrap().selected_name(), Some("broken"));
        ui.key(&mut runtime, KeyCode::Char(' '), KeyModifiers::NONE)
            .await;
        assert_eq!(runtime.mcp_enabled("broken"), Some(false));
        ui.key(&mut runtime, KeyCode::Enter, KeyModifiers::NONE)
            .await;
        assert_eq!(runtime.mcp_enabled("broken"), Some(true));
        assert!(
            ui.text().contains("enabled, but connecting failed"),
            "{}",
            ui.text()
        );
        ui.effect(
            &mut runtime,
            CommandEffect::Mcp(McpCommand::Login("broken".into())),
        )
        .await;
        assert!(ui
            .text()
            .contains("local command server and does not use OAuth"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn skills_picker_activation_arguments_and_disabling_preserve_the_composer() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        runtime
            .legacy_mut()
            .resources_mut()
            .skills
            .push(loop_agent::harness::types::Skill {
                name: "inspect".into(),
                description: "Inspect code".into(),
                body: "Inspect carefully".into(),
                path: dir.path().join("SKILL.md"),
                disable_model_invocation: false,
            });
        runtime.legacy().sync_harness_resources().await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Skills(SkillsCommand::List))
            .await;
        assert!(ui.text().contains("inspect"));
        ui.effect(&mut runtime, CommandEffect::Skills(SkillsCommand::Picker))
            .await;
        assert_eq!(ui.skill.as_ref().unwrap().selected_name(), Some("inspect"));
        ui.effect(
            &mut runtime,
            CommandEffect::Skill {
                name: "inspect".into(),
                args: "  this module  ".into(),
            },
        )
        .await;
        assert_eq!(ui.input.as_str(), "this module");
        assert!(ui.text().contains("skill `inspect` is active (inspect)"));
        assert!(!ui.working);
        ui.effect(
            &mut runtime,
            CommandEffect::Skills(SkillsCommand::Disable("inspect".into())),
        )
        .await;
        ui.effect(
            &mut runtime,
            CommandEffect::Skill {
                name: "inspect".into(),
                args: String::new(),
            },
        )
        .await;
        assert!(ui
            .text()
            .contains("skill `inspect` is disabled. Enable it with /skills enable inspect"));
        ui.effect(
            &mut runtime,
            CommandEffect::Skills(SkillsCommand::Enable("inspect".into())),
        )
        .await;
        assert!(ui.text().contains("skill `inspect` enabled"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_template_arguments_are_rendered_and_queued_while_busy() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        runtime.legacy_mut().resources_mut().prompts.push(
            loop_agent::harness::types::PromptTemplate {
                name: "inspect".into(),
                body: "Check $1 with $2".into(),
                path: dir.path().join("inspect.md"),
                argument_hint: Some("<file> <detail>".into()),
            },
        );
        let mut ui = Driver {
            working: true,
            ..Default::default()
        };
        ui.effect(
            &mut runtime,
            CommandEffect::Template {
                name: "inspect".into(),
                args: "'file with spaces' carefully".into(),
            },
        )
        .await;
        assert_eq!(ui.queue.len(), 1);
        assert_eq!(
            ui.queue.front().unwrap().prompt,
            "Check file with spaces with carefully"
        );
        assert!(dynamic_command_entries(&runtime)
            .iter()
            .any(|(name, hint)| name == "inspect" && hint == "<file> <detail> — prompt template"));
        ui.effect(
            &mut runtime,
            CommandEffect::Template {
                name: "missing".into(),
                args: String::new(),
            },
        )
        .await;
        assert!(ui.text().contains("template not found: missing"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_refreshes_resources_and_reports_completion() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
        std::fs::write(dir.path().join("prompts/s35-reload.md"), "Hello $1").unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        ui.effect(&mut runtime, CommandEffect::Reload).await;
        assert!(ui
            .text()
            .contains("reloaded models, skills, prompts, themes"));
        assert!(dynamic_command_entries(&runtime)
            .iter()
            .any(|(name, _)| name == "s35-reload"));
    })
    .await
    .unwrap();
}

#[cfg(feature = "orchestration")]
#[tokio::test]
async fn workflow_planning_holds_session_busy_and_cancellation_ends_its_stream() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let runtime = fixture(dir.path()).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        crate::surface_client::test_support::pause_provider(&runtime, started.clone(), finish);
        let mut updates = runtime.backend().subscribe();
        let request = loop_protocol::session_services::WorkflowParams {
            session: runtime.session_ref(),
            goal: "inspect".into(),
            concurrency: Some(1),
            request_id: "once".into(),
        };
        let first = runtime.sessions().workflow(request.clone()).await.unwrap();
        started.notified().await;
        assert!(runtime.backend().is_busy(&runtime.session_ref()));
        assert!(runtime
            .backend()
            .new_session(Default::default())
            .await
            .is_err());
        assert_eq!(runtime.sessions().workflow(request).await.unwrap(), first);
        runtime.backend().cancel(&runtime.session_ref());
        loop {
            let envelope = updates.recv().await.unwrap();
            if let SessionUpdate::WorkflowProgress(p) = envelope.update {
                if p.state == loop_protocol::session_services::WorkflowState::Ended {
                    assert_eq!(p.workflow_id, first.workflow_id);
                    assert!(p.error.is_some());
                    break;
                }
            }
        }
        runtime.shutdown().run().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_settings_and_usage_commands_keep_their_reports_and_local_state() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = fixture(dir.path()).await;
        let mut ui = Driver::default();
        prompt(&runtime, "hello").await;
        ui.effect(&mut runtime, CommandEffect::SessionInfo).await;
        assert!(ui.text().contains("Theme:"));
        assert!(ui.text().contains("faux/faux-model"));
        ui.effect(&mut runtime, CommandEffect::Settings).await;
        assert!(ui.text().contains("permissionMode: ask"));
        assert!(ui.text().contains("responseHeaderTimeoutMs:"));
        #[cfg(feature = "usage")]
        {
            ui.effect(&mut runtime, CommandEffect::Usage).await;
            assert!(runtime.usage_requested);
        }
    })
    .await
    .unwrap();
}
