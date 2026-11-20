//! 单个插件进程的启动、握手、日志与进程树回收。
//! @author kongweiguang

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use kerminal_application::{AppError, AppResult};
use kerminal_domain::plugins::{PluginId, PluginVersion};
use kerminal_plugin_protocol::{
    InitializeParams, InitializeResult, PROTOCOL_VERSION, PluginIdentity,
};
use serde_json::json;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
    time::{Instant as TokioInstant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::rpc::{RouterSlot, RpcConnection};
use crate::process_tree::{
    ProcessTreeGuard,
    spawn::{ManagedTokioChild, ProcessSpawnOptions},
};

const LOG_LIMIT: u64 = 5 * 1024 * 1024;

/// 路径和版本来自冻结的启动快照，不允许插件在请求中指定可执行文件。
#[derive(Clone, Debug)]
pub struct PluginProcessSpec {
    pub id: PluginId,
    pub version: PluginVersion,
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub data_directory: PathBuf,
    pub log_directory: PathBuf,
    pub locale: String,
    pub theme: String,
    pub host_api: String,
}

/// 持有子进程与整个进程树，直到 wait 明确完成才允许释放。
pub(crate) struct PluginProcess {
    pub(crate) child: ManagedTokioChild,
    pub(crate) rpc: RpcConnection,
    pub(crate) log: Option<JoinHandle<std::io::Result<()>>>,
    child_reaped: bool,
}

/// 握手失败仍带走已启动进程的所有权，超时不能靠丢弃句柄冒充回收。
pub(crate) struct StartFailure {
    pub(crate) error: AppError,
    pub(crate) owner: Option<FailedProcess>,
}

/// 管道未完全取得时仍保留 child，以便后续截止时间内继续清理。
pub(crate) enum FailedProcess {
    Child(ManagedTokioChild),
    Process(PluginProcess),
}

impl From<AppError> for StartFailure {
    /// 启动前的错误没有系统进程，不需要伪造清理对象。
    fn from(error: AppError) -> Self {
        Self { error, owner: None }
    }
}

impl FailedProcess {
    /// 清理失败时保留本对象，监督器可在明确新截止时间后继续 wait。
    pub(crate) async fn force_stop(&mut self, deadline: Instant) -> AppResult<()> {
        match self {
            Self::Child(child) => {
                child
                    .terminate_tree_until(TokioInstant::from_std(deadline))
                    .await
                    .map_err(|error| {
                        AppError::infrastructure_with_source("终止插件进程树", error)
                    })?;
                timeout_at(TokioInstant::from_std(deadline), child.wait())
                    .await
                    .map_err(|_| AppError::timeout("回收插件进程"))?
                    .map_err(|error| AppError::infrastructure_with_source("回收插件进程", error))?;
                Ok(())
            }
            Self::Process(process) => process.force_stop(deadline).await,
        }
    }
}

impl PluginProcessSpec {
    /// 所有路径必须由可信组合根解析，防止不同工作目录改变启动目标。
    pub fn validate(&self) -> AppResult<()> {
        if !self.executable.is_absolute()
            || !self.data_directory.is_absolute()
            || !self.log_directory.is_absolute()
            || self.locale.is_empty()
            || self.theme.is_empty()
            || self.host_api.is_empty()
        {
            return Err(AppError::invalid("插件进程启动配置"));
        }
        Ok(())
    }
}

impl PluginProcess {
    /// 白名单保留运行时依赖的系统路径与必要用户变量；启动即登记所有权，避免握手失败遗留进程。
    pub(crate) async fn spawn(
        spec: &PluginProcessSpec,
        generation: u64,
        router: RouterSlot,
    ) -> Result<Self, StartFailure> {
        spec.validate()?;
        fs::create_dir_all(&spec.data_directory)
            .await
            .map_err(|error| AppError::infrastructure_with_source("创建插件数据目录", error))?;
        fs::create_dir_all(&spec.log_directory)
            .await
            .map_err(|error| AppError::infrastructure_with_source("创建插件日志目录", error))?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .current_dir(&spec.data_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in ["PATH", "HOME", "USERPROFILE", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        // Winsock 用 SystemRoot 展开系统网络提供程序路径。只补这一项，维持子进程环境隔离。
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command
            .env("KERMINAL_PLUGIN_ID", spec.id.as_str())
            .env("KERMINAL_PLUGIN_DATA", &spec.data_directory);
        let mut child = ProcessTreeGuard::spawn_tokio(
            command,
            ProcessSpawnOptions {
                kill_on_drop: true,
                windows_creation_flags: 0x08000000,
            },
        )
        .map_err(|error| AppError::infrastructure_with_source("启动插件进程", error))?;
        let pipes = (
            child.stdin().take(),
            child.stdout().take(),
            child.stderr().take(),
        );
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            return Err(StartFailure {
                error: AppError::infrastructure("插件进程管道不可用"),
                owner: Some(FailedProcess::Child(child)),
            });
        };
        let rpc = RpcConnection::new(spec.id.clone(), generation, stdout, stdin, router);
        let log_path = spec.log_directory.join(format!("{}.log", spec.id.as_str()));
        let log = tokio::spawn(write_log(stderr, log_path));
        Ok(Self {
            child,
            rpc,
            log: Some(log),
            child_reaped: false,
        })
    }

    /// 握手只借用已有进程，取消等待时监督器仍可按关闭截止时间强杀和 wait。
    pub(crate) async fn handshake(
        &self,
        spec: &PluginProcessSpec,
        generation: u64,
        deadline: Instant,
    ) -> AppResult<()> {
        let request = InitializeParams {
            protocol: PROTOCOL_VERSION,
            host_api: spec.host_api.clone(),
            plugin: PluginIdentity {
                id: spec.id.to_string(),
                version: spec.version.to_string(),
            },
            locale: spec.locale.clone(),
            theme: spec.theme.clone(),
            data_dir: spec.data_directory.to_string_lossy().into_owned(),
            generation,
        };
        let handshake_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
        let params =
            serde_json::to_value(&request).map_err(|_| AppError::invalid("插件握手参数"))?;
        let value = self
            .rpc
            .request(
                "initialize",
                params,
                handshake_deadline,
                CancellationToken::new(),
            )
            .await?;
        let result: InitializeResult =
            serde_json::from_value(value).map_err(|_| AppError::invalid("插件握手回复"))?;
        result
            .validate_for(&request)
            .map_err(|_| AppError::invalid("插件握手身份不匹配"))?;
        Ok(())
    }

    /// 首先请求正常退出，宽限期最多两秒；之后强杀并在同一截止时间内 wait。
    pub(crate) async fn stop(&mut self, deadline: Instant) -> AppResult<()> {
        let grace = deadline.min(Instant::now() + Duration::from_secs(2));
        let _ = self
            .rpc
            .request("shutdown", json!({}), grace, CancellationToken::new())
            .await;
        let _ = timeout_at(TokioInstant::from_std(grace), self.child.wait()).await;
        self.force_stop(deadline).await
    }

    /// 强杀与 wait 均使用调用者提供的绝对截止时间，失败时调用方继续持有 owner。
    pub(crate) async fn force_stop(&mut self, deadline: Instant) -> AppResult<()> {
        self.rpc.close();
        if !self.child_reaped {
            self.child
                .terminate_tree_until(TokioInstant::from_std(deadline))
                .await
                .map_err(|error| AppError::infrastructure_with_source("终止插件进程树", error))?;
            timeout_at(TokioInstant::from_std(deadline), self.child.wait())
                .await
                .map_err(|_| AppError::timeout("回收插件进程"))?
                .map_err(|error| AppError::infrastructure_with_source("回收插件进程", error))?;
            self.child_reaped = true;
        }
        self.finish_log(deadline).await?;
        self.rpc.close_until(deadline).await
    }

    /// stderr 写入错误不能冒充日志已保存，调用者在超时后仍持有句柄。
    async fn finish_log(&mut self, deadline: Instant) -> AppResult<()> {
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        let result = timeout_at(TokioInstant::from_std(deadline), log)
            .await
            .map_err(|_| AppError::timeout("回收插件日志"))?;
        self.log = None;
        result
            .map_err(|_| AppError::infrastructure("插件日志任务终止"))?
            .map_err(|error| AppError::infrastructure_with_source("写入插件日志", error))
    }
}

/// 按字节滚动三个文件，单次 stderr 写入再大也不能使任何文件超过 5 MiB。
async fn write_log<R: AsyncRead + Unpin>(mut stderr: R, path: PathBuf) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let mut size = file.metadata().await?.len();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let mut offset = 0;
        while offset < read {
            if size >= LOG_LIMIT {
                file.flush().await?;
                drop(file);
                rotate_logs(&path).await?;
                file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await?;
                size = 0;
            }
            let remaining = (LOG_LIMIT - size) as usize;
            let length = remaining.min(read - offset);
            file.write_all(&buffer[offset..offset + length]).await?;
            offset += length;
            size += length as u64;
        }
    }
    file.flush().await
}

/// 先移除最旧一份再顺序改名，缺失文件表示首次滚动而非失败。
async fn rotate_logs(path: &Path) -> std::io::Result<()> {
    let first = path.with_extension("log.1");
    let second = path.with_extension("log.2");
    remove_if_exists(&second).await?;
    rename_if_exists(&first, &second).await?;
    rename_if_exists(path, &first).await
}

/// 只忽略目标确实不存在的情况，其他文件错误交给进程监督器处理。
async fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// 重启后的首次滚动可能没有旧文件，但不能掩盖权限等错误。
async fn rename_if_exists(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::rename(from, to).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
