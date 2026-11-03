//! Workspace admin, Session sandbox and permission mode
//! (`loop_wire::RuntimeAdmin`). Host-only methods are the daemon's.
//!
//! A workspace change writes the global settings file (or the trust store)
//! and reloads the workspace: new Sessions open with the new resources at
//! once, idle Sessions switch now, and busy ones when their turn or user
//! command ends.

use std::path::Path;
use std::sync::atomic::AtomicUsize;

use loop_agent::harness::SandboxMode;
use loop_app_core::config::{paths, settings::set_disabled_name, settings::set_skill_disabled};
use loop_app_core::config::{Settings, TrustStore};
use loop_protocol::admin::*;
use loop_wire::RuntimeAdmin;

use super::*;

/// A workspace reload a busy Session applies when its work ends.
pub(super) struct PendingWorkspace {
    resources: Arc<WorkspaceResources>,
    retiring: Option<Arc<Retiring>>,
}

/// Replaced workspace resources whose MCP connections close once every
/// Session that still used them has switched.
pub(super) struct Retiring {
    old: Arc<WorkspaceResources>,
    left: AtomicUsize,
}

impl Retiring {
    async fn release(&self) {
        if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.old.mcp_client.disconnect_all().await;
        }
    }
    pub(super) async fn close(&self) {
        self.old.mcp_client.disconnect_all().await;
    }
}

impl LiveSession {
    /// Apply a workspace reload that waited for this Session's work to
    /// end. The caller holds the lifecycle lock and has cleared `busy`.
    pub(super) async fn apply_pending(&self) {
        let pending = self.pending_workspace.lock().unwrap().take();
        if let Some(pending) = pending {
            if let Err(error) = self.opened.apply_workspace(&pending.resources).await {
                eprintln!("Runtime: could not apply the reloaded workspace: {error}");
            }
            if let Some(retiring) = pending.retiring {
                retiring.release().await;
            }
        }
    }
}

fn protocol(message: impl Into<String>) -> BackendError {
    BackendError::failed(ErrorCategory::Protocol, message)
}

impl Runtime {
    fn workspace_opts(&self, cwd: &Path) -> BootstrapOpts {
        let mut opts = self.session_opts();
        opts.cwd = cwd.to_path_buf();
        opts
    }

    /// The workspace an id names: one the Runtime has loaded, or its
    /// startup workspace. Never read as a path.
    fn resolve_workspace(&self, id: &str) -> BackendResult<PathBuf> {
        let mut known: Vec<PathBuf> = self.workspaces.lock().unwrap().keys().cloned().collect();
        if let Ok(cwd) = self.defaults.cwd.canonicalize() {
            known.push(cwd);
        }
        known
            .into_iter()
            .find(|cwd| tree::workspace_id(cwd) == id)
            .ok_or_else(|| protocol(format!("unknown workspace {id}")))
    }

    fn workspace_revision(&self, cwd: &Path) -> u64 {
        self.workspaces
            .lock()
            .unwrap()
            .get(cwd)
            .map_or(0, |state| state.revision)
    }

    fn check_workspace(&self, cwd: &Path, expected: Option<u64>) -> BackendResult<()> {
        let revision = self.workspace_revision(cwd);
        match expected {
            Some(expected) if expected != revision => Err(BackendError::Conflict { revision }),
            _ => Ok(()),
        }
    }

    async fn workspace_now(&self, cwd: &Path) -> BackendResult<Arc<WorkspaceResources>> {
        self.factory
            .workspace(&self.workspace_opts(cwd))
            .await
            .map_err(failed)
    }

    /// Change the global settings file (never a project file), as the
    /// standalone commands do.
    fn edit_settings(&self, edit: impl FnOnce(&mut Settings)) -> BackendResult<()> {
        let path = paths::settings_path(&self.factory.globals.agent_dir);
        let mut global = Settings::load_file(&path).map_err(failed)?;
        edit(&mut global);
        global.save_file(&path).map_err(failed)
    }

