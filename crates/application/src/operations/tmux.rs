//! tmux 命令用例和唯一终端会话的 attach 编排。
//!
//! 程序与参数始终分开传递；SSH attach 通过可信 connection lease 建立 PTY transport。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_domain::{
    operations::{
        TmuxAttachPlan, TmuxMutationReceipt, TmuxPane, TmuxPaneCapture, TmuxSession, TmuxTarget,
        TmuxWindow,
    },
    sessions::{
        LocalSessionTarget, SessionInfo, SessionOwner, SessionTarget, SshSessionTarget,
        TerminalSize,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, LeasedSessionTransportControl, RemoteConnectionLease,
        RemoteConnectionService, TerminalOpenRequest,
    },
    preferences::{ProfileRepository, VersionedProfile},
    sessions::{
        CreateSessionRequest, SessionOpenCleanupOwner, SessionService, SessionTransport,
        SessionTransportFactory, SessionTransportOpenError,
    },
};

use super::{
    CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, OperationsCommandExecutor,
    execute_with_deadline,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);
const OUTPUT_LIMIT: usize = 512 * 1024;
const CAPTURE_OUTPUT_LIMIT: usize = 1024 * 1024;
const FIELD_SEPARATOR: char = '\u{1f}';

/// 可用性探测保持连接错误与目标未安装 tmux 的结果区分。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxCapability {
    pub target_ref: String,
    pub available: bool,
    pub version: Option<String>,
    pub reason: Option<String>,
}

/// 创建请求不接受 shell 文本，名称和目录作为独立 argv 数据传递。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxCreateRequest {
    pub target: TmuxTarget,
    pub name: String,
    pub working_directory: Option<String>,
}

/// tmux 管理用例；同一 SessionService 负责所有 attach 终端的生命周期。
#[derive(Clone)]
pub struct TmuxService {
    commands: Arc<dyn OperationsCommandExecutor>,
    sessions: Arc<dyn SessionService>,
    connections: Arc<dyn RemoteConnectionService>,
    profiles: Arc<dyn ProfileRepository>,
}

/// 单次 SSH attach 的 transport factory；SessionService 先持有容量和清理所有权再调用它。
struct TmuxSshSessionFactory {
    connections: Arc<dyn RemoteConnectionService>,
    host_id: kerminal_domain::identity::HostId,
    program: String,
    arguments: Vec<String>,
}

/// SSH terminal 创建失败后仍由 SessionManager 重试关闭的本次独立 lease。
struct TmuxLeaseCleanupOwner {
    lease: RemoteConnectionLease,
}

#[async_trait]
impl SessionOpenCleanupOwner for TmuxLeaseCleanupOwner {
    /// 只关闭本次 attach 持有的 peer；超时后保留 lease 供 SessionManager 再次清理。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.lease
            .close_until(deadline, "tmux session transport open failed")
            .await
    }
}

#[async_trait]
impl SessionTransportFactory for TmuxSshSessionFactory {
    /// 只为本次 attach 建立 SSH lease；失败清理超时时把 owner 留给 SessionManager 重试。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Ssh(target) = target else {
            return Err(AppError::invalid("tmux SSH transport 收到非 SSH 会话目标").into());
        };
        if target.host_id != self.host_id || target.command.is_some() {
            return Err(AppError::invalid("tmux SSH transport 目标与创建请求不一致").into());
        }
        let lease = self
            .connections
            .connect(
                self.host_id.clone(),
                ConnectionIntent::Terminal,
                cancel.clone(),
            )
            .await?;
        if cancel.is_cancelled() {
            return Err(
                close_failed_tmux_lease(lease, AppError::cancelled("tmux 终端创建已取消")).await,
            );
        }
        let mut transport = match lease
            .open_terminal(
                TerminalOpenRequest {
                    term: "xterm-256color".to_owned(),
                    size,
                    environment: BTreeMap::new(),
                    command: None,
                    program: Some(self.program.clone()),
                    args: self.arguments.clone(),
                    working_directory: target
                        .working_directory
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                },
                cancel,
            )
            .await
        {
            Ok(transport) => transport,
            Err(error) => return Err(close_failed_tmux_lease(lease, error).await),
        };
        let channel = Arc::clone(&transport.control);
        transport.control = Arc::new(LeasedSessionTransportControl::new(
            channel,
            lease,
            "tmux session transport closed",
        ));
        Ok(transport)
    }
}

