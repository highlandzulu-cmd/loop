//! What a backend supports.

use serde::{Deserialize, Serialize};

/// Features a backend declares. Each must match what the backend can actually
/// do: a surface hides or rejects an operation whose capability is false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Capabilities {
    /// Can reopen a past session, replaying its history.
    pub load_session: bool,
    /// Can reopen a past session without replaying history.
    pub resume_session: bool,
    /// Can switch models within a session.
    pub set_model: bool,
    /// Has selectable session modes.
    pub modes: bool,
    /// Reports token usage.
    pub usage: bool,
    /// Accepts images in prompts.
    pub images: bool,
    /// Accepts more input while a turn is running.
    pub steering: bool,
    /// Sends file reads and writes to Loop instead of touching disk itself.
    pub fs_client: bool,
    /// Runs commands through Loop's terminal instead of its own.
    pub terminal_client: bool,
    /// Manages several Sessions through list and close requests.
    pub multi_session: bool,
    /// Supports attach, cursor replay and detach.
    pub attach: bool,
    /// Lists and navigates Session branches.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_tree: bool,
    /// Changes Session metadata.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_rename: bool,
    /// Creates separately leased forks and clones.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_fork: bool,
    /// Transfers bounded Session payloads.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub session_transfer: bool,
    /// Serves immutable transcript pages.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub branch_snapshot: bool,
    /// Has selectable reasoning effort levels (`session/efforts`).
    pub efforts: bool,
    /// `session/attach` returns a Session snapshot with its result.
    pub attach_snapshot: bool,
}
