//! Runtime-only MCP adapter over application use cases.
//!
//! @author kongweiguang

mod agent_tools;
mod agent_wire;
mod config_tools;
mod credentials_tools;
mod files_tools;
mod guides_tools;
mod history_tools;
mod port_forward_tools;
mod protocol;
mod runtime_control;
mod server;
mod services;
mod ssh_tools;
mod terminal_tools;

pub use runtime_control::McpRuntimeController;
pub use server::{McpServer, McpServerConfig, McpServerHandle};
pub use services::{McpServices, McpServicesValidationError};
