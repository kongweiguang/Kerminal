//! 仅允许验收驱动创建并回读本轮唯一 tmux session，不触碰预置会话。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_domain::{identity::HostId, operations::TmuxSession};

use super::{OperationsSection, OperationsView, ToolRailOperationTarget, state::SectionPhase};

impl OperationsView {
    /// 在固定 SSH 快照中确认名称未占用后，经生产弹框和确认回调提交唯一验收 session。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_create_tmux_session(
        &mut self,
        host_id: &HostId,
        session_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_acceptance_session_name(session_name)?;
        let snapshot = self
            .acceptance_tmux_snapshot(host_id)?
            .ok_or_else(|| "tmux 会话快照尚未就绪".to_owned())?;
        if !snapshot.capability.available {
            return Err("fixture SSH 主机没有可用的 tmux server".to_owned());
        }
        if snapshot
            .sessions
            .iter()
            .any(|session| session.name == session_name)
        {
            return Err("唯一验收 tmux session 名称已存在；拒绝覆盖或复用".to_owned());
        }
        if self.root_confirmation_active() || self.busy {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }

        self.request_tmux_create_dialog(window, cx);
        if !self.tmux_create_form_open {
            return Err("生产 tmux 创建表单未能打开".to_owned());
        }
        let Some(inputs) = self.inputs.as_ref() else {
            self.cancel_root_confirmation(cx);
            return Err("生产 tmux 创建表单缺少输入状态".to_owned());
        };
        inputs.tmux_name.update(cx, |state, cx| {
            state.set_value(session_name.to_owned(), window, cx);
        });
        self.confirm_root_confirmation(cx);
        if self.busy {
            Ok(true)
        } else {
            Err(self
                .status
                .clone()
                .unwrap_or_else(|| "生产 tmux 创建确认没有提交写操作".to_owned()))
        }
    }

    /// 返回 Ready 快照中目标匹配的唯一会话 ID；写请求或读取未完成时让驱动继续等待。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_created_session_id(
        &self,
        host_id: &HostId,
        session_name: &str,
    ) -> Result<Option<String>, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_acceptance_session_name(session_name)?;
        if self.busy {
            return Ok(None);
        }
        let Some(snapshot) = self.acceptance_tmux_snapshot(host_id)? else {
            return Ok(None);
        };
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == session_name)
        {
            return Ok(Some(session.id.clone()));
        }
        self.acceptance_tmux_operation_error().map_or(Ok(None), Err)
    }

    /// 仅对本轮唯一名称所指向的 session ID 打开正式改名确认，并提交派生的新名称。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_rename_tmux_session(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        current_name: &str,
        next_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_acceptance_session_name(current_name)?;
        validate_acceptance_renamed_session_name(current_name, next_name)?;
        let snapshot = self
            .acceptance_tmux_snapshot(host_id)?
            .ok_or_else(|| "tmux 会话快照尚未就绪".to_owned())?;
        require_fixture_session(snapshot, session_id, current_name)?;
        if self.root_confirmation_active() || self.busy {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }

        self.request_tmux_rename_dialog(session_id.to_owned(), window, cx);
        if self.tmux_rename_target.as_deref() != Some(session_id) {
            return Err("生产 tmux 改名表单未能绑定本轮 session".to_owned());
        }
        let Some(inputs) = self.inputs.as_ref() else {
            self.cancel_root_confirmation(cx);
            return Err("生产 tmux 改名表单缺少输入状态".to_owned());
        };
        inputs.tmux_rename.update(cx, |state, cx| {
            state.set_value(next_name.to_owned(), window, cx);
        });
        self.confirm_root_confirmation(cx);
        if self.busy {
            Ok(true)
        } else {
            Err(self
                .status
                .clone()
                .unwrap_or_else(|| "生产 tmux 改名确认没有提交写操作".to_owned()))
        }
    }

    /// 只有指定 ID 已在目标 Ready 快照中改成派生名称时才报告重命名完成。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_rename_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
        expected_name: &str,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_acceptance_renamed_session_name_from_suffix(expected_name)?;
        if self.busy {
            return Ok(false);
        }
        let Some(snapshot) = self.acceptance_tmux_snapshot(host_id)? else {
            return Ok(false);
        };
        if snapshot
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.name == expected_name)
        {
            return Ok(true);
        }
        self.acceptance_tmux_operation_error()
            .map_or(Ok(false), Err)
    }

    /// 仅对仍以本轮 `rr-ui-` 名称标识的 session ID 提交生产结束确认。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_kill_tmux_session(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        expected_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_acceptance_owned_session_name(expected_name)?;
        let snapshot = self
            .acceptance_tmux_snapshot(host_id)?
            .ok_or_else(|| "tmux 会话快照尚未就绪".to_owned())?;
        require_fixture_session(snapshot, session_id, expected_name)?;
        if self.root_confirmation_active() || self.busy {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }

        self.request_tmux_kill_dialog(session_id.to_owned(), cx);
        if self.tmux_pending_kill.as_deref() != Some(session_id) {
            return Err("生产 tmux 结束确认未能绑定本轮 session".to_owned());
        }
        self.confirm_root_confirmation(cx);
        if self.busy {
            Ok(true)
        } else {
            Err(self
                .status
                .clone()
                .unwrap_or_else(|| "生产 tmux 结束确认没有提交写操作".to_owned()))
        }
    }

    /// 仅在目标 Ready 快照确认指定 ID 消失后报告结束成功，失败回执会向 driver 返回错误。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_kill_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        if self.busy {
            return Ok(false);
        }
        let Some(snapshot) = self.acceptance_tmux_snapshot(host_id)? else {
            return Ok(false);
        };
        if snapshot
            .sessions
            .iter()
            .any(|session| session.id == session_id)
        {
            return self
                .acceptance_tmux_operation_error()
                .map_or(Ok(false), Err);
        }
        Ok(true)
    }

    /// 拒绝本机、dashboard、其他 section、其他 SSH host 和非固定 tmux 目标。
    ///
    /// @author kongweiguang
    pub(super) fn validate_acceptance_tmux_target(&self, host_id: &HostId) -> Result<(), String> {
        let target = self.tmux_target();
        if !self.active
            || !self.section_selection.is_fixed()
            || self.section_selection.active() != OperationsSection::Tmux
            || self.host_target.as_ref() != Some(host_id)
            || !matches!(
                self.tool_rail_target.as_ref(),
                Some(ToolRailOperationTarget::Ssh { host_id: target }) if target == host_id
            )
            || !self.tmux_target_available()
            || target.validate().is_err()
            || target.host_id.as_ref() != Some(host_id)
        {
            return Err("验收 tmux 操作目标与当前 fixture SSH host 不一致".to_owned());
        }
        Ok(())
    }

    /// 区分未就绪快照与失败回执；旧快照不能作为 mutation 的成功回读。
    ///
    /// @author kongweiguang
    fn acceptance_tmux_snapshot(
        &self,
        host_id: &HostId,
    ) -> Result<Option<&super::TmuxSnapshot>, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        match self.tmux.phase() {
            SectionPhase::Ready => {
                if let Some(error) = self.tmux.error() {
                    return Err(format!("tmux 会话读取失败：{error}"));
                }
                let snapshot = self
                    .tmux
                    .data()
                    .ok_or_else(|| "tmux 已标记完成但缺少会话快照".to_owned())?;
                if !snapshot
                    .sessions
                    .iter()
                    .all(|session| session.target_ref == snapshot.capability.target_ref)
                {
                    return Err("tmux session 快照包含其他 server target".to_owned());
                }
                Ok(Some(snapshot))
            }
            SectionPhase::Failed => Err(self
                .tmux
                .error()
                .map(|error| format!("tmux 会话读取失败：{error}"))
                .unwrap_or_else(|| "tmux 会话读取失败".to_owned())),
            SectionPhase::NotLoaded | SectionPhase::Loading | SectionPhase::Cancelled => Ok(None),
        }
    }

    /// 读取失败表单保留在生产确认态；以其原始反馈终止验收等待并留给 fixture 清理。
    ///
    /// @author kongweiguang
    fn acceptance_tmux_operation_error(&self) -> Option<String> {
        (!self.busy && self.tmux_dialog_context.is_some())
            .then(|| self.status.clone())
            .flatten()
    }
}

