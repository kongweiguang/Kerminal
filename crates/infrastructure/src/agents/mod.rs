//! Agent workspace 的文件仓储适配；生命周期和会话编排属于 application。
//!
//! @author kongweiguang

mod launcher_availability;
mod store;
mod workspace_repository;

pub use launcher_availability::PathAgentLauncherInstallationProbe;
pub use workspace_repository::FileAgentWorkspaceRepository;
