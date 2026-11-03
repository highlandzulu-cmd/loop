//! Workspace admin and host-only methods (design `tui-client.md` §3).
//!
//! Every workspace method names its workspace by the opaque id the host
//! reports in snapshots and listings; it is never read as a path. Host-only
//! methods are served on the Runtime host's protected socket or an embedded
//! adapter, never over a WebSocket.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SessionRef;

/// What `workspace/mcp` and `workspace/skills` do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AdminAction {
    /// Read the current state.
    #[default]
    List,
    /// Enable `name`.
    Enable,
    /// Disable `name`.
    Disable,
    /// Reconnect every enabled server (`workspace/mcp` only).
    Reload,
}

/// `workspace/mcp` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// What to do; list by default.
    #[serde(default)]
    pub action: AdminAction,
    /// The server to enable or disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Fail with a conflict unless the workspace is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// One configured MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerStatus {
    /// Configured name.
    pub name: String,
    /// Enabled in settings.
    pub enabled: bool,
    /// Connected now.
    pub connected: bool,
    /// Tools it offers.
    pub tools: u32,
    /// Why it is not connected, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `workspace/mcp` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpResult {
    /// Workspace revision.
    pub revision: u64,
    /// Configured servers.
    pub servers: Vec<McpServerStatus>,
    /// Busy Sessions that pick the change up when their turn ends.
    pub pending_sessions: u32,
}

/// `workspace/web_search` params. Without `enabled` or `provider` it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebSearchParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// Turn Loop's web tools on or off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Search with this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Fail with a conflict unless the workspace is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `workspace/web_search` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebSearchResult {
    /// Workspace revision.
    pub revision: u64,
    /// Whether the web tools are on.
    pub enabled: bool,
    /// Configured search provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Readable status; never contains keys.
    pub status: String,
    /// Busy Sessions that pick the change up when their turn ends.
    pub pending_sessions: u32,
}

/// `workspace/skills` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillsParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// What to do; list by default. `reload` is not accepted.
    #[serde(default)]
    pub action: AdminAction,
    /// The skill to enable or disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Fail with a conflict unless the workspace is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// One discovered skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillStatus {
    /// Skill name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Offered to Sessions.
    pub enabled: bool,
}

/// `workspace/skills` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillsResult {
    /// Workspace revision.
    pub revision: u64,
    /// Discovered skills.
    pub skills: Vec<SkillStatus>,
    /// Busy Sessions that pick the change up when their turn ends.
    pub pending_sessions: u32,
}

/// `workspace/templates` params. With `name` it renders that template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplatesParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// The template to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Arguments for the template.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// One prompt template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplateInfo {
    /// Template name, used as `/<name>`.
    pub name: String,
    /// What it is for.
    pub description: String,
}

/// `workspace/templates` result: the list, or one rendered text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplatesResult {
    /// Workspace revision.
    pub revision: u64,
    /// Templates, for a list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<TemplateInfo>>,
    /// Rendered prompt text, for a render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `workspace/reload` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReloadParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// Fail with a conflict unless the workspace is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `workspace/reload` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReloadResult {
    /// Workspace revision.
    pub revision: u64,
    /// What the reload reported.
    pub notices: Vec<String>,
    /// Busy Sessions that pick the change up when their turn ends.
    pub pending_sessions: u32,
}

/// `workspace/trust` params. Without `trusted` it reads; a write needs
/// host-local authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrustParams {
    /// Host workspace id.
    pub workspace_id: String,
    /// Trust or distrust the workspace's project files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted: Option<bool>,
    /// Fail with a conflict unless the workspace is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `workspace/trust` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TrustResult {
    /// Workspace revision.
    pub revision: u64,
    /// Whether project files are trusted.
    pub trusted: bool,
    /// Busy Sessions that pick the change up when their turn ends.
    pub pending_sessions: u32,
}

/// A Session's sandbox choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxChoice {
    /// `off` or `local`.
    pub mode: String,
    /// Local isolation (`full` or `partial`); the platform default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// Local container runtime; the platform default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// What `session/sandbox` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SandboxAction {
    /// Read the Session's sandbox.
    #[default]
    Status,
    /// Replace it with `config`.
    Set,
}

/// `session/sandbox` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxParams {
    /// Session reference.
    pub session: SessionRef,
    /// What to do; status by default.
    #[serde(default)]
    pub action: SandboxAction,
    /// The new sandbox, for `set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SandboxChoice>,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `session/sandbox` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SandboxResult {
    /// The Session's sandbox now.
    pub config: SandboxChoice,
    /// Readable status, as `/sandbox status` prints it.
    pub info: String,
    /// The Session revision.
    pub revision: u64,
    /// The update sequence that announced a change; absent for a read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `session/permission_mode` params. Without `mode` it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PermissionModeParams {
    /// Session reference.
    pub session: SessionRef,
    /// `read-only`, `ask`, `accept-edits`, `full-access` or `plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Fail with a conflict unless the Session is at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `session/permission_mode` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PermissionModeResult {
    /// The Session's permission mode.
    pub mode: String,
    /// What the mode allows, in words.
    pub effective_policy: String,
    /// The Session revision.
    pub revision: u64,
    /// The update sequence that announced a change; absent for a read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// The change waits for the running turn to end.
    pub pending: bool,
}

/// `runtime/usage` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageParams {
    /// Only the last this many days; all retained history when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    /// Page cursor from a previous result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Rows per page: 100 by default, at most 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `runtime/usage` result: the host's usage log, one row per Session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsageResult {
    /// Session rows, most recent first.
    pub rows: Vec<Value>,
    /// Totals over the whole period.
    pub totals: Value,
    /// Cursor for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// When the log was read, in ms since the epoch.
    pub taken_at_ms: u64,
    /// Whether the log could be read, in words.
    pub log_status: String,
}

/// Changes to saved defaults for new Sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DefaultsPatch {
    /// Default model for new Sessions, `provider/id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default reasoning effort for new Sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Response-header timeout for new Sessions in ms; 0 waits indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_header_timeout_ms: Option<u64>,
}

/// `runtime/defaults` params. Without `patch` it reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DefaultsParams {
    /// Host workspace id, for workspace-specific values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// What to save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<DefaultsPatch>,
    /// Fail with a conflict unless the defaults are at this revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

/// `runtime/defaults` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DefaultsResult {
    /// Defaults revision.
    pub revision: u64,
    /// Default model, `provider/id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Default permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Response-header timeout for new Sessions in ms; 0 waits indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_header_timeout_ms: Option<u64>,
}

/// One loaded Session in `runtime/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeSessionRow {
    /// Session reference.
    pub session: SessionRef,
    /// Host workspace id.
    pub workspace_id: String,
    /// `idle`, `busy`, or `parked` (waiting on an Escalation).
    pub state: String,
    /// Selected model, `provider/id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens billed so far; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// Totals over the listed rows only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeSessionTotals {
    /// Loaded Sessions, children counted separately.
    pub sessions: u32,
    /// Sessions with work running.
    pub busy: u32,
    /// Known tokens over the rows.
    pub tokens: u64,
    /// Rows whose tokens are unknown.
    pub unknown_tokens: u32,
}

/// `runtime/sessions` result: the Runtime panel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeSessionsResult {
    /// One row per loaded Session.
    pub sessions: Vec<RuntimeSessionRow>,
    /// Totals for these rows.
    pub totals: RuntimeSessionTotals,
    /// When the rows were read, in ms since the epoch.
    pub taken_at_ms: u64,
}
