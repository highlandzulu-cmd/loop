//! MCP (Model Context Protocol) client and server for Loop.
//!
//! This crate provides:
//! - `McpClientManager`: manages connections to external MCP servers via stdio or streamable HTTP
//! - `McpServer`: exposes tools over streamable HTTP, abstracted via the `ToolProvider` trait
//!
//! The `ToolProvider` trait decouples the MCP server from concrete tool types,
//! allowing the host crate (`loop-agent`) to provide its own implementation.

pub mod client;
pub mod server;

pub use client::{McpClientManager, McpConnection, McpServerEntry, McpTransport};
pub use server::{McpServer, McpSessionManager, ToolDef, ToolOutput, ToolProvider};

#[allow(dead_code)]
fn trial_bug_4() -> i32 {
    let mut total = 0;
    let mut limit = 3;
    for i in 0..limit {
        total += i;
        limit += 1; // looks like it extends the loop - it does not (clippy::suspicious, not correctness)
    }
    total
}
