//! Execution environment implementations.

mod host;

pub use host::HostExecutionEnv;

pub(crate) use host::exec_command;
