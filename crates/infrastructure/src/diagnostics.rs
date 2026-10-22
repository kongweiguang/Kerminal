//! 诊断端口的本机系统、配置、会话和 ZIP 归档适配器。
//!
//! @author kongweiguang

mod archive;
mod context;
pub(crate) mod gpu;
mod health;

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    diagnostics::{
        DiagnosticBundle, DiagnosticBundleArchive, DiagnosticBundleReport,
        DiagnosticContextSnapshot, DiagnosticsContextProvider, RuntimeHealthCollector,
        RuntimeHealthSnapshot,
    },
    operations::OperationsCommandExecutor,
    preferences::PreferencesService,
    sessions::SessionService,
};

use crate::{command_history::SqliteCommandHistoryRepository, storage::StorageCoordinator};

const APP_LOG_FILE_NAME: &str = "kerminal.log";
const APP_LOG_MAX_FILE_SIZE_BYTES: u64 = 1_000_000;
const APP_LOG_ROTATION_KEEP_FILES: usize = 5;

/// 将实际数据根和同进程共享的 command store 注入诊断 adapter。
#[derive(Clone)]
pub struct LocalDiagnosticsAdapter {
    pub(super) storage: StorageCoordinator,
    pub(super) command_history: Arc<SqliteCommandHistoryRepository>,
    pub(super) preferences: PreferencesService,
    pub(super) sessions: Arc<dyn SessionService>,
    pub(super) operations: Arc<dyn OperationsCommandExecutor>,
}

impl LocalDiagnosticsAdapter {
    /// 共用组合根已装配的业务端口，避免诊断另外启动平台命令或持有重复服务。
    pub fn new(
        storage: StorageCoordinator,
        command_history: Arc<SqliteCommandHistoryRepository>,
        preferences: PreferencesService,
        sessions: Arc<dyn SessionService>,
        operations: Arc<dyn OperationsCommandExecutor>,
    ) -> Self {
        Self {
            storage,
            command_history,
            preferences,
            sessions,
            operations,
        }
    }
}

#[async_trait]
impl DiagnosticsContextProvider for LocalDiagnosticsAdapter {
    /// 通过既有用例端口读取白名单状态；原始 TOML、数据库行与终端输出不进入上下文。
    async fn collect(&self) -> AppResult<DiagnosticContextSnapshot> {
        context::collect(self).await
    }
}

#[async_trait]
impl RuntimeHealthCollector for LocalDiagnosticsAdapter {
    /// 将 sysinfo 放入 blocking pool，并复用运维执行器获取 GPU 数据，避免诊断另管子进程。
    async fn collect(&self) -> AppResult<RuntimeHealthSnapshot> {
        health::collect(self).await
    }
}

#[async_trait]
impl DiagnosticBundleArchive for LocalDiagnosticsAdapter {
    /// 将报告归档到固定 diagnostics 目录；接收方无权指定绝对路径或覆盖目标文件。
    async fn create(&self, report: DiagnosticBundleReport) -> AppResult<DiagnosticBundle> {
        archive::create(self, report).await
    }
}