/// 尝试在首次预算内关闭失败的 lease；清理失败时将唯一 owner 交还给 SessionManager。
///
/// @author kongweiguang
async fn close_failed_tmux_lease(
    lease: RemoteConnectionLease,
    error: AppError,
) -> SessionTransportOpenError {
    match lease
        .close_until(
            tokio::time::Instant::now() + Duration::from_secs(5),
            "tmux session transport open failed",
        )
        .await
    {
        Ok(()) => SessionTransportOpenError::new(error),
        Err(cleanup) => SessionTransportOpenError::with_cleanup(
            error.with_source(cleanup),
            Arc::new(TmuxLeaseCleanupOwner { lease }),
        ),
    }
}

impl TmuxService {
    /// 注入命令、连接、profile 与 session ports，禁止此模块另建配置或终端 registry。
    pub fn new(
        commands: Arc<dyn OperationsCommandExecutor>,
        sessions: Arc<dyn SessionService>,
        connections: Arc<dyn RemoteConnectionService>,
        profiles: Arc<dyn ProfileRepository>,
    ) -> Self {
        Self {
            commands,
            sessions,
            connections,
            profiles,
        }
    }

    /// 以固定 C locale 探测版本；命令不存在作为不可用能力返回，其它连接错误仍传播。
    pub async fn probe(
        &self,
        target: TmuxTarget,
        cancel: CancellationToken,
    ) -> AppResult<TmuxCapability> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        let output = self.execute(&target, vec!["-V".to_owned()], cancel).await?;
        Ok(capability_from_output(&output, &target))
    }

    /// 列出指定 host/socket 下的 session；无 server 按 tmux 语义返回空集合。
    pub async fn list_sessions(
        &self,
        target: TmuxTarget,
        cancel: CancellationToken,
    ) -> AppResult<Vec<TmuxSession>> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        let format = session_format();
        let output = self
            .execute(
                &target,
                vec!["list-sessions".to_owned(), "-F".to_owned(), format],
                cancel,
            )
            .await?;
        if output.exit_code != Some(0) {
            if no_server_running(&output.stderr) {
                return Ok(Vec::new());
            }
            return Err(AppError::remote_exit(output.exit_code));
        }
        if output.truncated {
            return Err(AppError::infrastructure("tmux session 列表超过读取上限"));
        }
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(|line| parse_session(line, &target))
            .collect()
    }

    /// 创建后以同一 target 的列表回读确认 session 仍存活；回读失败保留可重试回执，不重放创建。
    ///
    /// @author kongweiguang
    pub async fn create_session(
        &self,
        request: TmuxCreateRequest,
        cancel: CancellationToken,
    ) -> AppResult<TmuxMutationReceipt> {
        request
            .target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_name(&request.name)?;
        if request
            .working_directory
            .as_deref()
            .is_some_and(|path| path.is_empty() || path.chars().any(char::is_control))
        {
            return Err(AppError::invalid("tmux 工作目录无效"));
        }
        let mut arguments = vec![
            "new-session".to_owned(),
            "-d".to_owned(),
            "-P".to_owned(),
            "-F".to_owned(),
            session_format(),
            "-s".to_owned(),
            request.name.clone(),
        ];
        if let Some(path) = request.working_directory {
            arguments.extend(["-c".to_owned(), path]);
        }
        let output = self
            .execute(&request.target, arguments, cancel.clone())
            .await?;
        ensure_success(&output)?;
        if output.truncated {
            return Err(AppError::infrastructure("tmux 创建输出超过读取上限"));
        }
        let created = output
            .stdout
            .split(|byte| *byte == b'\n')
            .find(|line| !line.iter().all(u8::is_ascii_whitespace))
            .ok_or_else(|| AppError::infrastructure("tmux 未返回新 session 摘要"))?;
        let created = parse_session(created, &request.target)?;
        let refreshed = self.list_sessions(request.target, cancel).await;
        receipt_after_create_refresh(created, refreshed)
    }

    /// 重命名成功后刷新目标摘要；失败时保留已确认的 mutation 结果并要求单独刷新。
    pub async fn rename_session(
        &self,
        target: TmuxTarget,
        session_id: String,
        new_name: String,
        cancel: CancellationToken,
    ) -> AppResult<TmuxMutationReceipt> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        validate_session_name(&new_name)?;
        let output = self
            .execute(
                &target,
                vec![
                    "rename-session".to_owned(),
                    "-t".to_owned(),
                    session_id.clone(),
                    new_name.clone(),
                ],
                cancel.clone(),
            )
            .await?;
        ensure_success(&output)?;
        let fallback = TmuxSession {
            id: session_id.clone(),
            name: new_name,
            attached: false,
            clients: 0,
            windows: 0,
            current_path: None,
            created_at: None,
            activity_at: None,
            target_ref: target_reference(&target),
        };
        let refreshed = self.list_sessions(target, cancel).await;
        match refreshed {
            Ok(sessions) => Ok(TmuxMutationReceipt {
                session: sessions
                    .into_iter()
                    .find(|session| session.id == session_id)
                    .unwrap_or(fallback),
                refresh_required: false,
            }),
            Err(_) => Ok(TmuxMutationReceipt {
                session: fallback,
                refresh_required: true,
            }),
        }
    }

    /// 仅将确认成功映射为 true、明确缺失映射为 false，其它远端失败保持可见错误。
    pub async fn kill_session(
        &self,
        target: TmuxTarget,
        session_id: String,
        cancel: CancellationToken,
    ) -> AppResult<bool> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        let output = self
            .execute(
                &target,
                vec!["kill-session".to_owned(), "-t".to_owned(), session_id],
                cancel,
            )
            .await?;
        classify_kill_result(&output)
    }

    /// 从指定 tmux session 分离客户端，不会杀死 server 或其它客户端。
    pub async fn detach_session(
        &self,
        target: TmuxTarget,
        session_id: String,
        cancel: CancellationToken,
    ) -> AppResult<()> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        let output = self
            .execute(
                &target,
                vec!["detach-client".to_owned(), "-s".to_owned(), session_id],
                cancel,
            )
            .await?;
        ensure_success(&output)
    }

    /// 返回指定 session 的窗口快照；窗口字段按固定格式解析，不暴露任意 tmux format 输入。
    pub async fn list_windows(
        &self,
        target: TmuxTarget,
        session_id: String,
        cancel: CancellationToken,
    ) -> AppResult<Vec<TmuxWindow>> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        let output = self
            .execute(
                &target,
                vec![
                    "list-windows".to_owned(),
                    "-t".to_owned(),
                    session_id,
                    "-F".to_owned(),
                    window_format(),
                ],
                cancel,
            )
            .await?;
        if output.exit_code != Some(0) {
            return Err(AppError::remote_exit(output.exit_code));
        }
        if output.truncated {
            return Err(AppError::infrastructure("tmux window 列表超过读取上限"));
        }
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(|line| parse_window(line, &target))
            .collect()
    }

    /// 返回指定 session/window/pane 范围下的 pane 快照，目标字符串仍作为单个 argv。
    pub async fn list_panes(
        &self,
        target: TmuxTarget,
        target_id: String,
        cancel: CancellationToken,
    ) -> AppResult<Vec<TmuxPane>> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&target_id)?;
        let output = self
            .execute(
                &target,
                vec![
                    "list-panes".to_owned(),
                    "-t".to_owned(),
                    target_id,
                    "-F".to_owned(),
                    pane_format(),
                ],
                cancel,
            )
            .await?;
        if output.exit_code != Some(0) {
            return Err(AppError::remote_exit(output.exit_code));
        }
        if output.truncated {
            return Err(AppError::infrastructure("tmux pane 列表超过读取上限"));
        }
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(|line| parse_pane(line, &target))
            .collect()
    }

    /// 捕获 pane 有界历史；不启用 escape 输出，返回纯文本快照供调用方按契约脱敏。
    pub async fn capture_pane(
        &self,
        target: TmuxTarget,
        pane_id: String,
        lines: u32,
        cancel: CancellationToken,
    ) -> AppResult<TmuxPaneCapture> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&pane_id)?;
        if !(1..=1000).contains(&lines) {
            return Err(AppError::invalid("tmux pane 行数必须在 1 到 1000 之间"));
        }
        let output = self
            .execute_with_limit(
                &target,
                vec![
                    "capture-pane".to_owned(),
                    "-p".to_owned(),
                    "-J".to_owned(),
                    "-S".to_owned(),
                    format!("-{lines}"),
                    "-t".to_owned(),
                    pane_id.clone(),
                ],
                CAPTURE_OUTPUT_LIMIT,
                cancel,
            )
            .await?;
        ensure_success(&output)?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let captured_lines = text.lines().count().min(u32::MAX as usize) as u32;
        Ok(TmuxPaneCapture {
            pane_id,
            text,
            captured_lines,
            truncated: output.truncated,
        })
    }

    /// 生成 attach 执行规格；profile 只解析本地启动上下文，不创建 UI pane 或 session。
    pub async fn attach_plan(
        &self,
        target: TmuxTarget,
        session_id: String,
        session_name: Option<String>,
        working_directory: Option<String>,
        cancel: CancellationToken,
    ) -> AppResult<TmuxAttachPlan> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        let name = session_name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| session_id.clone());
        validate_session_name(&name)?;
        if working_directory
            .as_deref()
            .is_some_and(|path| path.is_empty() || path.chars().any(char::is_control))
        {
            return Err(AppError::invalid("tmux 工作目录无效"));
        }
        let profile = self.resolve_local_profile(&target, &cancel).await?;
        let working_directory =
            working_directory.or_else(|| profile.and_then(|profile| profile.profile.cwd));
        let (program, mut arguments) = target_program_and_scope(&target);
        arguments.extend([
            "attach-session".to_owned(),
            "-t".to_owned(),
            session_id.clone(),
        ]);
        Ok(TmuxAttachPlan {
            target,
            session_id,
            session_name: name,
            program,
            arguments,
            working_directory,
        })
    }

    /// 建立 tmux attach 终端；本地 profile 仅提供 cwd/env，SSH 目标始终使用远端自身配置。
    pub async fn attach_session(
        &self,
        target: TmuxTarget,
        session_id: String,
        owner: SessionOwner,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> AppResult<SessionInfo> {
        target
            .validate()
            .map_err(|_| AppError::invalid("tmux 目标无效"))?;
        validate_session_reference(&session_id)?;
        let profile = self.resolve_local_profile(&target, &cancel).await?;
        let (program, mut arguments) = target_program_and_scope(&target);
        arguments.extend(["attach-session".to_owned(), "-t".to_owned(), session_id]);
        if let Some(host_id) = target.host_id.as_ref() {
            let factory: Arc<dyn SessionTransportFactory> = Arc::new(TmuxSshSessionFactory {
                connections: self.connections.clone(),
                host_id: host_id.clone(),
                program,
                arguments,
            });
            self.sessions
                .create_with_transport_factory(
                    CreateSessionRequest {
                        target: SessionTarget::Ssh(SshSessionTarget {
                            host_id: host_id.clone(),
                            command: None,
                            working_directory: None,
                        }),
                        owner,
                        size,
                    },
                    factory,
                    cancel,
                )
                .await
        } else {
            self.sessions
                .create_cancellable(
                    CreateSessionRequest {
                        target: SessionTarget::Local(LocalSessionTarget {
                            program: program.into(),
                            args: arguments.into_iter().map(Into::into).collect(),
                            working_directory: profile
                                .as_ref()
                                .and_then(|profile| profile.profile.cwd.as_deref())
                                .map(PathBuf::from),
                            environment: profile
                                .map(|profile| {
                                    profile
                                        .profile
                                        .env
                                        .into_iter()
                                        .map(|(key, value)| (key.into(), value.into()))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        }),
                        owner,
                        size,
                    },
                    cancel,
                )
                .await
        }
    }

    /// 按显式 profile ID 读取本地启动上下文；取消和缺失配置都必须硬失败，不猜默认项。
    async fn resolve_local_profile(
        &self,
        target: &TmuxTarget,
        cancel: &CancellationToken,
    ) -> AppResult<Option<VersionedProfile>> {
        let Some(profile_id) = target.profile_id.as_deref() else {
            return Ok(None);
        };
        if target.host_id.is_some() {
            return Err(AppError::invalid("SSH tmux 目标不能引用本地 profile"));
        }
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("tmux profile 读取已取消"));
        }
        let profile = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("tmux profile 读取已取消")),
            result = self.profiles.get_profile(profile_id) => result?,
        };
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("tmux profile 读取已取消"));
        }
        profile
            .map(Some)
            .ok_or_else(|| AppError::not_found("tmux 指定的本地 profile 不存在"))
    }

    /// 为查询执行程序并注入固定 locale，避免输出解析受用户系统语言影响。
    async fn execute(
        &self,
        target: &TmuxTarget,
        arguments: Vec<String>,
        cancel: CancellationToken,
    ) -> AppResult<super::CommandExecutionResult> {
        self.execute_with_limit(target, arguments, OUTPUT_LIMIT, cancel)
            .await
    }

    /// 为普通命令与 pane 捕获复用相同 target/locale/timeout，仅允许调用方降低/放大有界输出。
    async fn execute_with_limit(
        &self,
        target: &TmuxTarget,
        arguments: Vec<String>,
        max_output_bytes: usize,
        cancel: CancellationToken,
    ) -> AppResult<super::CommandExecutionResult> {
        let (program, mut scoped) = target_program_and_scope(target);
        scoped.extend(arguments);
        let mut invocation = CommandInvocation::new(program, scoped);
        invocation
            .environment
            .insert("LC_ALL".to_owned(), "C".to_owned());
        execute_with_deadline(
            self.commands.as_ref(),
            target
                .host_id
                .clone()
                .map_or(CommandExecutionTarget::Local, CommandExecutionTarget::Host),
            invocation,
            CommandExecutionLimits {
                timeout: COMMAND_TIMEOUT,
                max_output_bytes,
            },
            cancel,
        )
        .await
    }
}

