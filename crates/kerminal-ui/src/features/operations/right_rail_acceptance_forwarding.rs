#![cfg(feature = "right-rail-acceptance")]

//! 固定右栏四类 SSH 隧道验收桥；每次动作都按完整 fixture 配置和稳定 ID 复核。
//!
//! @author kongweiguang

use gpui_kit::{Context, ElementId, Window};
use kerminal_application::operations::ManagedPortForwardSnapshot;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForwardConfig, ProxyApplyScope,
        ProxyProtocol, RemoteAccessScope,
    },
};

use super::{
    OperationsSection, OperationsView, forms::ForwardBindScope, proxy_panel::ProxyCurrentSession,
};

impl OperationsView {
    /// 回读刷新期间的同一恢复动作和忙碌门禁，原隧道被删除后仍不更换绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_proxy_action(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        binding: &UiSessionBinding,
        cx: &gpui_kit::App,
    ) -> Result<(bool, bool), String> {
        validate_acceptance_forward(expected)?;
        if self.host_target.as_ref() != Some(&expected.host_id) || !self.active {
            return Err("代理验收目标已变化".to_owned());
        }
        let panel = self
            .proxy_panel
            .as_ref()
            .ok_or_else(|| "代理面板不可用".to_owned())?
            .read(cx);
        let current = panel
            .current_session()
            .ok_or_else(|| "刷新时代理终端投影消失".to_owned())?;
        if current.binding != *binding || panel.current_recovery_forward_id() != Some(forward_id) {
            return Err("刷新时代理恢复目标已变化".to_owned());
        }
        let action = panel.action_state_for_forward(forward_id);
        Ok((action.busy, action.restore_current))
    }

    /// 制造唯一的无效监听端口以执行正式校验，同时保持 fixture 表单及主机身份不变。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_invalid_forward_port(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ElementId, String> {
        if self.busy || self.forward_form_host_id().as_ref() != Some(host_id) {
            return Err("SSH 隧道表单目标不匹配或正在提交".to_owned());
        }
        let inputs = self
            .inputs
            .as_ref()
            .filter(|inputs| inputs.forward_form_open)
            .ok_or_else(|| "SSH 隧道表单尚未打开".to_owned())?;
        let port = inputs.forward_bind_port.clone();
        port.update(cx, |state, cx| state.set_value("0", window, cx));
        Ok(("input", port.entity_id()).into())
    }

    /// 验收同时观察真实列表 Loading 阶段和稳定请求序号，避免空白帧被当成刷新效果。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_forward_loading(&self) -> bool {
        self.forwards.phase() == super::state::SectionPhase::Loading
    }

    /// 仅读取当前 fixture 的代理投影，停止或删除原转发后仍须保留原恢复记录。
    /// 只读实体使用 App 上下文，使 Root 核验无需构造 Operations 的可变上下文。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_proxy_session(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &gpui_kit::App,
    ) -> Result<Option<(ProxyCurrentSession, Option<String>, String)>, String> {
        validate_acceptance_forward(expected)?;
        if expected.kind != ForwardKind::RemoteDynamic {
            return Err("代理验收要求远端 SOCKS 隧道".to_owned());
        }
        if !self.right_rail_acceptance_ready(OperationsSection::PortForwards, &expected.host_id)? {
            return Ok(None);
        }
        let Some(panel) = &self.proxy_panel else {
            return Ok(None);
        };
        let panel_id = panel.entity_id();
        let panel = panel.read(cx);
        let Some(current) = panel.current_session().cloned() else {
            return Ok(None);
        };
        if current.host_id != expected.host_id {
            return Err("代理投影不属于 fixture 主机".to_owned());
        }
        Ok(Some((
            current,
            panel.current_recovery_forward_id().map(str::to_owned),
            format!(
                "panel={panel_id:?}, action={:?}",
                panel.action_state_for_forward(forward_id)
            ),
        )))
    }

    /// 只通过生产 Add 表单保存唯一 rr-ui 定义，并严格比较全部方向和端点字段。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_create_forward(
        &mut self,
        expected: &PortForwardConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let expected = acceptance_forward_config(expected)?;
        let rows = self
            .acceptance_forward_rows(&expected.host_id)?
            .ok_or_else(|| "SSH 隧道快照尚未就绪".to_owned())?;
        if rows
            .iter()
            .any(|row| row.forward.config.name == expected.name)
        {
            return Err("本轮 rr-ui 隧道名称已存在；拒绝覆盖或复用".to_owned());
        }
        if self.busy || self.forward_save_pending.is_some() || self.root_confirmation_active() {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }

        self.prepare_forward_add(window, cx);
        let form_open = self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.forward_form_open);
        if !form_open || self.forward_form_host_id().as_ref() != Some(&expected.host_id) {
            if form_open {
                self.cancel_forward_form(cx);
            }
            return Err("生产隧道表单未绑定 fixture SSH 主机".to_owned());
        }
        self.switch_forward_kind(expected.kind, window, cx);
        if self.forward_kind != expected.kind {
            self.cancel_forward_form(cx);
            return Err("生产隧道表单未能切换到 fixture 转发类型".to_owned());
        }
        let Some(inputs) = self.inputs.as_ref() else {
            self.cancel_forward_form(cx);
            return Err("生产隧道表单缺少输入状态".to_owned());
        };
        inputs.forward_name.update(cx, |state, cx| {
            state.set_value(expected.name.clone(), window, cx)
        });
        inputs.forward_bind_scope.update(cx, |select, cx| {
            select.set_selected_value(&ForwardBindScope::Loopback, window, cx);
        });
        inputs.forward_bind_host.update(cx, |state, cx| {
            state.set_value(expected.bind.host.clone(), window, cx);
        });
        inputs.forward_bind_port.update(cx, |state, cx| {
            state.set_value(expected.bind.port.get().to_string(), window, cx);
        });
        let (destination_host, destination_port) = expected.destination.as_ref().map_or_else(
            || (String::new(), String::new()),
            |endpoint| (endpoint.host.clone(), endpoint.port.get().to_string()),
        );
        inputs.forward_destination_host.update(cx, |state, cx| {
            state.set_value(destination_host, window, cx);
        });
        inputs.forward_destination_port.update(cx, |state, cx| {
            state.set_value(destination_port, window, cx);
        });

        self.save_forward(false, cx);
        if self.busy {
            Ok(true)
        } else {
            Err(self
                .status
                .clone()
                .unwrap_or_else(|| "生产隧道表单没有提交保存操作".to_owned()))
        }
    }

    /// 从目标 host 的真实生产清单回读保存后的停止定义，并返回其稳定 ID。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_forward_id(
        &self,
        expected: &PortForwardConfig,
    ) -> Result<Option<String>, String> {
        validate_acceptance_forward(expected)?;
        if self.busy {
            return Ok(None);
        }
        let Some(rows) = self.acceptance_forward_rows(&expected.host_id)? else {
            return Ok(None);
        };
        let mut matches = rows
            .iter()
            .filter(|row| row.forward.config.name == expected.name);
        let Some(row) = matches.next() else {
            return self.acceptance_operation_error().map_or(Ok(None), Err);
        };
        if matches.next().is_some() {
            return Err("fixture 清单中存在重复的 rr-ui 隧道名称".to_owned());
        }
        if row.forward.config != *expected
            || !row.persistent
            || row.forward.state != ForwardState::Stopped
        {
            return Err("生产隧道回执与本轮 fixture 配置不一致".to_owned());
        }
        Ok(Some(row.forward.id.clone()))
    }

    /// 启动前再次核对同一停止定义，随后复用产品按稳定 ID 启动的正式动作。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_start_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let row = self.require_acceptance_forward(expected, forward_id)?;
        if row.forward.state != ForwardState::Stopped {
            return Err("本轮 fixture 隧道不处于可启动的停止态".to_owned());
        }
        if self.busy || self.root_confirmation_active() {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }
        self.start_saved_forward(forward_id.to_owned(), cx);
        self.mutation_started("生产隧道启动")
    }

    /// 回读同一 ID 的实际状态，允许 driver 区分失败启动、活动和停止，而不是只看受理结果。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_forward_state(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
    ) -> Result<Option<ForwardState>, String> {
        validate_acceptance_forward(expected)?;
        if self.busy {
            return Ok(None);
        }
        let Some(rows) = self.acceptance_forward_rows(&expected.host_id)? else {
            return Ok(None);
        };
        let row = rows
            .iter()
            .find(|row| row.forward.id == forward_id)
            .ok_or_else(|| "本轮 fixture 隧道 ID 已从生产清单消失".to_owned())?;
        validate_fixture_forward_row(row, expected, forward_id)?;
        Ok(Some(row.forward.state))
    }

    /// 活动态、失败态或关闭中的同一 fixture ID 才可停止；结果未知时由 driver 先回读状态。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_stop_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let row = self.require_acceptance_forward(expected, forward_id)?;
        if !matches!(
            row.forward.state,
            ForwardState::Active | ForwardState::Failed | ForwardState::Closing
        ) {
            return Err("本轮 fixture 隧道不处于可停止状态".to_owned());
        }
        if self.busy || self.root_confirmation_active() {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }
        self.stop_forward(forward_id.to_owned(), cx);
        self.mutation_started("生产隧道停止")
    }

    /// 冻结同一 fixture 的删除确认，避免验收绕过生产确认入口或改投其他定义。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_forward_delete_confirmation(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.require_acceptance_forward(expected, forward_id)?;
        if self.busy || self.root_confirmation_active() {
            return Err("Operations 有其他确认或写操作在途".to_owned());
        }
        self.request_delete_forward_definition(forward_id.to_owned(), cx);
        if self.right_rail_acceptance_forward_delete_confirmation_ready(expected, forward_id)? {
            Ok(true)
        } else {
            Err("生产隧道删除确认未打开".to_owned())
        }
    }

    /// 只接受完整配置、稳定 ID 和目标代次均匹配的删除确认。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_forward_delete_confirmation_ready(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
    ) -> Result<bool, String> {
        self.require_acceptance_forward(expected, forward_id)?;
        let Some((definition, generation)) = self.forward_pending_delete.as_ref() else {
            return Ok(false);
        };
        if definition.id != forward_id
            || definition.config != *expected
            || *generation != self.target_generation
        {
            return Err("隧道删除确认与验收目标不一致".to_owned());
        }
        Ok(true)
    }

    /// 取消仅撤销确认草稿，并重新核对原定义仍在当前目标清单中。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_cancel_forward_delete_confirmation(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_forward_delete_confirmation_ready(expected, forward_id)? {
            return Err("没有待取消的验收隧道删除确认".to_owned());
        }
        self.cancel_root_confirmation(cx);
        if self.forward_pending_delete.is_some() || self.busy {
            return Err("隧道删除确认未被取消".to_owned());
        }
        self.require_acceptance_forward(expected, forward_id)?;
        Ok(true)
    }

    /// 确认同一 ID 的停止定义后才删除，结果仍由生产 repository 和清单回读确认。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_delete_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let row = self.require_acceptance_forward(expected, forward_id)?;
        if row.forward.state != ForwardState::Stopped {
            return Err("必须先停止本轮 fixture 隧道才能删除定义".to_owned());
        }
        if self.busy
            || !self
                .right_rail_acceptance_forward_delete_confirmation_ready(expected, forward_id)?
        {
            return Err("本轮 fixture 隧道的删除确认尚未就绪".to_owned());
        }
        self.confirm_root_confirmation(cx);
        self.mutation_started("生产隧道删除")
    }

    /// 仅当目标 host 的 Ready 清单不再含本轮 ID 和唯一名称时确认删除完成。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_forward_deleted(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
    ) -> Result<bool, String> {
        validate_acceptance_forward(expected)?;
        if self.busy {
            return Ok(false);
        }
        let Some(rows) = self.acceptance_forward_rows(&expected.host_id)? else {
            return Ok(false);
        };
        if rows
            .iter()
            .any(|row| row.forward.id == forward_id || row.forward.config.name == expected.name)
        {
            return self.acceptance_operation_error().map_or(Ok(false), Err);
        }
        Ok(true)
    }

    /// 读写均限于当前 fixture host 的 Ports 视图；不从全局清单切换到其它目标。
    ///
    /// @author kongweiguang
    fn acceptance_forward_rows(
        &self,
        host_id: &HostId,
    ) -> Result<Option<&[ManagedPortForwardSnapshot]>, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::PortForwards, host_id)? {
            return Ok(None);
        }
        Ok(self.forwards.data().map(Vec::as_slice))
    }

    /// 精确绑定配置、host、持久性与 ID，拒绝验收名称被复用到另一个转发对象。
    ///
    /// @author kongweiguang
    fn require_acceptance_forward(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
    ) -> Result<&ManagedPortForwardSnapshot, String> {
        validate_acceptance_forward(expected)?;
        let rows = self
            .acceptance_forward_rows(&expected.host_id)?
            .ok_or_else(|| "SSH 隧道快照尚未就绪".to_owned())?;
        let row = rows
            .iter()
            .find(|row| row.forward.id == forward_id)
            .ok_or_else(|| "本轮 fixture 隧道 ID 不在当前生产清单中".to_owned())?;
        validate_fixture_forward_row(row, expected, forward_id)?;
        Ok(row)
    }

    /// 生产动作应同步进入 busy；否则把校验失败或未接线视为显式错误。
    ///
    /// @author kongweiguang
    fn mutation_started(&self, action: &str) -> Result<bool, String> {
        if self.busy {
            Ok(true)
        } else {
            Err(self
                .status
                .clone()
                .unwrap_or_else(|| format!("{action}没有提交写操作")))
        }
    }

    /// 失败的正式写入保留生产错误消息，避免验收 driver 盲目重放请求。
    ///
    /// @author kongweiguang
    fn acceptance_operation_error(&self) -> Option<String> {
        (!self.busy).then(|| self.status.clone()).flatten()
    }
}

