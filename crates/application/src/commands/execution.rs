//! 单条命令发送与历史记录用例。
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{CommandHistoryRecordRequest, CommandHistorySource, CommandHistoryTarget},
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use crate::sessions::{UiCommandLineRequest, UiSessionBinding};

use super::{AppError, AppResult, CommandsService};

/// 已发送事实与历史落盘结果分开返回，避免数据库故障把已执行命令伪报为未发送。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandExecutionReceipt {
    pub sent: bool,
    /// 普通命令保持交互 PTY 输入语义；write ack 不代表 shell 已完成命令。
    pub completion_tracked: bool,
    pub history_recorded: Option<bool>,
    pub history_skip_reason: Option<String>,
    pub history_error_code: Option<super::PublicErrorCode>,
}

impl CommandsService {
    /// 普通命令也必须携带可见 pane 的 generation；历史只在原子 writer 确认后提交。
    ///
    /// @author kongweiguang
    pub async fn send_ui_command(
        &self,
        binding: UiSessionBinding,
        command: String,
        record: bool,
        cancellation: CancellationToken,
    ) -> AppResult<CommandExecutionReceipt> {
        validate_command_text(&command)?;
        if !matches!(&binding.owner, SessionOwner::Ui { .. }) {
            return Err(AppError::conflict("命令输入必须绑定可见 UI 分屏"));
        }
        let session_id = binding.session_id.clone();
        let _command_guard = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令发送已取消")),
            guard = self.command_coordinator.acquire(&session_id) => guard,
        };
        let session = self.session_info(session_id.clone()).await?;
        if session.lifecycle != SessionLifecycle::Running || session.owner != binding.owner {
            return Err(AppError::conflict("指定终端会话当前不可执行命令"));
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令发送已取消；终端尚未接纳本次写入"));
        }

        // writer 入口在同一个 session gate 中复验 owner 和 generation；调用方不能拆分正文与 Enter。
        self.sessions
            .submit_ui_command_line(UiCommandLineRequest {
                binding,
                command: command.clone(),
            })
            .await?;

        let history = self
            .record_history(CommandHistoryRecordRequest {
                command,
                source: CommandHistorySource::User,
                target: history_target(&session.target),
                record: Some(record),
                session_id: Some(session.session_id),
                pane_id: owner_pane(&session.owner),
                tab_id: owner_tab(&session.owner),
                profile_id: None,
                remote_host_id: target_host(&session.target),
                cwd: target_cwd(&session.target),
                shell: local_shell(&session.target),
            })
            .await;
        Ok(match history {
            Ok(result) => CommandExecutionReceipt {
                sent: true,
                completion_tracked: false,
                history_recorded: Some(result.recorded),
                history_skip_reason: result.skip_reason,
                history_error_code: None,
            },
            Err(error) => CommandExecutionReceipt {
                sent: true,
                completion_tracked: false,
                history_recorded: None,
                history_skip_reason: None,
                history_error_code: Some(error.public_code()),
            },
        })
    }
}

/// 拒绝空命令、过长正文和 NUL，避免不受控输入进入 terminal transport。
fn validate_command_text(command: &str) -> AppResult<()> {
    if command.trim().is_empty() || command.chars().count() > 16_000 || command.contains('\0') {
        return Err(AppError::invalid("命令内容为空或超过长度限制"));
    }
    Ok(())
}

/// 只从会话真实目标映射历史类型，不使用 UI 文本或默认目标推断协议。
pub(super) fn history_target(target: &SessionTarget) -> CommandHistoryTarget {
    match target {
        SessionTarget::Local(_) => CommandHistoryTarget::Local,
        SessionTarget::Ssh(_) => CommandHistoryTarget::Ssh,
        SessionTarget::Telnet(_) => CommandHistoryTarget::Telnet,
        SessionTarget::Serial(_) => CommandHistoryTarget::Serial,
        SessionTarget::Container(_) => CommandHistoryTarget::Container,
    }
}

/// 只记录由 UI 所有的 pane，后台 Agent/MCP session 不伪造成可见分屏。
pub(super) fn owner_pane(owner: &SessionOwner) -> Option<kerminal_domain::identity::PaneId> {
    match owner {
        SessionOwner::Ui { pane_id, .. } => Some(pane_id.clone()),
        SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => None,
    }
}

/// 只记录由 UI 所有的 tab，后台命令仍通过 session ID 精确关联。
pub(super) fn owner_tab(owner: &SessionOwner) -> Option<kerminal_domain::identity::TabId> {
    match owner {
        SessionOwner::Ui { tab_id, .. } => Some(tab_id.clone()),
        SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => None,
    }
}

/// 从实际连接目标读取 host 引用，避免将容器或本地命令写入错误主机上下文。
pub(super) fn target_host(target: &SessionTarget) -> Option<kerminal_domain::identity::HostId> {
    match target {
        SessionTarget::Ssh(target) => Some(target.host_id.clone()),
        SessionTarget::Container(target) => target.host_id.clone(),
        SessionTarget::Local(_) | SessionTarget::Telnet(_) | SessionTarget::Serial(_) => None,
    }
}

/// 从目标定义提取用户已配置的工作目录；远端未知目录保持缺省而不猜测。
pub(super) fn target_cwd(target: &SessionTarget) -> Option<String> {
    match target {
        SessionTarget::Local(target) => target.working_directory.as_ref(),
        SessionTarget::Ssh(target) => target.working_directory.as_ref(),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => None,
    }
    .map(|path| path.to_string_lossy().into_owned())
}

/// 本地 shell 标记只保存可执行文件名，不把配置目录或完整启动参数混入搜索元数据。
pub(super) fn local_shell(target: &SessionTarget) -> Option<String> {
    let SessionTarget::Local(target) = target else {
        return None;
    };
    Some(
        std::path::Path::new(target.program.as_os_str())
            .file_name()
            .unwrap_or(target.program.as_os_str())
            .to_string_lossy()
            .into_owned(),
    )
}
