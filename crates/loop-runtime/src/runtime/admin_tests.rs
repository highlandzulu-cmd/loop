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

        // The Runtime panel sees one idle and one parked Session.
        let panel = admin(&rt).sessions().await.unwrap();
        assert_eq!(panel.totals.sessions, 2);
        assert_eq!(panel.totals.busy, 1);
        let state = |key: &SessionKey| {
            panel
                .sessions
                .iter()
                .find(|row| &row.session.key == key)
                .map(|row| row.state.clone())
        };
        assert_eq!(state(&idle.key).as_deref(), Some("idle"));
        assert_eq!(state(&busy.key).as_deref(), Some("parked"));
        assert_eq!(panel.sessions[0].workspace_id, workspace(dir.path()));

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
        // Saved for this workspace on the host, not in the global defaults.
        let saved =
            std::fs::read_to_string(dir.path().join("agent/workspace-settings.json")).unwrap();
        assert!(saved.contains("notes"), "{saved}");
        let global =
            std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap_or_default();
        assert!(!global.contains("notes"), "{global}");
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

async fn tools_and_prompt(rt: &Runtime, session: &SessionRef) -> (Vec<String>, String) {
    let live = rt.route(session).unwrap();
    let mut names: Vec<String> = live
        .opened
        .harness
        .get_tools()
        .await
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    (names, live.opened.harness.system_prompt().await)
}

#[tokio::test]
async fn a_reloaded_session_matches_a_fresh_one_and_keeps_runtime_tools() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let before = tools_and_prompt(&rt, &session).await;
        assert!(
            before.0.iter().any(|n| n == subagent::TOOL),
            "{:?}",
            before.0
        );
        let snapshot = rt.snapshot(&session).await.unwrap();
        assert_eq!(snapshot.workspace_revision, 0);
        admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
            .await
            .unwrap();
        assert_eq!(tools_and_prompt(&rt, &session).await, before);
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        assert_eq!(tools_and_prompt(&rt, &fresh).await, before);
        // Snapshots report the workspace revision admin results use.
        assert_eq!(rt.snapshot(&session).await.unwrap().workspace_revision, 1);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

fn generation(rt: &Runtime, session: &SessionRef) -> Arc<admin::Generation> {
    let live = rt.route(session).unwrap();
    let held = live.generation.lock().unwrap().clone();
    held.expect("a published Session holds its generation")
}

