//! Workspace admin on a shared Runtime: reload reaches idle Sessions at
//! once and busy ones when their turn ends; toggles write the global
//! settings; permission mode is recorded in the Session tree.
use super::*;
use crate::log::{EventLog, MemoryEventLog};
use loop_ai::{
    providers::{faux_provider_fn, FauxResponse},
    Context, Message, ToolCall,
};
use loop_protocol::admin::*;
use loop_wire::RuntimeAdmin;

const LIMIT: Duration = Duration::from_secs(60);

/// A prompt starting `park` asks for an approval nobody answers.
async fn runtime(dir: &std::path::Path) -> (Arc<Runtime>, Arc<MemoryEventLog>) {
    let models = Arc::new(loop_ai::Models::create(Default::default()));
    models.set_provider(faux_provider_fn(Arc::new(|context: &Context| {
        let answered = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant(_)));
        let response = if answered {
            FauxResponse::Text("finished".into())
        } else {
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "park".into(),
                name: "bash".into(),
                arguments: json!({"command": "echo parked"}),
                thought_signature: None,
            }])
        };
        Box::pin(std::future::ready(response))
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
    let rt = Runtime::new(globals, opts, log.clone(), RuntimeLimits::default())
        .await
        .unwrap()
        .with_close_grace(Duration::from_millis(500))
        .into_shared();
    (rt, log)
}

fn write_skill(dir: &std::path::Path, name: &str) {
    let skill = dir.join("agent").join("skills").join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Keeps {name}\n---\nBody\n"),
    )
    .unwrap();
}

fn workspace(dir: &std::path::Path) -> String {
    tree::workspace_id(&dir.canonicalize().unwrap())
}

/// Whether `session` is offered the test's `notes` skill.
async fn has_notes(rt: &Runtime, session: &SessionRef) -> bool {
    let live = rt.route(session).unwrap();
    let resources = live.opened.harness.get_resources().await;
    resources.skills.iter().any(|s| s.name == "notes")
}