/// 只在同一 target 回读到刚创建的 session identity 后报告持久创建，避免把短命 shell 当成仍存活资源。
///
/// 列表失败仍返回原创建回执并要求单独刷新，因为写命令已确认成功且不能自动重放；明确回读为空则报未找到。
///
/// @author kongweiguang
fn receipt_after_create_refresh(
    created: TmuxSession,
    refreshed: AppResult<Vec<TmuxSession>>,
) -> AppResult<TmuxMutationReceipt> {
    match refreshed {
        Ok(sessions) => {
            let session = sessions
                .into_iter()
                .find(|session| {
                    session.id == created.id && session.target_ref == created.target_ref
                })
                .ok_or_else(|| {
                    AppError::not_found("tmux 创建后的 session 已退出或无法在原目标中确认")
                })?;
            Ok(TmuxMutationReceipt {
                session,
                refresh_required: false,
            })
        }
        Err(_) => Ok(TmuxMutationReceipt {
            session: created,
            refresh_required: true,
        }),
    }
}

/// 构造不含目标机命令片段的 tmux 稳定格式字符串。
fn session_format() -> String {
    [
        "#{session_id}",
        "#{session_name}",
        "#{session_attached}",
        "#{session_windows}",
        "#{session_path}",
        "#{session_created}",
        "#{session_activity}",
    ]
    .join(&FIELD_SEPARATOR.to_string())
}

