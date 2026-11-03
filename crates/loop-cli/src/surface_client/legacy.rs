//! Temporary native escape hatch for S3.5 groups 3–6. Remove group by group.
//! Neither this type nor SurfaceClient dereferences to the application Runtime.
use super::*;
use loop_app_core::{
    runtime::{ConnectedProvider, McpToggleOutcome},
    Runtime,
};

pub struct LegacyHost {
    pub(super) inner: Runtime,
}
impl LegacyHost {
    pub(super) fn clear_session_approvals(&self) {
        if let Some(bridge) = &self.inner.tool_approval {
            bridge.clear_session_grants();
        }
    }
    pub(super) async fn restore_session_approvals(&self) {
        self.clear_session_approvals();
        if let Some(bridge) = &self.inner.tool_approval {
            if let Ok(entries) = self.inner.harness.read_session_entries().await {
                for group in crate::tool_approval::auto_approve_from_entries(&entries) {
                    bridge.grant_session(&group);
                }
            }
        }
    }
    pub fn permission_mode(&self) -> loop_app_core::permissions::PermissionMode {
        self.inner.permission_mode()
    }
    pub async fn set_permission_mode(&mut self, mode: loop_app_core::permissions::PermissionMode) {
        self.inner.set_permission_mode(mode).await
    }
    pub async fn sync_agent_mode(&mut self) -> Option<String> {
        self.inner.sync_agent_mode().await
    }
    pub fn save_settings(&self) -> anyhow::Result<()> {
        self.inner.save_settings()
    }
    pub async fn sync_harness_resources(&self) {
        self.inner.sync_harness_resources().await
    }
    pub async fn set_skill_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<bool> {
        self.inner.set_skill_enabled(name, enabled).await
    }
    pub async fn set_mcp_server_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> anyhow::Result<McpToggleOutcome> {
        self.inner.set_mcp_server_enabled(name, enabled).await
    }
    pub async fn refresh_mcp_tools(&self) -> anyhow::Result<()> {
        self.inner.refresh_mcp_tools().await
    }
    pub async fn connect_provider(
        &mut self,
        request: &crate::config::ProviderLoginRequest,
    ) -> anyhow::Result<ConnectedProvider> {
        self.inner.connect_provider(request).await
    }
    pub async fn connect_web_search(
        &mut self,
        provider: &str,
        api_key: &str,
    ) -> anyhow::Result<String> {
        self.inner.connect_web_search(provider, api_key).await
    }
    pub fn connected_providers(&self) -> Vec<String> {
        self.inner.connected_providers()
    }
    pub async fn logout_provider(&mut self, provider: &str) -> anyhow::Result<String> {
        self.inner.logout_provider(provider).await
    }
    pub fn web_tools(&self) -> loop_app_core::web_tools::WebTools {
        self.inner.web_tools()
    }
    pub fn bash_limits(&self) -> loop_agent::harness::BashLimits {
        self.inner.bash_limits()
    }
    pub fn web_status(&self) -> String {
        self.inner.web_status()
    }
    pub async fn set_web_enabled(&mut self, enabled: bool) -> anyhow::Result<String> {
        self.inner.set_web_enabled(enabled).await
    }
    pub async fn set_web_provider(&mut self, provider: &str) -> anyhow::Result<String> {
        self.inner.set_web_provider(provider).await
    }
    #[cfg(feature = "telemetry")]
    pub fn usage_log(&self) -> Option<loop_telemetry::UsageLog> {
        self.inner.usage_log()
    }
    #[cfg(feature = "telemetry")]
    pub fn flush_usage_log(&self) {
        self.inner.flush_usage_log()
    }
    #[cfg(feature = "telemetry")]
    pub fn tracing_status(&self) -> Option<loop_telemetry::TelemetryStatus> {
        self.inner.tracing_status()
    }
    #[cfg(feature = "telemetry")]
    pub fn set_tracing_enabled(
        &mut self,
        enabled: bool,
    ) -> anyhow::Result<loop_telemetry::TelemetryStatus> {
        self.inner.set_tracing_enabled(enabled)
    }
    #[cfg(feature = "telemetry")]
    pub fn setup_tracing(
        &mut self,
        request: &crate::config::TracingSetupRequest,
    ) -> anyhow::Result<loop_telemetry::TelemetryStatus> {
        self.inner.setup_tracing(request)
    }
    /// Group 3 (`/ttfb` default): the response timeout for new Sessions.
    pub async fn set_default_response_timeout(&self, ms: u64) {
        self.inner.native.set_default_response_timeout(ms).await
    }
    /// Group 3 (`/reload`): refresh the model catalog.
    pub async fn refresh_models(&self) {
        let _ = self
            .inner
            .models
            .refresh(loop_ai::ModelsRefreshOptions {
                allow_network: Some(true),
                force: true,
                provider_id: None,
            })
            .await;
    }
    pub fn harness(&self) -> &Arc<loop_agent::harness::AgentHarness> {
        &self.inner.harness
    }
    pub fn agents(&self) -> &Arc<loop_app_core::agents::AgentManager> {
        &self.inner.agents
    }
    pub fn settings(&self) -> &crate::config::Settings {
        &self.inner.settings
    }
    pub fn settings_mut(&mut self) -> &mut crate::config::Settings {
        &mut self.inner.settings
    }
    pub fn resources(&self) -> &crate::resources::LoadedResources {
        &self.inner.resources
    }
    pub fn resources_mut(&mut self) -> &mut crate::resources::LoadedResources {
        &mut self.inner.resources
    }
    pub fn credentials(&self) -> &Arc<crate::config::auth::FileCredentialStore> {
        &self.inner.credentials
    }
    pub fn mcp_client(&self) -> &Arc<loop_mcp::McpClientManager> {
        &self.inner.mcp_client
    }
    pub fn tool_gate(&self) -> &Arc<loop_app_core::tool_gate::ToolGate> {
        &self.inner.tool_gate
    }
    pub fn active_skills(&self) -> &Vec<String> {
        &self.inner.active_skills
    }
    pub fn active_skills_mut(&mut self) -> &mut Vec<String> {
        &mut self.inner.active_skills
    }
    pub fn trust(&self) -> &crate::config::trust::TrustStore {
        &self.inner.trust
    }
    pub fn trust_mut(&mut self) -> &mut crate::config::trust::TrustStore {
        &mut self.inner.trust
    }
    pub fn project_trusted(&self) -> bool {
        self.inner.project_trusted
    }
    pub fn project_trusted_mut(&mut self) -> &mut bool {
        &mut self.inner.project_trusted
    }
    pub fn tool_approval(&self) -> &Option<Arc<ToolApprovalBridge>> {
        &self.inner.tool_approval
    }
    pub fn tool_approval_mut(&mut self) -> &mut Option<Arc<ToolApprovalBridge>> {
        &mut self.inner.tool_approval
    }
}
