//! Adapters for persistence, operating-system services, and remote transports.
//!
//! @author kongweiguang

pub mod agents;
pub mod command_history;
pub mod commands;
pub mod configuration;
pub mod configuration_changes;
pub mod configuration_sync;
pub mod connections;
pub mod data_migration;
pub mod diagnostics;
pub mod files;
pub mod local_pty;
pub mod operations;
pub mod platform;
pub(crate) mod process_tree;
pub mod session_logs;
pub mod sessions;
pub mod shell_integration;
pub mod storage;
pub mod vault;
pub mod workspace;