/// 探测只把明确版本输出认作可用，SSH/命令执行端口错误在调用点继续作为错误传播。
fn capability_from_output(
    output: &super::CommandExecutionResult,
    target: &TmuxTarget,
) -> TmuxCapability {
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.exit_code == Some(0) && version.starts_with("tmux ") {
        TmuxCapability {
            target_ref: target_reference(target),
            available: true,
            version: Some(version),
            reason: None,
        }
    } else {
        TmuxCapability {
            target_ref: target_reference(target),
            available: false,
            version: None,
            reason: Some("目标未安装或无法启动 tmux".to_owned()),
        }
    }
}

/// tmux window 元数据只包含产品展示所需字段，并固定使用 unit separator 防止换行歧义。
fn window_format() -> String {
    [
        "#{window_id}",
        "#{window_index}",
        "#{window_name}",
        "#{window_active}",
        "#{window_panes}",
        "#{window_width}",
        "#{window_height}",
        "#{window_layout}",
        "#{window_flags}",
        "#{pane_current_path}",
    ]
    .join(&FIELD_SEPARATOR.to_string())
}

/// tmux pane 快照保持 target/session/window/pane 标识和尺寸，避免后续重查时猜目标。
fn pane_format() -> String {
    [
        "#{pane_id}",
        "#{session_id}",
        "#{window_id}",
        "#{pane_index}",
        "#{pane_title}",
        "#{pane_current_command}",
        "#{pane_current_path}",
        "#{pane_width}",
        "#{pane_height}",
        "#{pane_active}",
        "#{pane_dead}",
    ]
    .join(&FIELD_SEPARATOR.to_string())
}