#[tokio::test]
async fn a_second_reload_keeps_the_generation_a_busy_session_still_uses() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = runtime(dir.path()).await;
        let busy = rt.new_session(SessionSpec::default()).await.unwrap();
        let in_use = generation(&rt, &busy);
        let in_use_closed = Arc::clone(&in_use.released);
        drop(in_use);
        let turn = park(&rt, &log, &busy).await;
        let reload = || {
            admin(&rt).reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
        };
        reload().await.unwrap();
        let skipped_closed = {
            let live = rt.route(&busy).unwrap();
            let pending = live.pending_workspace.lock().unwrap();
            Arc::clone(&pending.as_ref().unwrap().released)
        };
        reload().await.unwrap();
        // The generation it was only going to switch to is closed; the one
        // its running turn uses keeps its connections.
        assert!(skipped_closed.load(Ordering::Acquire));
        assert!(!in_use_closed.load(Ordering::Acquire));

        rt.cancel(&busy);
        turn.outcome().await;
        assert!(in_use_closed.load(Ordering::Acquire));
        assert!(!generation(&rt, &busy).released.load(Ordering::Acquire));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_session_opening_across_a_reload_adopts_the_new_workspace() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".agents/skills/proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("SKILL.md"),
            "---\nname: proj\ndescription: Project skill\n---\nBody\n",
        )
        .unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let trust = |trusted| TrustParams {
            workspace_id: workspace(dir.path()),
            trusted: Some(trusted),
            expected_revision: None,
        };
        // Loads the workspace, trusted, with the project skill.
        let first = rt.new_session(SessionSpec::default()).await.unwrap();
        admin(&rt).trust(trust(true)).await.unwrap();
        let offered = |names: Vec<String>| names.iter().any(|n| n == "proj");
        let skills_of = |session: SessionRef| {
            let rt = rt.clone();
            async move {
                let live = rt.route(&session).unwrap();
                let names: Vec<String> = live
                    .opened
                    .harness
                    .get_resources()
                    .await
                    .skills
                    .into_iter()
                    .map(|s| s.name)
                    .collect();
                names
            }
        };
        assert!(offered(skills_of(first.clone()).await));

        // A Session opens with the trusted workspace; trust is revoked
        // before it is published.
        let (reached, release) = rt.pauses.arm("open_publishing");
        let opening = tokio::spawn({
            let rt = rt.clone();
            async move { rt.new_session(SessionSpec::default()).await }
        });
        reached.notified().await;
        let revoked = admin(&rt).trust(trust(false)).await.unwrap();
        assert_eq!(revoked.pending_sessions, 0);
        release.send(false).unwrap();
        let late = opening.await.unwrap().unwrap();
        assert!(!offered(skills_of(late.clone()).await));
        assert!(!offered(skills_of(first).await));
        let current = rt
            .workspaces
            .lock()
            .unwrap()
            .get(&dir.path().canonicalize().unwrap())
            .map(|state| Arc::clone(&state.generation))
            .unwrap();
        assert!(Arc::ptr_eq(&generation(&rt, &late), &current));
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn workspace_toggles_stay_in_their_own_workspace() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let open = |cwd: &std::path::Path| SessionSpec {
            cwd: Some(cwd.to_path_buf()),
            ..SessionSpec::default()
        };
        let in_a = rt.new_session(open(&a)).await.unwrap();
        let in_b = rt.new_session(open(&b)).await.unwrap();
        let disabled = admin(&rt)
            .skills(SkillsParams {
                workspace_id: workspace(&a),
                action: AdminAction::Disable,
                name: Some("notes".into()),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(disabled.revision, 1);
        assert!(!has_notes(&rt, &in_a).await);
        assert!(has_notes(&rt, &in_b).await);
        let b_canonical = b.canonicalize().unwrap();
        assert_eq!(rt.workspace_revision(&b_canonical), 0);
        // B reloaded, or a new Session there, still has it.
        let reloaded = admin(&rt)
            .reload(ReloadParams {
                workspace_id: workspace(&b),
                expected_revision: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(reloaded.revision, 1);
        assert!(has_notes(&rt, &in_b).await);
        let again = rt.new_session(open(&a)).await.unwrap();
        assert!(!has_notes(&rt, &again).await);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_rereads_model_definitions() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _log) = runtime(dir.path()).await;
        let session = rt.new_session(SessionSpec::default()).await.unwrap();
        let models = |ids: &[&str]| {
            json!({"providers": [{"id": "local", "baseUrl": "http://127.0.0.1:9/v1",
                "models": ids}]})
            .to_string()
        };
        let path = dir.path().join("agent/models.json");
        std::fs::write(&path, models(&["first"])).unwrap();
        let catalog = &rt.factory.globals.models;
        assert!(catalog.get_model("local", "first").is_none());
        let reload = || {
            admin(&rt).reload(ReloadParams {
                workspace_id: workspace(dir.path()),
                expected_revision: None,
            })
        };
        reload().await.unwrap();
        assert!(catalog.get_model("local", "first").is_some());
        std::fs::write(&path, models(&["second"])).unwrap();
        reload().await.unwrap();
        assert!(catalog.get_model("local", "second").is_some());
        let fresh = rt.new_session(SessionSpec::default()).await.unwrap();
        let offered = ModelControl::models(&*rt, &fresh).await.unwrap();
        assert!(
            offered.models.iter().any(|m| m.id == "local/second"),
            "{:?}",
            offered.models
        );
        drop(session);
        rt.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn defaults_writes_are_serialized_with_each_other_and_workspace_changes() {
    tokio::time::timeout(LIMIT, async {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "notes");
        let (rt, _log) = runtime(dir.path()).await;
        let _session = rt.new_session(SessionSpec::default()).await.unwrap();
        let save = |ms| {
            rt.update_defaults(DefaultsParams {
                patch: Some(DefaultsPatch {
                    response_header_timeout_ms: Some(ms),
                }),
                expected_revision: Some(0),
                ..DefaultsParams::default()
            })
        };
        let (one, two) = tokio::join!(save(11_000), save(22_000));
        let conflicts = [&one, &two]
            .iter()
            .filter(|r| matches!(r, Err(BackendError::Conflict { revision: 1 })))
            .count();
        assert_eq!(conflicts, 1, "{one:?} {two:?}");
        let saved = one.or(two).unwrap().response_header_timeout_ms.unwrap();

        // A defaults write racing a workspace toggle loses neither.
        let toggle = admin(&rt).skills(SkillsParams {
            workspace_id: workspace(dir.path()),
            action: AdminAction::Disable,
            name: Some("notes".into()),
            expected_revision: None,
        });
        let defaults = rt.update_defaults(DefaultsParams {
            patch: Some(DefaultsPatch {
                response_header_timeout_ms: Some(33_000),
            }),
            ..DefaultsParams::default()
        });
        let (toggled, written) = tokio::join!(toggle, defaults);
        toggled.unwrap();
        assert_eq!(written.unwrap().revision, 2);
        let global = std::fs::read_to_string(dir.path().join("agent/settings.json")).unwrap();
        assert!(
            global.contains("33000") && !global.contains(&saved.to_string()),
            "{global}"
        );
        let overrides =
            std::fs::read_to_string(dir.path().join("agent/workspace-settings.json")).unwrap();
        assert!(overrides.contains("notes"), "{overrides}");
        rt.shutdown().await;
    })
    .await
    .unwrap();
}
