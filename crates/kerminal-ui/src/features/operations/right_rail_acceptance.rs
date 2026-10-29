//! 仅供固定右栏原生验收使用的只读就绪检查。
//!
//! @author kongweiguang

#[path = "right_rail_acceptance_tmux_attach.rs"]
mod tmux_attach;

use kerminal_domain::{
    identity::HostId,
    operations::{ComposeProjectTarget, ContainerStatus, ServerInfoTarget},
};

use super::{
    ContainerOutputData, ContainerOutputKind, ContainerOutputTarget, ContainerPanelContext,
    ContainerReadKind, DetailReadPhase, OperationsSection, OperationsView, ToolRailOperationTarget,
    render::{
        container_panel::{ContainerHostPage, HostContainerTab},
        server::{ServerNetworkFilter, ServerProcessSort},
    },
    state::ServerInfoPanel,
    state::{SectionPhase, SectionState},
};

impl OperationsView {
    /// 只对当前固定 SSH 面板返回完成态，避免验收把 dashboard 或旧目标快照当作右栏结果。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_ready(
        &self,
        section: OperationsSection,
        host_id: &HostId,
    ) -> Result<bool, String> {
        self.validate_acceptance_target(section, host_id)?;
        if !self.active {
            return Ok(false);
        }

        match section {
            OperationsSection::ServerInfo => {
                let Some(snapshot) = acceptance_snapshot(&self.server, "服务器信息")? else {
                    return Ok(false);
                };
                if snapshot.target
                    != (ServerInfoTarget::Ssh {
                        host_id: host_id.clone(),
                    })
                {
                    return Err("服务器信息快照与验收 SSH 主机不一致".to_owned());
                }
                Ok(snapshot.sample_times.fast_at_unix_ms.is_some())
            }
            OperationsSection::Containers => {
                self.validate_container_host_context(host_id)?;
                let Some(rows) = acceptance_snapshot(&self.containers, "容器列表")? else {
                    return Ok(false);
                };
                if rows.iter().any(|row| {
                    row.target.host_id.as_ref() != Some(host_id)
                        || row.target.runtime != self.runtime
                }) {
                    return Err("容器列表快照与验收 SSH 主机或运行时不一致".to_owned());
                }
                Ok(true)
            }
            OperationsSection::Tmux => {
                let target = self.tmux_target();
                if !self.tmux_target_available()
                    || target.validate().is_err()
                    || target.host_id.as_ref() != Some(host_id)
                {
                    return Err("tmux 查询目标与验收 SSH 主机不一致".to_owned());
                }
                // TmuxSnapshot 不重复保存目标；固定视图在目标代次变化时会清空其 section。
                let Some(_snapshot) = acceptance_snapshot(&self.tmux, "tmux 会话")? else {
                    return Ok(false);
                };
                Ok(true)
            }
            OperationsSection::PortForwards => {
                // ManagedPortForwardSnapshot 是全局清单，可合法包含其他主机的转发定义。
                let Some(_snapshot) = acceptance_snapshot(&self.forwards, "SSH 隧道")? else {
                    return Ok(false);
                };
                Ok(true)
            }
        }
    }

    /// 切页仅依赖固定 SSH 目标和活动面板；同目标采样处于 Loading 时仍允许切换，截图仍由 Ready 快照门禁决定。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_select_server_page(
        &mut self,
        page: &str,
        host_id: &HostId,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        self.validate_acceptance_target(OperationsSection::ServerInfo, host_id)?;
        if !self.active {
            return Err("服务器信息验收面板当前未激活".to_owned());
        }
        if self.server_info_target
            != (ServerInfoTarget::Ssh {
                host_id: host_id.clone(),
            })
        {
            return Err("服务器信息查询目标与验收 SSH 主机不一致".to_owned());
        }
        let next = match page {
            "resources" => ServerInfoPanel::Resources,
            "processes" => ServerInfoPanel::Processes,
            _ => return Err("验收只支持 System 资源或进程页面".to_owned()),
        };
        if self.server_info_panel != next {
            match self.server_info_panel {
                ServerInfoPanel::Overview => {}
                ServerInfoPanel::Resources => {
                    self.server_network_filter = ServerNetworkFilter::Primary;
                }
                ServerInfoPanel::Processes => {
                    self.server_process_sort = ServerProcessSort::Cpu;
                }
            }
            self.server_info_panel = next;
        }
        cx.notify();
        Ok(())
    }

    /// 页面截图只在仍显示同一 SSH host 且当前生产页已切换成功后就绪。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_server_page_ready(
        &self,
        page: &str,
        host_id: &HostId,
    ) -> Result<bool, String> {
        let expected = match page {
            "resources" => ServerInfoPanel::Resources,
            "processes" => ServerInfoPanel::Processes,
            _ => return Err("验收只支持 System 资源或进程页面".to_owned()),
        };
        if self.server_info_panel != expected
            || !self.right_rail_acceptance_ready(OperationsSection::ServerInfo, host_id)?
        {
            return Ok(false);
        }
        let Some(snapshot) = acceptance_snapshot(&self.server, "服务器信息")? else {
            return Ok(false);
        };
        Ok(snapshot.sample_times.slow_at_unix_ms.is_some())
    }

    /// 按固定 fixture 名称从当前 host/runtime 快照选择真实运行容器，避免任意命中其它容器。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_container_target_id(
        &self,
        host_id: &HostId,
        fixture_container_name: &str,
    ) -> Result<Option<String>, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(None);
        }
        self.validate_container_host_context(host_id)?;
        Ok(self.containers.data().and_then(|rows| {
            rows.iter()
                .find(|row| {
                    row.target.host_id.as_ref() == Some(host_id)
                        && row.target.runtime == self.runtime
                        && row.name == fixture_container_name
                        && row.status == ContainerStatus::Running
                })
                .map(|row| row.target.container_id.clone())
        }))
    }

    /// 仅在固定 Containers SSH 面板仍绑定 fixture 宿主时回到生产容器列表页。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_return_to_container_list(
        &mut self,
        host_id: &HostId,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        self.validate_acceptance_target(OperationsSection::Containers, host_id)?;
        if !self.active {
            return Err("容器验收面板当前未激活".to_owned());
        }
        self.validate_container_host_context(host_id)?;
        self.container_host.navigate_to_list();
        cx.notify();
        Ok(())
    }

    /// 打开指定容器的真实 inspector 并通过现有 Root 读请求事件加载详情或监控快照。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_container_page(
        &mut self,
        host_id: &HostId,
        container_id: &str,
        page: &str,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Err("容器列表尚未绑定验收 SSH 主机".to_owned());
        }
        self.validate_container_host_context(host_id)?;
        let target = self
            .containers
            .data()
            .and_then(|rows| {
                rows.iter().find(|row| {
                    row.target.container_id == container_id
                        && row.target.host_id.as_ref() == Some(host_id)
                        && row.target.runtime == self.runtime
                        && row.status == ContainerStatus::Running
                })
            })
            .map(|row| row.target.clone())
            .ok_or_else(|| "验收容器已不在当前 SSH host 的运行容器列表中".to_owned())?;
        let (tab, kind) = match page {
            "details" => (HostContainerTab::Details, ContainerReadKind::Inspect),
            "stats" => (HostContainerTab::Stats, ContainerReadKind::Stats),
            _ => return Err("验收只支持容器详情或监控页面".to_owned()),
        };
        self.container_host.select_container(target.clone());
        self.container_host.set_container_tab(tab);
        self.container_info_tab = kind;
        let ticket = self.container_action_ticket(Some(target.clone()));
        self.request_container_read(target, kind, &ticket, cx);
        cx.notify();
        Ok(())
    }

    /// 验收 inspector 只接受仍属原 SSH host/runtime 的目标及完成回执，不把旧详情当新结果。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_container_page_ready(
        &self,
        host_id: &HostId,
        container_id: &str,
        page: &str,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(false);
        }
        self.validate_container_host_context(host_id)?;
        let Some(target) = self.containers.data().and_then(|rows| {
            rows.iter()
                .find(|row| {
                    row.target.container_id == container_id
                        && row.target.host_id.as_ref() == Some(host_id)
                        && row.target.runtime == self.runtime
                })
                .map(|row| &row.target)
        }) else {
            return Err("验收容器已从当前 SSH host 的权威列表中消失".to_owned());
        };
        if !matches!(
            self.container_host.page(),
            ContainerHostPage::Container { target: selected } if selected == target
        ) {
            return Ok(false);
        }
        match page {
            "details" => {
                if self.container_host.container_tab() != HostContainerTab::Details {
                    return Ok(false);
                }
                match self.container_inspect.phase() {
                    DetailReadPhase::Loading
                    | DetailReadPhase::Idle
                    | DetailReadPhase::Cancelled => Ok(false),
                    DetailReadPhase::Failed => Err(self
                        .container_inspect
                        .feedback()
                        .unwrap_or("容器详情读取失败")
                        .to_owned()),
                    DetailReadPhase::Ready => self
                        .container_inspect
                        .data()
                        .filter(|details| details.target == *target)
                        .map(|_| true)
                        .ok_or_else(|| "容器详情回执与验收目标不一致".to_owned()),
                }
            }
            "stats" => {
                if self.container_host.container_tab() != HostContainerTab::Stats {
                    return Ok(false);
                }
                let expected = ContainerOutputTarget {
                    target: target.clone(),
                    kind: ContainerOutputKind::Stats,
                };
                match self.container_output.phase() {
                    DetailReadPhase::Loading
                    | DetailReadPhase::Idle
                    | DetailReadPhase::Cancelled => Ok(false),
                    DetailReadPhase::Failed => Err(self
                        .container_output
                        .feedback()
                        .unwrap_or("容器监控读取失败")
                        .to_owned()),
                    DetailReadPhase::Ready => {
                        if self.container_output.target() != Some(&expected) {
                            return Err("容器监控回执与验收目标不一致".to_owned());
                        }
                        match self.container_output.data() {
                            Some(ContainerOutputData::Stats(stats)) if stats.target == *target => {
                                Ok(true)
                            }
                            _ => Err("容器监控回执缺少目标匹配的结构化指标".to_owned()),
                        }
                    }
                }
            }
            _ => Err("验收只支持容器详情或监控页面".to_owned()),
        }
    }

    /// Selects only the named fixture project; another project on the same Docker host is never a fallback.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_target(
        &self,
        host_id: &HostId,
        fixture_project: &str,
    ) -> Result<Option<ComposeProjectTarget>, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(None);
        }
        self.validate_container_host_context(host_id)?;
        Ok(self.containers.data().and_then(|rows| {
            rows.iter().find_map(|container| {
                let target = super::state::compose_target(container)?;
                (target.host_id.as_ref() == Some(host_id)
                    && target.runtime == self.runtime
                    && target.project == fixture_project)
                    .then_some(target)
            })
        }))
    }

    /// Opens Compose details through the same presentation state used by the production project row.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_select_compose_target(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Err("容器列表尚未绑定验收 SSH 主机".to_owned());
        }
        self.validate_container_host_context(host_id)?;
        if target.host_id.as_ref() != Some(host_id)
            || target.runtime != self.runtime
            || !self.containers.data().is_some_and(|rows| {
                rows.iter().any(|container| {
                    super::state::compose_target(container).as_ref() == Some(target)
                })
            })
        {
            return Err("Compose 项目不属于当前验收 SSH 主机快照".to_owned());
        }
        self.container_host.select_project(target.clone());
        self.compose_yaml.select_project(target);
        cx.notify();
        Ok(())
    }

    /// Revalidates the fixture project and invokes the same guarded request used by the production YAML action.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_request_compose_config(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        path: &str,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<bool, String> {
        if target.host_id.as_ref() != Some(host_id)
            || target.runtime != self.runtime
            || target.config_files.len() != 1
            || target.config_files.first().map(String::as_str) != Some(path)
        {
            return Err("Compose 配置路径与当前 fixture 项目不一致".to_owned());
        }
        if !self.right_rail_acceptance_compose_target_selected(host_id, target)? {
            return Ok(false);
        }

        let ticket = self.container_action_ticket(None);
        self.request_compose_config(
            target.host_id.clone(),
            path.to_owned(),
            Some(target.working_dir.clone()),
            &ticket,
            cx,
        );
        Ok(true)
    }

    /// Requires the same fixture project to remain selected after opening its YAML workspace tab.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_compose_target_selected(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Containers, host_id)? {
            return Ok(false);
        }
        self.validate_container_host_context(host_id)?;
        if !self.containers.data().is_some_and(|rows| {
            rows.iter()
                .any(|container| super::state::compose_target(container).as_ref() == Some(target))
        }) {
            return Err("Compose 项目已从当前 SSH 主机快照中消失".to_owned());
        }
        Ok(matches!(
            self.container_host.page(),
            ContainerHostPage::ComposeProject { target: selected } if selected == target
        ))
    }

    /// 读取当前 tmux 快照内首个属于其 capability target 的真实会话，拒绝跨目标展开。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_session_id(
        &self,
        host_id: &HostId,
    ) -> Result<Option<String>, String> {
        if !self.right_rail_acceptance_ready(OperationsSection::Tmux, host_id)? {
            return Ok(None);
        }
        let target = self.tmux_target();
        if target.host_id.as_ref() != Some(host_id) {
            return Err("tmux 验收目标已离开原 SSH host".to_owned());
        }
        let snapshot = self
            .tmux
            .data()
            .ok_or_else(|| "tmux 验收快照未就绪".to_owned())?;
        if !snapshot.capability.available {
            return Err(snapshot
                .capability
                .reason
                .clone()
                .unwrap_or_else(|| "验收 SSH host 未安装或无法启动 tmux".to_owned()));
        }
        Ok(snapshot
            .sessions
            .iter()
            .find(|session| session.target_ref == snapshot.capability.target_ref)
            .map(|session| session.id.clone()))
    }

    /// 每次鼠标展开前再次校验 host、tmux server 与 session 身份仍来自同一快照。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_tmux_session_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
    ) -> Result<bool, String> {
        let Some(current_id) = self.right_rail_acceptance_tmux_session_id(host_id)? else {
            return Ok(false);
        };
        Ok(current_id == session_id)
    }

    /// 通过生产 tmux 折叠状态机设定截图所需状态；不启动窗口详情或额外网络读取。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_set_tmux_session_expanded(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        expanded: bool,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_tmux_session_ready(host_id, session_id)? {
            return Err("验收 tmux session 已不属于当前 SSH host 快照".to_owned());
        }
        let mut current = self.tmux_panel.toggle_session_details(session_id);
        if current != expanded {
            current = self.tmux_panel.toggle_session_details(session_id);
        }
        cx.notify();
        Ok(current)
    }

    /// 拒绝 dashboard、其他 section、本机和失配 HostId，保证验收读取始终命中固定 SSH 目标。
    ///
    /// @author kongweiguang
    fn validate_acceptance_target(
        &self,
        section: OperationsSection,
        host_id: &HostId,
    ) -> Result<(), String> {
        if !self.section_selection.is_fixed() || self.section_selection.active() != section {
            return Err("验收 section 与当前固定右栏面板不一致".to_owned());
        }
        if self.host_target.as_ref() != Some(host_id)
            || !matches!(
                self.tool_rail_target.as_ref(),
                Some(ToolRailOperationTarget::Ssh { host_id: target }) if target == host_id
            )
        {
            return Err("验收 SSH 主机与当前右栏目标不一致".to_owned());
        }
        Ok(())
    }

    /// 容器验收桥共享同一宿主校验，避免生命周期与页面捕获对可用目标作出不同判断。
    ///
    /// @author kongweiguang
    pub(super) fn validate_container_host_context(&self, host_id: &HostId) -> Result<(), String> {
        match self.container_panel_context.as_ref() {
            Some(ContainerPanelContext::Host {
                host_id: Some(target),
                ..
            }) if target == host_id => Ok(()),
            Some(ContainerPanelContext::WorkspaceHost {
                host_id: target, ..
            }) if target == host_id => Ok(()),
            _ => Err("容器面板没有绑定验收 SSH 主机的 Host 上下文".to_owned()),
        }
    }
}

/// SectionPhase::Ready 由完成回执在释放取消票据后设置，因此同时证明快照存在且没有在途读取。
///
/// @author kongweiguang
fn acceptance_snapshot<'a, T>(
    state: &'a SectionState<T>,
    section_name: &str,
) -> Result<Option<&'a T>, String> {
    if let Some(error) = state.error() {
        return Err(format!("{section_name}读取失败：{error}"));
    }
    match state.phase() {
        SectionPhase::Ready => state
            .data()
            .map(Some)
            .ok_or_else(|| format!("{section_name}已标记完成但没有结果快照")),
        SectionPhase::Failed => Err(format!("{section_name}读取失败")),
        SectionPhase::NotLoaded | SectionPhase::Loading | SectionPhase::Cancelled => Ok(None),
    }
}