/// 组合 tmux UTF-8 输出模式与 socket 作用域，避免 C locale 把控制分隔符净化成下划线。
///
/// `-u` 是客户端输出选项；保留 `LC_ALL=C` 仍可稳定解析诊断文本，tmux 元数据则继续以 UTF-8
/// 字节输出。该标志位于 `-L`/`-S` 之前，所有目标值仍保持独立 argv。
///
/// @author kongweiguang
fn target_program_and_scope(target: &TmuxTarget) -> (String, Vec<String>) {
    let mut arguments = vec!["-u".to_owned()];
    if let Some(socket_name) = &target.socket_name {
        arguments.extend(["-L".to_owned(), socket_name.clone()]);
    } else if let Some(socket_path) = &target.socket_path {
        arguments.extend(["-S".to_owned(), socket_path.clone()]);
    }
    (
        target
            .tmux_path
            .clone()
            .unwrap_or_else(|| "tmux".to_owned()),
        arguments,
    )
}

/// 将一行 unit-separator 数据校验后转成 tmux session 投影。
fn parse_session(line: &[u8], target: &TmuxTarget) -> AppResult<TmuxSession> {
    let text = std::str::from_utf8(line)
        .map_err(|_| AppError::infrastructure("tmux session 行不是有效 UTF-8"))?;
    let fields: Vec<_> = text.split(FIELD_SEPARATOR).collect();
    if fields.len() != 7 || fields[0].is_empty() || fields[1].is_empty() {
        return Err(AppError::infrastructure("tmux session 行字段不完整"));
    }
    let clients = fields[2]
        .parse::<u32>()
        .map_err(|_| AppError::infrastructure("tmux session 客户端数无效"))?;
    let windows = fields[3]
        .parse::<u32>()
        .map_err(|_| AppError::infrastructure("tmux session 窗口数无效"))?;
    let created_at = parse_optional_u64(fields[5])?;
    let activity_at = parse_optional_u64(fields[6])?;
    Ok(TmuxSession {
        id: fields[0].to_owned(),
        name: fields[1].to_owned(),
        attached: clients > 0,
        clients,
        windows,
        current_path: (!fields[4].is_empty()).then(|| fields[4].to_owned()),
        created_at,
        activity_at,
        target_ref: target_reference(target),
    })
}

