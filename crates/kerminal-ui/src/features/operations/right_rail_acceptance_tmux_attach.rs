//! 仅供隔离 fixture 验证 tmux Attach 与 header 分离的生产入口和服务回读。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{identity::HostId, operations::TmuxSession, sessions::SessionOwner};

use super::super::{OperationsView, TmuxSnapshot, state::SectionPhase};

impl OperationsView {
    /// 查找严格属于固定 SSH tmux server 的随机 session 名称，区分快照未就绪与确认不存在。
    /// 外层 None 表示继续等待，内层 None 表示 Ready 快照中没有该名称。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_session_id_for_name(
        &self,
        host_id: &HostId,
        session_name: &str,
        binding: &UiSessionBinding,
    ) -> Result<Option<Option<String>>, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_attach_acceptance_session_name(session_name)?;
        if !self.active || self.busy {
            return Ok(None);
        }
        self.validate_attach_acceptance_binding(host_id, binding)?;
        let Some(snapshot) = self.attach_acceptance_tmux_snapshot(host_id)? else {
            return Ok(None);
        };
        Ok(Some(
            snapshot
                .sessions
                .iter()
                .find(|session| session.name == session_name)
                .map(|session| session.id.clone()),
        ))
    }

    /// 在生产 Ops target 与原 UI owner/generation 仍匹配时调用 Attach 按钮所用的生产入口。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_request_tmux_attach(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_attach_acceptance_session_name(session_name)?;
        self.validate_attach_acceptance_binding(host_id, binding)?;
        if self.busy || self.root_confirmation_active() {
            return Err("tmux Attach 被其它 Operations 写入或确认占用".to_owned());
        }
        let Some(session) =
            self.attach_acceptance_tmux_session(host_id, session_id, session_name)?
        else {
            return Ok(false);
        };
        if session.attached || session.clients != 0 {
            return Err("本轮新建 tmux session 已有 client；拒绝把它当作未连接 fixture".to_owned());
        }
        if !self.tmux_attach_ready() {
            return Err("当前 fixture pane 不再是 tmux Attach 的同目标终端".to_owned());
        }

        self.request_tmux_attach(session_id.to_owned(), session_name.to_owned(), cx);
        Ok(self.tmux_details.session_id.as_deref() == Some(session_id))
    }

    /// 只在 tmux 读取未完成且无其它写操作时启动生产刷新，避免验收轮询取消上一轮请求。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_refresh_tmux_snapshot(
        &mut self,
        host_id: &HostId,
        binding: &UiSessionBinding,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        self.validate_acceptance_tmux_target(host_id)?;
        if !self.active {
            return Err("tmux 验收面板当前未显示".to_owned());
        }
        self.validate_attach_acceptance_binding(host_id, binding)?;
        if !self.busy && self.tmux.phase() != SectionPhase::Loading {
            self.load_tmux(cx);
        }
        Ok(())
    }

    /// 返回本轮 session 的生产 attached/client 数，读取仍加载时不把旧值报告成完成状态。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_session_clients(
        &self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
    ) -> Result<Option<(bool, u32)>, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_attach_acceptance_session_name(session_name)?;
        self.validate_attach_acceptance_binding(host_id, binding)?;
        if !self.active || self.busy || self.tmux_detach_pending.is_some() {
            return Ok(None);
        }
        let Some(session) =
            self.attach_acceptance_tmux_session(host_id, session_id, session_name)?
        else {
            return Ok(None);
        };
        Ok(Some((session.attached, session.clients)))
    }

    /// 只调用生产 header 的 detach request；新 Attach 流程必须先由服务确认至少一个 client。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_request_tmux_header_exit(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
        binding: &UiSessionBinding,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        validate_attach_acceptance_session_name(session_name)?;
        self.validate_attach_acceptance_binding(host_id, binding)?;
        let Some(session) =
            self.attach_acceptance_tmux_session(host_id, session_id, session_name)?
        else {
            return Ok(false);
        };
        if !session.attached && session.clients == 0 {
            return Err("生产快照尚未确认本轮 session 有 attached client".to_owned());
        }
        if self.tmux_details.session_id.as_deref() != Some(session_id) {
            return Err("tmux header 当前选择不是本轮唯一 session".to_owned());
        }
        if self.current_tmux_attach_intent().is_none() || self.tmux_detach_pending.is_some() {
            return Err("生产 tmux header 分离按钮当前不可用".to_owned());
        }

        self.request_tmux_detach_current(cx);
        Ok(self.tmux_detach_pending.as_ref().is_some_and(|pending| {
            pending.terminal.binding == *binding
                && pending.terminal.target.host_id() == Some(host_id)
        }))
    }

    /// 分离回执没有到达前保持等待；只有原 fixture binding 仍有效时才报告 header action 已收尾。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_detach_idle(
        &self,
        host_id: &HostId,
        binding: &UiSessionBinding,
    ) -> Result<bool, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        self.validate_attach_acceptance_binding(host_id, binding)?;
        Ok(self.tmux_detach_pending.is_none())
    }

    /// 每个 I/O 回执都绑定固定 SSH host 与原 pane 的完整 session generation。
    ///
    /// @author kongweiguang
    fn validate_attach_acceptance_binding(
        &self,
        host_id: &HostId,
        binding: &UiSessionBinding,
    ) -> Result<(), String> {
        let terminal = self
            .tmux_attach_binding
            .as_ref()
            .ok_or_else(|| "tmux 验收缺少 Root 投影的 fixture pane".to_owned())?;
        let owner_matches_pane = matches!(
            &binding.owner,
            SessionOwner::Ui { pane_id, .. } if pane_id == &terminal.pane_id
        );
        if terminal.binding != *binding
            || terminal.target.host_id() != Some(host_id)
            || !owner_matches_pane
        {
            return Err("tmux 验收的 Root pane owner 或 generation 已变化".to_owned());
        }
        Ok(())
    }

    /// Ready 快照中的行必须同时匹配本轮 ID、名称与唯一 tmux capability target。
    ///
    /// @author kongweiguang
    fn attach_acceptance_tmux_session<'a>(
        &'a self,
        host_id: &HostId,
        session_id: &str,
        session_name: &str,
    ) -> Result<Option<&'a TmuxSession>, String> {
        validate_attach_acceptance_session_name(session_name)?;
        let Some(snapshot) = self.attach_acceptance_tmux_snapshot(host_id)? else {
            return Ok(None);
        };
        snapshot
            .sessions
            .iter()
            .find(|session| {
                session.id == session_id
                    && session.name == session_name
                    && session.target_ref == snapshot.capability.target_ref
            })
            .map(Some)
            .ok_or_else(|| "本轮 tmux session 已从 fixture server 消失或身份变化".to_owned())
    }

    /// 仅消费当前固定 tmux section 的完整成功快照，错误、加载和取消状态都不能伪造资源事实。
    ///
    /// @author kongweiguang
    fn attach_acceptance_tmux_snapshot(
        &self,
        host_id: &HostId,
    ) -> Result<Option<&TmuxSnapshot>, String> {
        self.validate_acceptance_tmux_target(host_id)?;
        match self.tmux.phase() {
            SectionPhase::Ready => {
                if let Some(error) = self.tmux.error() {
                    return Err(format!("tmux 验收服务读取失败：{error}"));
                }
                let snapshot = self
                    .tmux
                    .data()
                    .ok_or_else(|| "tmux Ready 状态缺少生产 session 快照".to_owned())?;
                if !snapshot.capability.available {
                    return Err(snapshot
                        .capability
                        .reason
                        .clone()
                        .unwrap_or_else(|| "fixture host 的 tmux server 不可用".to_owned()));
                }
                if !snapshot
                    .sessions
                    .iter()
                    .all(|session| session.target_ref == snapshot.capability.target_ref)
                {
                    return Err("tmux 验收快照混入其它 server target".to_owned());
                }
                Ok(Some(snapshot))
            }
            SectionPhase::Failed => Err(self
                .tmux
                .error()
                .map(|error| format!("tmux 验收服务读取失败：{error}"))
                .unwrap_or_else(|| "tmux 验收服务读取失败".to_owned())),
            SectionPhase::NotLoaded | SectionPhase::Loading | SectionPhase::Cancelled => Ok(None),
        }
    }
}

/// 验收 session 名使用固定产品前缀和单次 run 标记，不能引用 fixture 种子或其它会话。
///
/// @author kongweiguang
fn validate_attach_acceptance_session_name(name: &str) -> Result<(), String> {
    let suffix = name.strip_prefix("rr-ui-").unwrap_or_default();
    if suffix.is_empty()
        || name.len() > 64
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Attach 验收只允许 rr-ui- 前缀的本轮唯一 session 名".to_owned());
    }
    Ok(())
}
