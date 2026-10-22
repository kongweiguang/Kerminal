//! Application use cases and ports shared by desktop and MCP adapters.
//!
//! @author kongweiguang

pub mod agents;
pub mod commands;
pub mod configuration_changes;
pub mod configuration_directory;
pub mod configuration_sync;
pub mod connections;
pub mod data_migration;
pub mod diagnostics;
pub mod error;
pub mod executor;
pub mod files;
pub mod local_shells;
pub mod mcp_runtime;
pub mod operations;
pub mod platform;
pub mod preferences;
pub mod sessions;
pub mod transfers;
pub mod updates;
pub mod workspace;

pub use configuration_changes::ConfigurationChangeService;
pub use configuration_directory::{
    ConfigurationDirectoryKind, ConfigurationDirectoryService, OpenConfigurationDirectoryResult,
};
pub use configuration_sync::ConfigurationSyncService;
pub use error::{AppError, AppResult, PublicErrorCode};
pub use executor::ApplicationTaskExecutor;
pub use preferences::{PreferencesService, ProfileService};
pub use sessions::SessionService;
pub use updates::{
    DownloadedUpdate, DownloadedUpdateId, UpdateCandidate, UpdateCandidateId,
    UpdateInstallDisposition, UpdateInstallResult, UpdatePort, UpdateProgress, UpdateService,
};
