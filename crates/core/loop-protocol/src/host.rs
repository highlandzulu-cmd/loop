//! Host-only credential and telemetry services. Transports supply authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;
/// Requested host operation; each service validates its own allowed actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HostAction {
    /// Read connected providers.
    List,
    /// Verify and connect a provider.
    Connect,
    /// Disconnect a provider.
    Disconnect,
    /// Read telemetry state.
    Status,
    /// Enable telemetry.
    Enable,
    /// Disable telemetry.
    Disable,
    /// Configure telemetry.
    Setup,
    /// Start MCP OAuth.
    Login,
    /// Remove MCP OAuth credentials.
    Logout,
}
/// Provider request. Login payloads are never part of Session history or Debug output.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProvidersParams {
    /// Operation.
    pub action: HostAction,
    /// Provider identity for disconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Validated provider setup payload; may contain secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<Value>,
}
impl std::fmt::Debug for ProvidersParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvidersParams")
            .field("action", &self.action)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}
/// A provider's public connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderInfo {
    /// Stable provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Credentials are connected.
    pub connected: bool,
    /// Models found during connect, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_count: Option<usize>,
}
/// Redacted provider result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProvidersResult {
    /// Public provider rows.
    pub providers: Vec<ProviderInfo>,
    /// Status notices, without credentials.
    pub notices: Vec<String>,
}
/// Host telemetry operation; setup payloads may contain credentials.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TracingParams {
    /// Operation.
    pub action: HostAction,
    /// Validated setup payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}
impl std::fmt::Debug for TracingParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracingParams")
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}
/// Redacted telemetry state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TracingResult {
    /// Enabled preference.
    pub enabled: bool,
    /// Chosen backend.
    pub backend: String,
    /// Human-readable redacted status.
    pub status: String,
}
/// Host MCP OAuth request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpAuthParams {
    /// Opaque workspace id.
    pub workspace_id: String,
    /// Login or logout.
    pub action: HostAction,
    /// Configured server name.
    pub name: String,
}
/// Host MCP OAuth result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpAuthResult {
    /// Authorized, not_required, removed or absent.
    pub status: String,
    /// Public status messages.
    pub notices: Vec<String>,
}
