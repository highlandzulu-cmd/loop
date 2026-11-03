//! Native implementation of the same typed admin requests used by a Runtime.
use super::*;
use loop_protocol::{admin::*, BackendError, BackendResult, Capabilities, ErrorCategory};
use loop_wire::RuntimeAdmin;

pub(super) struct NativeAdmin<'a>(pub tokio::sync::Mutex<&'a mut loop_app_core::Runtime>);
fn failed(e: impl std::fmt::Display) -> BackendError {
    BackendError::failed(ErrorCategory::Other, e.to_string())
}
impl NativeAdmin<'_> {
    fn workspace(runtime: &loop_app_core::Runtime, id: &str) -> BackendResult<()> {
        if id
            != loop_runtime::workspace_id(
                &runtime
                    .cwd
                    .canonicalize()
                    .unwrap_or_else(|_| runtime.cwd.clone()),
            )
        {
            return Err(failed("unknown workspace"));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl RuntimeAdmin for NativeAdmin<'_> {
    fn offered(&self) -> Capabilities {
        Capabilities {
            workspace_mcp: true,
            workspace_trust: true,
            permission_mode: true,
            ..Default::default()
        }
    }
    async fn mcp(&self, p: McpParams) -> BackendResult<McpResult> {
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        let mut errors = std::collections::HashMap::new();
        match p.action {
            AdminAction::Enable | AdminAction::Disable => {
                let name = p
                    .name
                    .as_deref()
                    .ok_or_else(|| failed("missing MCP server name"))?;
                match r
                    .set_mcp_server_enabled(name, p.action == AdminAction::Enable)
                    .await
                    .map_err(failed)?
                {
                    loop_app_core::runtime::McpToggleOutcome::Missing => {
                        return Err(failed(format!(
                            "mcp server not found: {name} (see /mcp list)"
                        )))
                    }
                    loop_app_core::runtime::McpToggleOutcome::ConnectFailed(error) => {
                        errors.insert(name.to_string(), error);
                    }
                    loop_app_core::runtime::McpToggleOutcome::Updated => {}
                }
            }
            AdminAction::Reload => {
                r.mcp_client.disconnect_all().await;
                let entries = loop_app_core::runtime::enabled_mcp_server_entries(
                    &r.settings.mcp_servers,
                    &r.settings.disabled_mcp_servers,
                );
                for (name, result) in r.mcp_client.connect_all(&entries).await {
                    if let Err(error) = result {
                        errors.insert(name, error);
                    }
                }
                r.refresh_mcp_tools().await.map_err(failed)?;
            }
            AdminAction::List => {}
        }
        let connected: std::collections::HashMap<_, _> =
            r.mcp_client.list_connections().await.into_iter().collect();
        Ok(McpResult {
            servers: r
                .settings
                .mcp_servers
                .keys()
                .map(|name| McpServerStatus {
                    name: name.clone(),
                    enabled: !r.settings.disabled_mcp_servers.contains(name),
                    connected: connected.contains_key(name),
                    tools: connected.get(name).copied().unwrap_or(0) as u32,
                    error: errors.remove(name),
                })
                .collect(),
            ..Default::default()
        })
    }
    async fn permission_mode(
        &self,
        p: PermissionModeParams,
    ) -> BackendResult<PermissionModeResult> {
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        if p.session != r.session_ref() {
            return Err(failed("unknown Session"));
        }
        if let Some(mode) = p.mode {
            let mode = loop_app_core::permissions::PermissionMode::parse(&mode)
                .ok_or_else(|| failed("unknown permission mode"))?;
            if r.is_native() {
                r.native
                    .record_permission_mode(&p.session, mode.as_str(), p.expected_revision)
                    .await?;
            }
            r.set_permission_mode(mode).await;
        }
        let mode = r.permission_mode();
        Ok(PermissionModeResult {
            mode: mode.as_str().into(),
            effective_policy: mode.description().into(),
            ..Default::default()
        })
    }
    async fn trust(&self, p: TrustParams) -> BackendResult<TrustResult> {
        let mut guard = self.0.lock().await;
        let r = &mut **guard;
        Self::workspace(r, &p.workspace_id)?;
        if let Some(trusted) = p.trusted {
            r.trust.set(&r.cwd, trusted).map_err(failed)?;
            r.project_trusted = trusted;
            r.resources =
                crate::resources::load_resources(&r.agent_dir, &r.cwd, trusted, &r.settings);
            r.sync_harness_resources().await;
        }
        Ok(TrustResult {
            trusted: r.project_trusted,
            ..Default::default()
        })
    }
}

/// An owned sandbox service lets the terminal keep draining input during startup.
pub(super) struct NativeSandbox {
    pub native: Arc<loop_agent::native_backend::NativeBackend>,
    pub harness: Arc<loop_agent::harness::AgentHarness>,
    pub cwd: std::path::PathBuf,
    pub agent_dir: std::path::PathBuf,
    pub settings: crate::config::settings::SandboxSettings,
    pub web: loop_app_core::web_tools::WebTools,
    pub search: loop_agent::harness::SearchFilter,
    pub bash: loop_agent::harness::BashLimits,
}
#[async_trait::async_trait]
impl RuntimeAdmin for NativeSandbox {
    fn offered(&self) -> Capabilities {
        Capabilities {
            session_sandbox: true,
            ..Default::default()
        }
    }
    async fn sandbox(&self, p: SandboxParams) -> BackendResult<SandboxResult> {
        use loop_agent::harness::SandboxMode;
        use loop_app_core::sandbox_builder::{SandboxBuilder, SandboxRequest};
        if self.native.current_session().await != p.session {
            return Err(BackendError::SessionNotFound(p.session.key));
        }
        self.native.check_session_revision(p.expected_revision)?;
        let mut choice = SandboxChoice {
            mode: self.settings.mode.clone(),
            isolation: self.settings.isolation.clone(),
            runtime: self.settings.runtime.clone(),
        };
        if p.action == SandboxAction::Set {
            if self.native.is_busy(&p.session) {
                return Err(BackendError::Busy);
            }
            choice = p
                .config
                .ok_or_else(|| failed("missing sandbox configuration"))?;
            let mode = match choice.mode.as_str() {
                "off" => SandboxMode::Disabled,
                "local" => {
                    let mut settings = self.settings.clone();
                    settings.mode = choice.mode.clone();
                    settings.isolation = choice.isolation.clone();
                    settings.runtime = choice.runtime.clone();
                    let request = SandboxRequest::from_settings(&settings).map_err(failed)?;
                    let builder =
                        SandboxBuilder::for_current_platform().with_agent_dir(&self.agent_dir);
                    let (sandbox, _) = builder.start(&self.cwd, &request).await.map_err(failed)?;
                    SandboxMode::Enabled { sandbox }
                }
                _ => return Err(failed("unknown sandbox mode")),
            };
            let previous = self.harness.replace_sandbox(mode).await;
            let tools = async {
                let env = self.harness.tool_env().await.map_err(failed)?;
                self.harness
                    .set_tools(crate::build_tools(
                        env,
                        &self.web,
                        self.bash,
                        self.search.clone(),
                    ))
                    .await
                    .map_err(failed)
            }
            .await;
            if let Err(error) = tools {
                let rejected = self.harness.replace_sandbox(previous).await;
                if let SandboxMode::Enabled { sandbox } = rejected {
                    let _ = sandbox.destroy().await;
                }
                return Err(error);
            }
            if let SandboxMode::Enabled { sandbox } = previous {
                let _ = sandbox.destroy().await;
            }
        }
        Ok(SandboxResult {
            config: choice,
            info: self.harness.sandbox_info().await.format_box(),
            ..Default::default()
        })
    }
}