/// 将窗口行校验后投影为 application DTO；格式损坏作为基础设施错误返回。
fn parse_window(line: &[u8], target: &TmuxTarget) -> AppResult<TmuxWindow> {
    let text = std::str::from_utf8(line)
        .map_err(|_| AppError::infrastructure("tmux window 行不是有效 UTF-8"))?;
    let fields: Vec<_> = text.split(FIELD_SEPARATOR).collect();
    if fields.len() != 10 || fields[0].is_empty() || fields[2].is_empty() {
        return Err(AppError::infrastructure("tmux window 行字段不完整"));
    }
    Ok(TmuxWindow {
        id: fields[0].to_owned(),
        index: parse_tmux_u32(fields[1])?,
        name: fields[2].to_owned(),
        active: parse_tmux_bool(fields[3])?,
        pane_count: parse_tmux_u32(fields[4])?,
        width: parse_tmux_u32(fields[5])?,
        height: parse_tmux_u32(fields[6])?,
        layout: optional_tmux_text(fields[7]),
        flags: optional_tmux_text(fields[8]),
        current_path: optional_tmux_text(fields[9]),
        target_ref: target_reference(target),
    })
}

/// 将 pane 行映射为稳定快照；解析失败不返回部分或猜测性的 pane 状态。
fn parse_pane(line: &[u8], target: &TmuxTarget) -> AppResult<TmuxPane> {
    let text = std::str::from_utf8(line)
        .map_err(|_| AppError::infrastructure("tmux pane 行不是有效 UTF-8"))?;
    let fields: Vec<_> = text.split(FIELD_SEPARATOR).collect();
    if fields.len() != 11 || fields[0].is_empty() || fields[1].is_empty() || fields[2].is_empty() {
        return Err(AppError::infrastructure("tmux pane 行字段不完整"));
    }
    Ok(TmuxPane {
        id: fields[0].to_owned(),
        index: parse_tmux_u32(fields[3])?,
        session_id: fields[1].to_owned(),
        window_id: fields[2].to_owned(),
        title: fields[4].to_owned(),
        current_command: fields[5].to_owned(),
        current_path: optional_tmux_text(fields[6]),
        width: parse_tmux_u32(fields[7])?,
        height: parse_tmux_u32(fields[8])?,
        active: parse_tmux_bool(fields[9])?,
        dead: parse_tmux_bool(fields[10])?,
        target_ref: target_reference(target),
    })
}