/// 只接受本轮随机 fixture 命名空间中的 tmux session，阻止验收入口引用种子会话。
///
/// @author kongweiguang
fn validate_acceptance_session_name(name: &str) -> Result<(), String> {
    let suffix = name.strip_prefix("rr-ui-").unwrap_or_default();
    if suffix.is_empty()
        || name.len() > 64
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("验收 tmux session 必须使用唯一的 rr-ui-<id> 名称".to_owned());
    }
    Ok(())
}

/// 改名仅可派生为同一 fixture ID 的 `-renamed` 名称，避免把验收能力借给任意 session。
///
/// @author kongweiguang
fn validate_acceptance_renamed_session_name(current: &str, next: &str) -> Result<(), String> {
    validate_acceptance_session_name(current)?;
    let expected = format!("{current}-renamed");
    if next != expected {
        return Err("验收改名只能为本轮唯一 session 添加 -renamed 后缀".to_owned());
    }
    Ok(())
}

/// 回读阶段只接收 fixture 约定的改名结果，不把自由输入作为预期状态。
///
/// @author kongweiguang
fn validate_acceptance_renamed_session_name_from_suffix(name: &str) -> Result<(), String> {
    let Some(current) = name.strip_suffix("-renamed") else {
        return Err("验收 tmux 改名结果缺少 -renamed 后缀".to_owned());
    };
    validate_acceptance_session_name(current)
}

/// 清理允许使用创建名或其唯一派生名，保留失败后由 driver 清理本轮资源的路径。
///
/// @author kongweiguang
fn validate_acceptance_owned_session_name(name: &str) -> Result<(), String> {
    if let Some(current) = name.strip_suffix("-renamed") {
        validate_acceptance_session_name(current)
    } else {
        validate_acceptance_session_name(name)
    }
}

/// 在写入前确认 session ID、fixture 名称和 server scope 同时匹配快照中的同一行。
///
/// @author kongweiguang
fn require_fixture_session<'a>(
    snapshot: &'a super::TmuxSnapshot,
    session_id: &str,
    expected_name: &str,
) -> Result<&'a TmuxSession, String> {
    snapshot
        .sessions
        .iter()
        .find(|session| {
            session.id == session_id
                && session.name == expected_name
                && session.target_ref == snapshot.capability.target_ref
        })
        .ok_or_else(|| "验收操作拒绝触碰非本轮或已变化的 tmux session".to_owned())
}