    /// Reload `cwd`'s workspace and move its live Sessions onto it: idle
    /// ones now, busy ones when their work ends. Returns the workspace
    /// revision, notices and how many Sessions wait. Caller holds `admin`.
    pub(super) async fn reload_workspace(
        &self,
        cwd: &Path,
    ) -> BackendResult<(Arc<WorkspaceResources>, u64, Vec<String>, u32)> {
        let (fresh, old) = self
            .factory
            .reload_workspace(&self.workspace_opts(cwd))
            .await
            .map_err(failed)?;
        let revision = {
            let mut workspaces = self.workspaces.lock().unwrap();
            let state = workspaces
                .entry(cwd.to_path_buf())
                .or_insert_with(|| WorkspaceState {
                    resources: Arc::clone(&fresh),
                    sandbox: None,
                    revision: 0,
                });
            state.resources = Arc::clone(&fresh);
            state.revision += 1;
            state.revision
        };
        // The reload's own count, released below.
        let retiring = old.map(|old| {
            Arc::new(Retiring {
                old,
                left: AtomicUsize::new(1),
            })
        });
        if let Some(retiring) = &retiring {
            self.retiring.lock().unwrap().push(Arc::clone(retiring));
        }
        let live: Vec<Arc<LiveSession>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|live| live.opened.workspace.cwd == cwd)
            .cloned()
            .collect();
        let mut notices: Vec<String> = Vec::new();
        let mut pending = 0;
        for live in live {
            let _lifecycle = live.lifecycle.lock().await;
            if live.closing.load(Ordering::Acquire) {
                continue;
            }
            if live.busy.load(Ordering::Acquire) {
                pending += 1;
                if let Some(retiring) = &retiring {
                    retiring.left.fetch_add(1, Ordering::AcqRel);
                }
                let replaced = live
                    .pending_workspace
                    .lock()
                    .unwrap()
                    .replace(PendingWorkspace {
                        resources: Arc::clone(&fresh),
                        retiring: retiring.clone(),
                    });
                if let Some(retiring) = replaced.and_then(|p| p.retiring) {
                    retiring.release().await;
                }
                continue;
            }
            match live.opened.apply_workspace(&fresh).await {
                Ok(applied) => {
                    for notice in applied {
                        if !notices.contains(&notice) {
                            notices.push(notice);
                        }
                    }
                }
                Err(error) => notices.push(format!("{}: {error}", live.reference.key)),
            }
        }
        if let Some(retiring) = &retiring {
            retiring.release().await;
        }
        if notices.is_empty() {
            notices.extend(fresh.startup_notices.iter().cloned());
        }
        Ok((fresh, revision, notices, pending))
    }

    async fn mcp_servers(workspace: &WorkspaceResources) -> Vec<McpServerStatus> {
        let connected: HashMap<String, usize> = workspace
            .mcp_client
            .list_connections()
            .await
            .into_iter()
            .collect();
        let settings = &workspace.settings;
        settings
            .mcp_servers
            .keys()
            .map(|name| {
                let enabled = !settings.disabled_mcp_servers.contains(name);
                let tools = connected.get(name).copied();
                McpServerStatus {
                    name: name.clone(),
                    enabled,
                    connected: tools.is_some(),
                    tools: tools.unwrap_or(0) as u32,
                    error: (enabled && tools.is_none()).then(|| "not connected".into()),
                }
            })
            .collect()
    }

    fn skills_of(workspace: &WorkspaceResources) -> Vec<SkillStatus> {
        let resources = &workspace.resources;
        let mut skills: Vec<SkillStatus> = resources
            .skills
            .iter()
            .map(|s| (s, true))
            .chain(resources.disabled_skills.iter().map(|s| (s, false)))
            .map(|(skill, enabled)| SkillStatus {
                name: skill.name.clone(),
                description: skill.description.clone(),
                enabled,
            })
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// The sandbox a Session runs in, as it would be chosen.
    pub(super) fn sandbox_choice(
        settings: &loop_app_core::config::Settings,
        sandboxed: bool,
    ) -> SandboxChoice {
        if sandboxed {
            SandboxChoice {
                mode: "local".into(),
                isolation: settings.sandbox.isolation.clone(),
                runtime: settings.sandbox.runtime.clone(),
            }
        } else {
            SandboxChoice {
                mode: "off".into(),
                ..SandboxChoice::default()
            }
        }
    }

    /// Start the sandbox `choice` asks for, validated by the platform builder.
    async fn start_sandbox(
        &self,
        workspace: &WorkspaceResources,
        choice: &SandboxChoice,
    ) -> BackendResult<SandboxMode> {
        use loop_app_core::sandbox_builder::{
            parse_local_sandbox_flags, SandboxBuilder, SandboxRequest,
        };
        match choice.mode.as_str() {
            "off" | "disabled" => Ok(SandboxMode::Disabled),
            "local" => {
                let args: Vec<String> = [&choice.isolation, &choice.runtime]
                    .into_iter()
                    .flatten()
                    .map(|flag| format!("--{flag}"))
                    .collect();
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let flags = parse_local_sandbox_flags(&args).map_err(protocol)?;
                let builder = SandboxBuilder::for_current_platform()
                    .with_agent_dir(&self.factory.globals.agent_dir);
                let request = SandboxRequest::for_flags(&workspace.settings.sandbox, flags);
                builder
                    .resolve(&request)
                    .map_err(|e| protocol(e.to_string()))?;
                let (sandbox, _plan) = builder
                    .start(&workspace.cwd, &request)
                    .await
                    .map_err(|e| failed(format!("sandbox: {e}")))?;
                Ok(SandboxMode::Enabled { sandbox })
            }
            other => Err(protocol(format!(
                "unknown sandbox mode `{other}`: use off or local"
            ))),
        }
    }
}

