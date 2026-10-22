//! Docker 与 Podman 交互终端的 SessionTransport 适配。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, RemoteConnectionLease, RemoteConnectionService, TerminalOpenRequest,
    },
    sessions::{
        SessionOpenCleanupOwner, SessionTransport, SessionTransportFactory,
        SessionTransportOpenError,
    },
};
use kerminal_domain::{
    files::ContainerRuntime,
    sessions::{ContainerSessionTarget, LocalSessionTarget, SessionTarget, TerminalSize},
};
use tokio_util::sync::CancellationToken;

use crate::local_pty::LocalPtySessionTransportFactory;

const LEASE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// 容器 SSH PTY 创建失败后交由 SessionManager 重试关闭的独立 lease。
struct ContainerLeaseCleanupOwner {
    lease: RemoteConnectionLease,
}

#[async_trait]
impl SessionOpenCleanupOwner for ContainerLeaseCleanupOwner {
    /// 超时不会丢弃连接句柄，SessionManager 可在统一 shutdown deadline 前继续清理。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.lease
            .close_until(deadline, "container session transport open failed")
            .await
    }
}

/// 把容器目标路由到本地 PTY 或 SSH PTY，不另建终端 registry。
pub struct ContainerSessionTransportFactory {
    local: LocalPtySessionTransportFactory,
    connections: Arc<dyn RemoteConnectionService>,
}

impl ContainerSessionTransportFactory {
    /// 注入共用 SSH service；本地目标通过无状态 PTY provider 启动容器 CLI。
    pub fn new(connections: Arc<dyn RemoteConnectionService>) -> Self {
        Self {
            local: LocalPtySessionTransportFactory::new(),
            connections,
        }
    }
}

#[async_trait]
impl SessionTransportFactory for ContainerSessionTransportFactory {
    /// 仅接受 ContainerSessionTarget；失败的 SSH lease 作为 cleanup owner 交给 SessionManager。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Container(target) = target else {
            return Err(AppError::unsupported("容器终端 provider 不支持此会话目标").into());
        };
        validate_container_target(target)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("容器终端创建已取消").into());
        }

        match target.host_id.as_ref() {
            None => {
                let local_target = SessionTarget::Local(LocalSessionTarget {
                    program: runtime_program(target.runtime).into(),
                    args: runtime_arguments(target),
                    working_directory: None,
                    environment: BTreeMap::new(),
                });
                self.local.open(&local_target, size, cancellation).await
            }
            Some(host_id) => self.open_remote(target, host_id, size, cancellation).await,
        }
    }
}

impl ContainerSessionTransportFactory {
    /// 每次远端 attach 持有独立 lease；失败时关闭该 lease 并保留两侧错误。
    ///
    /// @author kongweiguang
    async fn open_remote(
        &self,
        target: &ContainerSessionTarget,
        host_id: &kerminal_domain::identity::HostId,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let arguments = runtime_arguments(target)
            .into_iter()
            .map(|argument| {
                argument
                    .into_string()
                    .map_err(|_| AppError::invalid("远程容器命令包含非 UTF-8 参数"))
            })
            .collect::<AppResult<Vec<_>>>()?;
        let lease = self
            .connections
            .connect(
                host_id.clone(),
                ConnectionIntent::Terminal,
                cancellation.clone(),
            )
            .await?;
        if cancellation.is_cancelled() {
            return Err(close_failed_lease(lease, AppError::cancelled("容器终端创建已取消")).await);
        }
        match lease
            .open_terminal(
                TerminalOpenRequest {
                    term: "xterm-256color".to_owned(),
                    size,
                    environment: BTreeMap::new(),
                    command: None,
                    program: Some(runtime_program(target.runtime).to_owned()),
                    args: arguments,
                    working_directory: None,
                },
                cancellation.clone(),
            )
            .await
        {
            Ok(transport) => Ok(transport),
            Err(error) => Err(close_failed_lease(lease, error).await),
        }
    }
}

/// 固定容器标识和命令约束，避免 CLI 把用户值解释成选项或空命令。
fn validate_container_target(target: &ContainerSessionTarget) -> AppResult<()> {
    let mut chars = target.container_id.chars();
    if !chars
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        || target.container_id.len() > 128
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(AppError::invalid("容器标识格式无效"));
    }
    if target.command.is_empty()
        || target.command.iter().any(|argument| {
            argument.is_empty() || argument.to_string_lossy().chars().any(char::is_control)
        })
    {
        return Err(AppError::invalid("容器 exec 命令无效"));
    }
    Ok(())
}

/// 返回固定 runtime executable；用户输入只进入后续独立 argv 元素。
fn runtime_program(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::Podman => "podman",
    }
}

/// 构造 `exec -it` argv；命令项不拼接成 shell 文本。
fn runtime_arguments(target: &ContainerSessionTarget) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("exec"),
        OsString::from("--interactive"),
        OsString::from("--tty"),
        OsString::from(&target.container_id),
    ];
    arguments.extend(target.command.iter().cloned());
    arguments
}

/// 首次清理失败时保留 lease owner，避免把活动 SSH peer 仅塞进错误链后丢弃。
///
/// @author kongweiguang
async fn close_failed_lease(
    lease: RemoteConnectionLease,
    error: AppError,
) -> SessionTransportOpenError {
    match lease
        .close_until(
            tokio::time::Instant::now() + LEASE_CLEANUP_TIMEOUT,
            "容器终端创建失败",
        )
        .await
    {
        Ok(()) => SessionTransportOpenError::new(error),
        Err(cleanup) => SessionTransportOpenError::with_cleanup(
            error.with_source(cleanup),
            Arc::new(ContainerLeaseCleanupOwner { lease }),
        ),
    }
}