/// 严格限制到 loopback 四类模式及不应用代理的单次验收定义。
///
/// @author kongweiguang
fn validate_acceptance_forward(expected: &PortForwardConfig) -> Result<(), String> {
    validate_acceptance_forward_name(&expected.name)?;
    expected
        .validate()
        .map_err(|_| "验收隧道配置未通过领域校验".to_owned())?;
    let loopback_bind = expected.bind.host == "127.0.0.1";
    let destination_is_loopback = expected
        .destination
        .as_ref()
        .is_some_and(|endpoint| endpoint.host == "127.0.0.1");
    let dynamic = matches!(
        expected.kind,
        ForwardKind::Dynamic | ForwardKind::RemoteDynamic
    );
    let remote = matches!(
        expected.kind,
        ForwardKind::Remote | ForwardKind::RemoteDynamic
    );
    if !loopback_bind
        || expected.proxy_apply_scope != ProxyApplyScope::None
        || dynamic != expected.destination.is_none()
        || dynamic != (expected.proxy_protocol == Some(ProxyProtocol::Socks5))
        || (remote && expected.remote_access_scope != Some(RemoteAccessScope::Loopback))
        || (!remote && expected.remote_access_scope.is_some())
        || (!dynamic && !destination_is_loopback)
    {
        return Err("验收隧道必须使用 loopback、fixture SOCKS5/TCP 端点且不应用代理".to_owned());
    }
    Ok(())
}