#[async_trait]
impl RuntimeAdmin for Runtime {
    fn offered(&self) -> Capabilities {
        Capabilities {
            workspace_mcp: true,
            workspace_web_search: true,
            workspace_skills: true,
            workspace_templates: true,
            workspace_reload: true,
            workspace_trust: true,
            session_sandbox: true,
            permission_mode: true,
            ..Capabilities::default()
        }
    }

    async fn mcp(&self, params: McpParams) -> BackendResult<McpResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = match params.action {
            AdminAction::List => self.workspace_now(&cwd).await?,
            AdminAction::Reload => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
            action => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let name = params
                    .name
                    .ok_or_else(|| protocol("name the MCP server to enable or disable"))?;
                let current = self.workspace_now(&cwd).await?;
                if !current.settings.mcp_servers.contains_key(&name) {
                    return Err(protocol(format!("no MCP server is named `{name}`")));
                }
                let enabled = action == AdminAction::Enable;
                self.edit_settings(|s| {
                    set_disabled_name(&mut s.disabled_mcp_servers, &name, enabled)
                })?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
        };
        Ok(McpResult {
            revision: self.workspace_revision(&cwd),
            servers: Self::mcp_servers(&workspace).await,
            pending_sessions,
        })
    }

    async fn web_search(&self, params: WebSearchParams) -> BackendResult<WebSearchResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = if params.enabled.is_none() && params.provider.is_none() {
            self.workspace_now(&cwd).await?
        } else {
            self.check_workspace(&cwd, params.expected_revision)?;
            let provider = params.provider.map(|p| p.trim().to_ascii_lowercase());
            if let Some(provider) = &provider {
                if loop_agent::harness::web_search_backend(provider).is_none() {
                    let ids: Vec<&str> = loop_agent::harness::WEB_SEARCH_BACKENDS
                        .iter()
                        .map(|b| b.id)
                        .collect();
                    return Err(protocol(format!(
                        "unknown search provider `{provider}`; choose one of: {}",
                        ids.join(", ")
                    )));
                }
            }
            self.edit_settings(|s| {
                let web = s.web_search.get_or_insert_with(Default::default);
                if let Some(enabled) = params.enabled {
                    web.enabled = enabled;
                }
                if provider.is_some() {
                    web.provider = provider;
                }
            })?;
            let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
            pending_sessions = pending;
            fresh
        };
        let config = workspace.settings.web_search.clone().unwrap_or_default();
        Ok(WebSearchResult {
            revision: self.workspace_revision(&cwd),
            enabled: config.enabled,
            provider: config.provider,
            status: workspace.web_status(),
            pending_sessions,
        })
    }

    async fn skills(&self, params: SkillsParams) -> BackendResult<SkillsResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let mut pending_sessions = 0;
        let workspace = match params.action {
            AdminAction::List => self.workspace_now(&cwd).await?,
            AdminAction::Reload => {
                return Err(protocol("skills are reloaded with workspace/reload"))
            }
            action => {
                self.check_workspace(&cwd, params.expected_revision)?;
                let name = params
                    .name
                    .ok_or_else(|| protocol("name the skill to enable or disable"))?;
                let current = self.workspace_now(&cwd).await?;
                if !Self::skills_of(&current).iter().any(|s| s.name == name) {
                    return Err(protocol(format!("no skill is named `{name}`")));
                }
                let enabled = action == AdminAction::Enable;
                self.edit_settings(|s| set_skill_disabled(&mut s.disabled_skills, &name, enabled))?;
                let (fresh, _, _, pending) = self.reload_workspace(&cwd).await?;
                pending_sessions = pending;
                fresh
            }
        };
        Ok(SkillsResult {
            revision: self.workspace_revision(&cwd),
            skills: Self::skills_of(&workspace),
            pending_sessions,
        })
    }

    async fn templates(&self, params: TemplatesParams) -> BackendResult<TemplatesResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let workspace = self.workspace_now(&cwd).await?;
        let prompts = &workspace.resources.prompts;
        let revision = self.workspace_revision(&cwd);
        if let Some(name) = params.name {
            let template = prompts
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| protocol(format!("no prompt template is named `{name}`")))?;
            let text = loop_agent::harness::prompt_templates::substitute_args(
                &template.body,
                &params.args,
            );
            return Ok(TemplatesResult {
                revision,
                templates: None,
                text: Some(text),
            });
        }
        let templates = prompts
            .iter()
            .map(|t| TemplateInfo {
                name: t.name.clone(),
                description: t.argument_hint.clone().unwrap_or_else(|| {
                    t.body
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect()
                }),
            })
            .collect();
        Ok(TemplatesResult {
            revision,
            templates: Some(templates),
            text: None,
        })
    }

    async fn reload(&self, params: ReloadParams) -> BackendResult<ReloadResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        self.check_workspace(&cwd, params.expected_revision)?;
        let (_, revision, notices, pending_sessions) = self.reload_workspace(&cwd).await?;
        Ok(ReloadResult {
            revision,
            notices,
            pending_sessions,
        })
    }

    async fn trust(&self, params: TrustParams) -> BackendResult<TrustResult> {
        let cwd = self.resolve_workspace(&params.workspace_id)?;
        let _admin = self.admin.lock().await;
        let path = paths::trust_path(&self.factory.globals.agent_dir);
        let mut pending_sessions = 0;
        if let Some(trusted) = params.trusted {
            // The wire refuses this without host-local authority.
            self.check_workspace(&cwd, params.expected_revision)?;
            let mut store = TrustStore::load(&path).map_err(failed)?;
            store.set(&cwd, trusted).map_err(failed)?;
            pending_sessions = self.reload_workspace(&cwd).await?.3;
        }
        let trusted = TrustStore::load(&path).map_err(failed)?.get(&cwd) == Some(true);
        Ok(TrustResult {
            revision: self.workspace_revision(&cwd),
            trusted,
            pending_sessions,
        })
    }

    async fn sandbox(&self, params: SandboxParams) -> BackendResult<SandboxResult> {
        let live = self.loaded(&params.session).await?;
        if params.action == SandboxAction::Set {
            let choice = params
                .config
                .ok_or_else(|| protocol("give the sandbox config to set"))?;
            let _lifecycle = live.lifecycle.lock().await;
            if live.closing.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire) {
                return Err(failed("Session closing"));
            }
            if live.busy.load(Ordering::Acquire) {
                return Err(BackendError::Busy);
            }
            let native = &live.opened.native;
            if let Some(expected) = params.expected_revision {
                if expected != native.revision() {
                    return Err(BackendError::Conflict {
                        revision: native.revision(),
                    });
                }
            }
            let workspace = self.workspace_now(&live.opened.workspace.cwd).await?;
            let mode = self.start_sandbox(&workspace, &choice).await?;
            let shared = self
                .workspaces
                .lock()
                .unwrap()
                .get(&workspace.cwd)
                .and_then(|state| state.sandbox.clone());
            let previous = live.opened.harness.replace_sandbox(mode).await;
            // The workspace's own sandbox serves other Sessions; keep it.
            if let SandboxMode::Enabled { sandbox } = previous {
                if !shared.is_some_and(|shared| Arc::ptr_eq(&shared, &sandbox)) {
                    let _ = sandbox.destroy().await;
                }
            }
            live.opened
                .apply_workspace(&workspace)
                .await
                .map_err(failed)?;
            let sandboxed = choice.mode == "local";
            *live.sandbox_choice.lock().unwrap() = if sandboxed {
                choice
            } else {
                Self::sandbox_choice(&workspace.settings, false)
            };
        }
        let info = live.opened.harness.sandbox_info().await.format_box();
        let config = live.sandbox_choice.lock().unwrap().clone();
        Ok(SandboxResult {
            config,
            info,
            revision: live.opened.native.revision(),
            seq: None,
        })
    }

    async fn permission_mode(
        &self,
        params: PermissionModeParams,
    ) -> BackendResult<PermissionModeResult> {
        use loop_app_core::permissions::PermissionMode;
        let session = &params.session;
        let Some(name) = params.mode else {
            let live = self.loaded(session).await?;
            let mode = live.opened.tool_gate.mode();
            return Ok(PermissionModeResult {
                mode: mode.as_str().into(),
                effective_policy: mode.description().into(),
                revision: live.opened.native.revision(),
                seq: None,
                pending: false,
            });
        };
        let mode = PermissionMode::parse(&name).ok_or_else(|| {
            protocol(format!(
                "unknown permission mode `{name}`: use read-only, ask, accept-edits, full-access or plan"
            ))
        })?;
        let expected = params.expected_revision;
        let result = self
            .set_config(session, false, |live| async move {
                let result = live
                    .opened
                    .native
                    .record_permission_mode(session, mode.as_str(), expected)
                    .await?;
                // The gate checks every tool call, so the mode applies at once.
                live.opened.tool_gate.set_mode(mode);
                Ok(result)
            })
            .await?;
        Ok(PermissionModeResult {
            mode: mode.as_str().into(),
            effective_policy: mode.description().into(),
            revision: result.revision,
            seq: Some(result.seq),
            pending: false,
        })
    }
}
