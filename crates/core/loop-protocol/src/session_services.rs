//! Session skill activation and streamed workflow control.
use crate::SessionRef;
use serde::{Deserialize, Serialize};
/// Activate or deactivate one Session skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillParams {
    /// Target Session.
    pub session: SessionRef,
    /// Discovered skill name.
    pub name: String,
    /// Activate when true; deactivate when false.
    #[serde(default = "active")]
    pub active: bool,
    /// Optimistic Session revision.
    #[serde(default)]
    pub expected_revision: Option<u64>,
}
fn active() -> bool {
    true
}
/// Start a workflow. Repeating the same request id returns the same workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowParams {
    /// Target Session.
    pub session: SessionRef,
    /// Goal to plan and execute.
    pub goal: String,
    /// Maximum parallel tasks, from 1 through 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// Caller-generated id for deduplication.
    pub request_id: String,
}
/// An accepted workflow; execution reports on the Session update stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowResult {
    /// Stable workflow id.
    pub workflow_id: String,
}
/// Workflow lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    /// Planning completed.
    Planned,
    /// A task started.
    TaskStarted,
    /// A task succeeded.
    TaskCompleted,
    /// A task failed.
    TaskFailed,
    /// Execution ended, including planning errors and cancellation.
    Ended,
}
/// Final workflow output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowCompletion {
    /// All tasks succeeded.
    pub success: bool,
    /// Successful task count.
    pub completed_count: usize,
    /// Failed task count.
    pub failed_count: usize,
    /// Total task count.
    pub total_count: usize,
    /// Combined output.
    pub output: String,
    /// Written artifacts.
    pub artifacts: Vec<String>,
}
/// Progress through the same ordered stream as all Session updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkflowProgress {
    /// Workflow identity.
    pub workflow_id: String,
    /// Lifecycle state.
    pub state: WorkflowState,
    /// Task identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Task description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Planned outline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<String>,
    /// Mermaid graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<String>,
    /// Task output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Failure message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Final result, only on ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<WorkflowCompletion>,
}
