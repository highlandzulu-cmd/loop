//! Execution environment implementations.

mod host;

pub use host::HostExecutionEnv;

pub(crate) use host::exec_command;

#[cfg(target_os = "macos")]
mod process_tree;

mod session;
pub use session::SessionExecutionEnv;
