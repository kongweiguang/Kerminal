//! 固定右栏容器验收桥接；所有读回和动作都限定在当前 Containers SSH 面板。
//!
//! @author kongweiguang

use gpui_kit::{Bounds, Context, Pixels, Window};
use kerminal_domain::{
    files::FileEndpoint,
    identity::HostId,
    operations::{ComposeProjectTarget, ContainerAction, ContainerStatus},
};

use super::{ContainerMutationRequest, OperationsSection, OperationsView};

impl OperationsView {
    /// 读取真实编辑器及 fixture 末行的绘制边界，避免仅凭外围容器高度误判输入区已填满。
    /// 只读查询接受 App，使根视图可核验子实体而不伪造其 Context。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_editor_bounds(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &gpui_kit::App,
    ) -> Result<Option<(Bounds<Pixels>, Option<Bounds<Pixels>>)>, String> {
        let Some((_, _, _, _, ready)) =
            self.right_rail_acceptance_compose_presentation(host_id, target)?
        else {
            return Ok(None);
        };
        if !ready {
            return Ok(None);
        }
        let preview = self
            .compose_yaml
            .preview()
            .ok_or_else(|| "Compose 预览缺少文件回执".to_owned())?;
        if preview.truncated {
            return Err("Compose fixture 预览被截断".to_owned());
        }
        let text = std::str::from_utf8(&preview.bytes)
            .map_err(|_| "Compose fixture 不是 UTF-8".to_owned())?;
        let marker = "com.kerminal.fixture.kind";
        let tail = text
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| "Compose fixture 没有内容".to_owned())?;
        if !tail.trim().starts_with(marker) || !tail.contains("right-rail-compose") {
            return Err("Compose fixture 末行不符合本轮验收内容".to_owned());
        }
        let start = text
            .rfind(marker)
            .ok_or_else(|| "Compose fixture 缺少末行标识".to_owned())?;
        let editor = self
            .compose_yaml
            .editor()
            .ok_or_else(|| "Compose 预览编辑器不可用".to_owned())?
            .read(cx);
        if editor.value().as_ref() != text {
            return Err("Compose 编辑器文本与原始文件回执不一致".to_owned());
        }
        Ok(Some((
            editor.input_bounds(),
            editor.range_to_bounds(&(start..start + marker.len())),
        )))
    }

    /// 只读当前夹具项目的页面、展开和预览状态，确保真实点击没有跳到其他目标或复用旧文件回执。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_presentation(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
    ) -> Result<Option<(bool, bool, bool, bool, bool)>, String> {
        use super::render::container_panel::{ComposeProjectTab, ContainerHostPage};

        if self
            .right_rail_acceptance_compose_owned_state(host_id, target)?
            .is_none()
        {
            return Ok(None);
        }
        let list = matches!(self.container_host.page(), ContainerHostPage::List);
        let detail = matches!(
            self.container_host.page(),
            ContainerHostPage::ComposeProject { target: selected } if selected == target
        );
        let yaml = detail && self.container_host.project_tab() == ComposeProjectTab::Yaml;
        let yaml_ready = if yaml && !self.compose_yaml.is_loading() {
            if let Some(error) = self.compose_yaml.error() {
                return Err(format!("Compose YAML 预览失败：{error}"));
            }
            self.compose_yaml.preview().is_some_and(|preview| {
                matches!(
                    &preview.endpoint,
                    FileEndpoint::Remote { host_id: preview_host, path }
                        if preview_host == host_id && path.as_str() == target.config_files[0]
                ) && !preview.binary
                    && !preview.bytes.is_empty()
                    && self.compose_yaml.editor().is_some()
            })
        } else {
            false
        };
        Ok(Some((
            list,
            self.container_host.is_project_expanded(target),
            detail,
            yaml,
            yaml_ready,
        )))
    }

    /// 只返回本轮隔离 Compose 项目在权威宿主列表中的实际对象；拆除后允许返回空集合。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_owned_state(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
    ) -> Result<Option<Vec<(String, ContainerStatus)>>, String> {
        let run_id = validate_fixture_compose_target(host_id, target)?;
        if self.busy || !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(None);
        }
        self.validate_container_host_context(host_id)?;
        if !self.container_host.include_stopped() {
            return Err("Compose fixture 回读必须包含停止容器".to_owned());
        }
        if target.runtime != self.runtime || !self.compose_projects.contains(target) {
            return Err("Compose fixture 已不属于当前运行时的已知项目".to_owned());
        }
        if self.compose_projects.iter().any(|project| {
            project.host_id == target.host_id
                && project.runtime == target.runtime
                && project.project == target.project
                && project != target
        }) {
            return Err("同名 Compose 项目存在不同配置，拒绝操作 fixture".to_owned());
        }
        if let Some(status) = self.status.as_ref() {
            return Err(format!("Compose 操作反馈：{status}"));
        }
        if self.container_write_scope_blocked() {
            return Ok(None);
        }
        let rows = self
            .containers
            .data()
            .ok_or_else(|| "Compose fixture 列表缺少快照".to_owned())?;
        let mut result = Vec::new();
        for row in rows.iter().filter(|row| {
            row.compose
                .as_ref()
                .and_then(|metadata| metadata.project.as_deref())
                == Some(target.project.as_str())
        }) {
            if super::state::compose_target(row).as_ref() != Some(target) {
                return Err("同名 Compose 容器使用不同配置，拒绝操作 fixture".to_owned());
            }
            let owned_id = row
                .labels
                .iter()
                .any(|(key, value)| key == "com.kerminal.fixture.id" && value == run_id);
            let owned_kind = row.labels.iter().any(|(key, value)| {
                key == "com.kerminal.fixture.kind" && value == "right-rail-compose"
            });
            if row.name != target.project
                || !owned_id
                || !owned_kind
                || row.target.container_id.is_empty()
            {
                return Err("Compose 容器名称或所有权标签与本轮 fixture 不一致".to_owned());
            }
            result.push((row.target.container_id.clone(), row.status));
        }
        if result.len() > 1 {
            return Err("Compose fixture 出现多个容器，拒绝执行验收".to_owned());
        }
        Ok(Some(result))
    }

    /// 同一项目拆除后仍通过已保留的完整目标打开详情，不能按同名项目重新猜路径。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_select_owned_compose(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_fixture_compose_target(host_id, target)?;
        self.validate_container_host_context(host_id)?;
        if !self.container_host.include_stopped() {
            self.set_host_container_include_stopped(true, cx);
            return Ok(false);
        }
        if self
            .right_rail_acceptance_compose_owned_state(host_id, target)?
            .is_none()
        {
            return Ok(false);
        }
        self.leave_container_page_scroll();
        self.container_host.select_project(target.clone());
        self.compose_yaml.select_project(target);
        cx.notify();
        Ok(true)
    }

    /// 模态只有在绑定本轮完整 Compose 目标时才视为就绪。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_down_confirmation_ready(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
    ) -> Result<bool, String> {
        validate_fixture_compose_target(host_id, target)?;
        self.validate_container_host_context(host_id)?;
        Ok(self.active
            && self.compose_pending_down.as_ref() == Some(target)
            && self.compose_pending_context == self.container_panel_context
            && self.root_confirmation_active())
    }

    /// 取消复用生产根模态并请求原宿主刷新，证明没有发出拆除。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_cancel_compose_down(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !self.right_rail_acceptance_compose_down_confirmation_ready(host_id, target)? {
            return Err("没有本轮 Compose fixture 的拆除确认".to_owned());
        }
        self.cancel_root_confirmation(cx);
        let ticket = self.container_action_ticket(None);
        self.request_container_panel_refresh(self.runtime, &ticket, cx);
        Ok(())
    }

    /// 确认仍经过原生产模态与 Root 事件，driver 必须随后回读空容器列表。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_confirm_compose_down(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !self.right_rail_acceptance_compose_down_confirmation_ready(host_id, target)? {
            return Err("没有本轮 Compose fixture 的拆除确认".to_owned());
        }
        self.confirm_root_confirmation(cx);
        if self.compose_pending_down.is_some() || self.status.is_some() {
            return Err("生产 Compose 拆除确认未能提交".to_owned());
        }
        Ok(())
    }

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

/// 路径和项目名必须同时属于相同 run ID，禁止把验收动作转向其它 Compose 项目。
///
/// @author kongweiguang
fn validate_fixture_compose_target<'a>(
    host_id: &HostId,
    target: &'a ComposeProjectTarget,
) -> Result<&'a str, String> {
    let run_id = target
        .project
        .strip_prefix("kerminal-rr-compose-")
        .filter(|suffix| {
            suffix.len() == 12
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| "Compose fixture 项目名称不符合本轮隔离规则".to_owned())?;
    let root = format!("/run/kerminal-right-rail-{run_id}");
    if target.host_id.as_ref() != Some(host_id)
        || target.working_dir != root
        || target.config_files.len() != 1
        || target.config_files[0] != format!("{root}/compose.yml")
    {
        return Err("Compose fixture 宿主、目录或配置路径不匹配".to_owned());
    }
    Ok(run_id)
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