/// 只把 tmux 固定格式中的空文本映射为缺失值，保留其他字段原样供兼容层解释。
fn optional_tmux_text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// tmux 固定格式布尔项只接受 0/1，阻止未知输出悄悄变成 false。
fn parse_tmux_bool(value: &str) -> AppResult<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(AppError::infrastructure("tmux 布尔字段无效")),
    }
}

/// tmux 尺寸和计数统一按无符号十进制解析，不接受 locale 字符串或负数。
fn parse_tmux_u32(value: &str) -> AppResult<u32> {
    value
        .parse()
        .map_err(|_| AppError::infrastructure("tmux 数字字段无效"))
}

/// 将 tmux 空值标记映射为 None，其它非数字字段作为损坏输出失败。
fn parse_optional_u64(value: &str) -> AppResult<Option<u64>> {
    if value.is_empty() || value == "0" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| AppError::infrastructure("tmux 时间字段无效"))
}

/// 用兼容旧 wire 的 host/profile 基础标识加显式 socket scope，避免同一目标出现两种身份编码。
fn target_reference(target: &TmuxTarget) -> String {
    let base = if let Some(host_id) = target.host_id.as_ref() {
        format!("ssh:{}", host_id.as_str())
    } else {
        target.profile_id.as_deref().map_or_else(
            || "local".to_owned(),
            |profile_id| format!("local:{profile_id}"),
        )
    };
    if let Some(socket_name) = target.socket_name.as_deref() {
        format!("{base}|L:{socket_name}")
    } else if let Some(socket_path) = target.socket_path.as_deref() {
        format!("{base}|S:{socket_path}")
    } else {
        base
    }
}

/// 限制 session 名称并拒绝控制字符，降低 tmux 格式输出和输入法中的边界混淆。
fn validate_session_name(value: &str) -> AppResult<()> {
    if value.trim().is_empty() || value.chars().count() > 80 || value.chars().any(char::is_control)
    {
        Err(AppError::invalid("tmux session 名称无效"))
    } else {
        Ok(())
    }
}

/// session ID/name 作为单个 argv 传递，但仍拒绝控制字符和空值。
fn validate_session_reference(value: &str) -> AppResult<()> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        Err(AppError::invalid("tmux session 标识无效"))
    } else {
        Ok(())
    }
}

/// 仅在非零退出后的固定 C locale 诊断明确表示 server/socket 不存在时映射为空列表；权限等故障继续报错。
///
/// @author kongweiguang
fn no_server_running(stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let message = message.trim();
    message.contains("no server running")
        || message.contains("no sessions")
        || (message.starts_with("error connecting to ")
            && message.ends_with("(no such file or directory)"))
        || (message.contains("failed to connect to server")
            && (message.contains("connection refused")
                || message.contains("no such file or directory")
                || message.contains("not found")))
}

/// 识别已不存在 session 的稳定失败文本，保留 kill 用例的删除幂等性。
fn missing_session(stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "can't find session",
        "session not found",
        "no current session",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// 只有退出码确认成功或 stderr 明确说明目标已不存在时才给出确定 kill 结果。
fn classify_kill_result(output: &super::CommandExecutionResult) -> AppResult<bool> {
    if output.exit_code == Some(0) {
        return Ok(true);
    }
    if !output.truncated && (missing_session(&output.stderr) || no_server_running(&output.stderr)) {
        return Ok(false);
    }
    Err(AppError::remote_exit(output.exit_code))
}

/// 非零退出码作为 tmux 业务失败返回，不能把失败刷新伪装成成功变更。
fn ensure_success(output: &super::CommandExecutionResult) -> AppResult<()> {
    if output.exit_code == Some(0) {
        Ok(())
    } else {
        Err(AppError::remote_exit(output.exit_code))
    }
}

#[cfg(test)]
#[path = "../../tests/support/tmux.rs"]
mod tests;
