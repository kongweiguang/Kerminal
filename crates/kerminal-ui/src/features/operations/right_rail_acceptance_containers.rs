//! 固定右栏容器验收桥接；所有读回和动作都限定在当前 Containers SSH 面板。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_domain::{
    identity::HostId,
    operations::{ContainerAction, ContainerStatus},
};

use super::{ContainerMutationRequest, OperationsSection, OperationsView};

impl OperationsView {
    /// 只从 Ready 列表返回 fixture 的真实容器 ID 和状态，避免旧快照或相似名称驱动动作。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_container_state(
        &self,
        host_id: &HostId,
        fixture_container_name: &str,
    ) -> Result<Option<(String, ContainerStatus)>, String> {
        let Some(container) = self.acceptance_fixture_container(host_id, fixture_container_name)?
        else {
            return Ok(None);
        };
        Ok(Some((
            container.target.container_id.clone(),
            container.status,
        )))
    }

    /// 仅对本轮 nginx fixture 打开生产 Stop 或 Restart 确认，不经过验收专用弹层。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_container_lifecycle_confirmation(
        &mut self,
        host_id: &HostId,
        fixture_container_name: &str,
        action: ContainerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        validate_fixture_container_name(fixture_container_name)?;
        if !matches!(action, ContainerAction::Stop | ContainerAction::Restart) {
            return Err("容器确认验收只支持停止或重启".to_owned());
        }
        let container = self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .ok_or_else(|| "fixture 容器列表尚未就绪".to_owned())?;
        if container.status != ContainerStatus::Running {
            return Err("fixture 容器必须处于运行状态才能停止或重启".to_owned());
        }
        if self.busy || self.root_confirmation_active() {
            return Err("容器面板已有操作或确认弹层在途".to_owned());
        }

        let target = container.target.clone();
        self.container_host.select_container_row(target.clone());
        self.request_container_lifecycle_confirmation(
            target,
            fixture_container_name.to_owned(),
            action,
            window,
            cx,
        );
        if self.container_delete_confirmation.is_some() {
            Ok(())
        } else {
            Err("生产容器生命周期确认未能打开".to_owned())
        }
    }

    /// 只把当前固定 fixture 的生产生命周期确认投影为就绪，不把已发起的操作当作确认结果。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_container_confirmation_ready(
        &self,
        host_id: &HostId,
        fixture_container_name: &str,
    ) -> Result<bool, String> {
        validate_fixture_container_name(fixture_container_name)?;
        if self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .is_none()
        {
            return Ok(false);
        }
        Ok(self.container_delete_confirmation.is_some() && self.root_confirmation_active())
    }

    /// 取消只关闭尚未提交的生产确认，再经现有容器刷新事件验证 fixture 仍保持原状态。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_cancel_container_confirmation(
        &mut self,
        host_id: &HostId,
        fixture_container_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        validate_fixture_container_name(fixture_container_name)?;
        let container = self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .ok_or_else(|| "fixture 容器列表尚未就绪".to_owned())?;
        if container.status != ContainerStatus::Running || self.busy {
            return Err("取消前 fixture 容器状态或操作状态已变化".to_owned());
        }
        if self.container_delete_confirmation.is_none() {
            return Err("当前没有可取消的生产容器确认".to_owned());
        }

        self.dismiss_container_delete_confirmation(cx);
        if self.container_delete_confirmation.is_some() {
            return Err("生产容器确认未能取消".to_owned());
        }
        let ticket = self.container_action_ticket(None);
        self.request_container_panel_refresh(self.runtime, &ticket, cx);
        Ok(())
    }

    /// Root mutation event 异步处理；这里只确认生产确认草稿已消费，driver 随后仍须回读同一 ID 的目标状态。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_confirm_container_confirmation(
        &mut self,
        host_id: &HostId,
        fixture_container_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        validate_fixture_container_name(fixture_container_name)?;
        let container = self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .ok_or_else(|| "fixture 容器列表尚未就绪".to_owned())?;
        if container.status != ContainerStatus::Running || self.busy {
            return Err("确认前 fixture 容器状态或操作状态已变化".to_owned());
        }
        if self.container_delete_confirmation.is_none() {
            return Err("当前没有可提交的生产容器确认".to_owned());
        }

        self.confirm_container_delete(cx);
        if self.container_delete_confirmation.is_none() {
            Ok(())
        } else {
            Err("生产容器确认未被消费，操作未触发".to_owned())
        }
    }

    /// 从 Exited 快照提交生产 Start 请求；Root 异步处理后仍由 driver 回读同一 ID 的 Running 状态。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_start_container(
        &mut self,
        host_id: &HostId,
        fixture_container_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        validate_fixture_container_name(fixture_container_name)?;
        let container = self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .ok_or_else(|| "fixture 容器列表尚未就绪".to_owned())?;
        if container.status != ContainerStatus::Exited {
            return Err("fixture 容器必须先停止后才能验收启动".to_owned());
        }
        if self.busy || self.root_confirmation_active() {
            return Err("容器面板已有操作或确认弹层在途".to_owned());
        }

        let target = container.target.clone();
        let ticket = self.container_action_ticket(Some(target.clone()));
        self.container_host.select_container_row(target.clone());
        if !self.container_action_ticket_is_current(&ticket, Some(&target))
            || !self.container_mutation_allowed(&target)
        {
            return Err("fixture 启动目标已变化，未提交操作".to_owned());
        }
        self.request_container_mutation(
            ContainerMutationRequest {
                target,
                action: ContainerAction::Start,
                force: false,
            },
            &ticket,
            cx,
        );
        Ok(())
    }

    /// 日志入口复用生产菜单的事件并冻结当前 fixture target，不自行拼接终端命令。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_request_container_logs(
        &mut self,
        host_id: &HostId,
        fixture_container_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        validate_fixture_container_name(fixture_container_name)?;
        let container = self
            .acceptance_fixture_container(host_id, fixture_container_name)?
            .ok_or_else(|| "fixture 容器列表尚未就绪".to_owned())?;
        if container.status != ContainerStatus::Running {
            return Err("只有运行中的 fixture 容器可以打开日志".to_owned());
        }

        let target = container.target.clone();
        let ticket = self.container_action_ticket(Some(target.clone()));
        if !self.container_action_ticket_is_current(&ticket, Some(&target))
            || !self.container_read_allowed(&target)
        {
            return Err("日志请求目标已不属于当前 Containers 面板".to_owned());
        }
        self.container_host.select_container_row(target.clone());
        self.request_container_logs(target, &ticket, cx);
        Ok(())
    }

    /// 读取已通过真实 Details 请求回执的启动时间，供 Restart 回读排除旧 Running 快照。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_container_started_at(
        &self,
        host_id: &HostId,
        container_id: &str,
    ) -> Result<Option<String>, String> {
        let Some(container) = self.acceptance_fixture_container_by_id(host_id, container_id)?
        else {
            return Ok(None);
        };
        if !self.right_rail_acceptance_container_page_ready(host_id, container_id, "details")? {
            return Ok(None);
        }
        let Some(details) = self
            .container_inspect
            .data()
            .filter(|details| details.target == container.target)
        else {
            return Err("容器详情回执与 fixture target 不一致".to_owned());
        };
        details
            .started_at
            .clone()
            .map(Some)
            .ok_or_else(|| "运行中的 fixture 容器详情缺少 StartedAt".to_owned())
    }

    /// 当前容器列表、状态和详情必须仍对应同一 fixture host/runtime/ID。
    ///
    /// @author kongweiguang
    fn acceptance_fixture_container(
        &self,
        host_id: &HostId,
        fixture_container_name: &str,
    ) -> Result<Option<&kerminal_domain::operations::ContainerSummary>, String> {
        validate_fixture_container_name(fixture_container_name)?;
        if self.busy || !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(None);
        }
        self.validate_container_host_context(host_id)?;
        let rows = self
            .containers
            .data()
            .ok_or_else(|| "容器列表标记完成但缺少快照".to_owned())?;
        let mut matching = rows.iter().filter(|row| {
            row.name == fixture_container_name
                && row.target.host_id.as_ref() == Some(host_id)
                && row.target.runtime == self.runtime
        });
        let Some(container) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err("当前 SSH host/runtime 中存在重复 fixture 容器名称".to_owned());
        }
        if container.target.container_id.trim().is_empty() {
            return Err("fixture 容器快照缺少容器 ID".to_owned());
        }
        Ok(Some(container))
    }

    /// 容器 ID 不是名称代理；详情回读只接受同一 host/runtime 下精确匹配的原 ID。
    ///
    /// @author kongweiguang
    fn acceptance_fixture_container_by_id(
        &self,
        host_id: &HostId,
        container_id: &str,
    ) -> Result<Option<&kerminal_domain::operations::ContainerSummary>, String> {
        if container_id.trim().is_empty() || self.busy {
            return Ok(None);
        }
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(None);
        }
        self.validate_container_host_context(host_id)?;
        let rows = self
            .containers
            .data()
            .ok_or_else(|| "容器列表标记完成但缺少快照".to_owned())?;
        let mut matching = rows.iter().filter(|row| {
            row.target.container_id == container_id
                && row.target.host_id.as_ref() == Some(host_id)
                && row.target.runtime == self.runtime
        });
        let Some(container) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err("当前 SSH host/runtime 中存在重复 fixture 容器 ID".to_owned());
        }
        Ok(Some(container))
    }
}

/// 固定 fixture 命名空间避免验收参数映射到任意用户容器。
///
/// @author kongweiguang
fn validate_fixture_container_name(name: &str) -> Result<(), String> {
    let Some(suffix) = name.strip_prefix("kerminal-rr-") else {
        return Err("验收容器名必须使用 kerminal-rr- 前缀".to_owned());
    };
    if suffix.len() != 12 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("验收容器名必须以 12 位十六进制 fixture ID 结尾".to_owned());
    }
    Ok(())
}