/// 行动前逐字段复核配置，避免相同名称或伪造前缀将操作指向用户的隧道。
///
/// @author kongweiguang
fn validate_fixture_forward_row(
    row: &ManagedPortForwardSnapshot,
    expected: &PortForwardConfig,
    forward_id: &str,
) -> Result<(), String> {
    if row.forward.id != forward_id || row.forward.config != *expected || !row.persistent {
        return Err("验收隧道身份不匹配；拒绝操作其他转发".to_owned());
    }
    Ok(())
}

/// 只允许本轮唯一 `rr-ui-<suffix>` 命名空间，防止验收写入用户的普通隧道。
///
/// @author kongweiguang
fn validate_acceptance_forward_name(name: &str) -> Result<(), String> {
    let suffix = name.strip_prefix("rr-ui-").unwrap_or_default();
    if suffix.is_empty()
        || name.len() > 64
        || suffix.starts_with('-')
        || suffix.ends_with('-')
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("验收隧道名称必须使用唯一的 rr-ui-<id> 格式".to_owned());
    }
    Ok(())
}

/// 从真实 UI 使用的端点解析器构造唯一验收配置，复用产品对四种模式的字段规范。
///
/// @author kongweiguang
fn acceptance_forward_config(expected: &PortForwardConfig) -> Result<PortForwardConfig, String> {
    validate_acceptance_forward(expected)?;
    let (destination_host, destination_port) = expected.destination.as_ref().map_or_else(
        || (String::new(), String::new()),
        |endpoint: &ForwardEndpoint| (endpoint.host.clone(), endpoint.port.get().to_string()),
    );
    let rebuilt = super::forms::forward_config(
        Some(expected.host_id.clone()),
        &expected.name,
        expected.kind,
        &expected.bind.host,
        &expected.bind.port.get().to_string(),
        &destination_host,
        &destination_port,
    )
    .map_err(|error| error.message())?;
    if rebuilt != *expected {
        return Err("验收隧道配置与生产表单解析结果不一致".to_owned());
    }
    Ok(rebuilt)
}