/// Start a turn on `session` that waits on an unanswered Escalation.
async fn park(rt: &Runtime, log: &MemoryEventLog, session: &SessionRef) -> TurnHandle {
    let turn = rt
        .prompt(session, vec![ContentBlock::text("park")])
        .await
        .unwrap();
    loop {
        let raised = log.read_runtime_after(0).await.unwrap().iter().any(|e| {
            matches!(e.event.kind, EventKind::EscalationRaised(_))
                && e.event.session_key.as_ref() == Some(&session.key)
        });
        if raised {
            break turn;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn reload_reaches_idle_sessions_now_and_busy_ones_when_their_turn_ends() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path()).await;
        let idle = rt.new_session(SessionSpec::default()).await.unwrap();
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let turn = park(&rt, &log, &busy).await;

        write_skill(dir.path(), "notes");
        let reloaded = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(reloaded.revision, 1);
        assert_eq!(reloaded.pending_sessions, 1);
        assert!(has_notes(&rt, &idle).await);
        assert!(!has_notes(&rt, &busy).await, "applied mid-turn");

        rt.cancel(&busy);
        turn.outcome().await;
        assert!(has_notes(&rt, &busy).await);
        // New Sessions open with the reloaded workspace.
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(has_notes(&rt, &fresh).await);
        // A stale revision is refused with the current one.
        let stale = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: Some(0),
            })
            .await
            .unwrap_err();
        assert_eq!(stale, BackendError::Conflict { revision: 1 });
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn toggling_a_skill_saves_global_settings_and_reaches_sessions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        assert!(has_notes(&rt, &session).await);
        let id = workspace(dir.path());
        let result = rt
            .skills(SkillsParams {
                workspace_id: id.clone(),
                action: AdminAction::Disable,
                name: Some("notes".into()),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(result.revision, 1);
        assert_eq!(result.pending_sessions, 0);
        // The host's own global skills may be listed too.
        let notes: Vec<_> = result
            .skills
            .into_iter()
            .filter(|s| s.name == "notes")
            .collect();
        assert_eq!(
            notes,
            vec![SkillStatus {
                name: "notes".into(),
                description: "Keeps notes".into(),
                enabled: false,
            }]
        );
        assert!(!has_notes(&rt, &session).await);
        let saved = std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap();
        assert!(saved.contains("notes"), "{saved}");
        assert!(rt
            .skills(SkillsParams {
                workspace_id: id.clone(),
                action: AdminAction::Enable,
                name: Some("missing".into()),
                expected_revision: None,
            })
            .await
            .is_err());
        // An id the host did not issue is refused, never read as a path.
        let path = dir.path().to_string_lossy().into_owned();
        assert!(admin(&rt)
            .reload(ReloadParams {
                workspace_id: path,
                expected_revision: None,
            })
            .await
            .is_err());
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn templates_list_and_render_and_trust_reads_and_writes() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("agent/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("review.md"), "Review $1 carefully\n").unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let id = workspace(dir.path());
        let listed = rt
            .templates(TemplatesParams {
                workspace_id: id.clone(),
                name: None,
                args: vec![],
            })
            .await
            .unwrap();
        let names: Vec<_> = listed
            .templates
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["review".to_string()]);
        let rendered = rt
            .templates(TemplatesParams {
                workspace_id: id.clone(),
                name: Some("review".into()),
                args: vec!["main.rs".into()],
            })
            .await
            .unwrap();
        assert_eq!(
            rendered.text.as_deref().map(str::trim),
            Some("Review main.rs carefully")
        );

        let read = |trusted| TrustParams {
            workspace_id: id.clone(),
            trusted,
            expected_revision: None,
        };
        assert!(!rt.trust(read(None)).await.unwrap().trusted);
        let written = rt.trust(read(Some(true))).await.unwrap();
        assert!(written.trusted);
        assert_eq!(written.revision, 1);
        assert!(rt.trust(read(None)).await.unwrap().trusted);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn permission_mode_is_recorded_and_restored_on_reopen() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let params = |mode: Option<&str>, expected| PermissionModeParams {
            session: session.clone(),
            mode: mode.map(str::to_owned),
            expected_revision: expected,
        };
        let before = rt.permission_mode(params(None, None)).await.unwrap();
        assert_eq!(before.mode, "ask");
        let set = rt
            .permission_mode(params(Some("plan"), Some(before.revision)))
            .await
            .unwrap();
        assert_eq!(set.mode, "plan");
        assert_eq!(set.revision, before.revision + 1);
        assert!(set.seq.is_some());
        assert_eq!(
            rt.permission_mode(params(Some("ask"), Some(before.revision)))
                .await
                .unwrap_err(),
            BackendError::Conflict {
                revision: set.revision
            }
        );
        assert!(rt
            .permission_mode(params(Some("everything"), None))
            .await
            .is_err());

        rt.close(&session.key, false).await.unwrap();
        let reopened = rt
            .load_session(&session, SessionSpec::default())
            .await
            .unwrap();
        let after = rt
            .permission_mode(PermissionModeParams {
                session: reopened,
                mode: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(after.mode, "plan");
        assert_eq!(after.revision, set.revision);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sandbox_status_reports_off_and_a_bad_mode_is_refused() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let status = rt
            .sandbox(SandboxParams {
                session: session.clone(),
                action: SandboxAction::Status,
                config: None,
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(status.config.mode, "off");
        assert!(!status.info.is_empty());
        let refused = rt
            .sandbox(SandboxParams {
                session: session.clone(),
                action: SandboxAction::Set,
                config: Some(SandboxChoice {
                    mode: "remote".into(),
                    ..SandboxChoice::default()
                }),
                expected_revision: None,
            })
            .await;
        assert!(refused.is_err());
        // Turning an already-off sandbox off rebuilds its tools and stays off.
        let off = rt
            .sandbox(SandboxParams {
                session,
                action: SandboxAction::Set,
                config: Some(SandboxChoice {
                    mode: "off".into(),
                    ..SandboxChoice::default()
                }),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(off.config.mode, "off");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

/// The admin service, past `Runtime`'s own `reload`.
fn admin(rt: &Runtime) -> &dyn RuntimeAdmin {
    rt
}
