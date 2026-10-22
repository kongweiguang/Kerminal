//! 运维按钮调用 application 用例并按目标世代应用回执。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{
    AppError, PublicErrorCode,
    operations::{
        ContainerLogs, PortForwardDefinition, ServerInfoQuery, ServerInfoTier, TmuxCapability,
        TmuxCreateRequest,
    },
};
use kerminal_domain::files::{FileEndpoint, RemotePath};
use kerminal_domain::operations::{
    ComposeAction, ComposeProjectTarget, ContainerListQuery, ContainerRuntime, ContainerTarget,
    PortForwardConfig, ServerInfoSnapshot, ServerInfoTarget, TmuxMutationReceipt, TmuxTarget,
};
use tokio_util::sync::CancellationToken;

use super::{
    ContainerPanelContext, ContainerReadKind, OperationsEvent, OperationsView, TmuxDetailState,
    container_panel::ContainerPanelActionTicket,
    forms::{ForwardFormTransition, container_command, forward_config, next_forward_form_open},
    state::{ContainerMutationRequest, LoadTicket, run_cancellable_request, run_section_request},
};

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct ServerMonitorTierRequest {
    ticket: LoadTicket,
    target: ServerInfoTarget,
    tier: ServerInfoTier,
    operations: std::sync::Arc<kerminal_application::operations::OperationsService>,
    executor: kerminal_application::ApplicationTaskExecutor,
}

impl OperationsView {
    /// 为每个写用例创建视图持有的取消令牌，使取消按钮与窗口销毁都能通知应用边界。
    ///
    /// @author kongweiguang
    fn begin_operation_mutation(&mut self) -> CancellationToken {
        let cancellation = CancellationToken::new();
        self.mutation_cancellation = Some(cancellation.clone());
        cancellation
    }

    /// 只有用例返回或确认取消后才释放 busy 状态，避免用户误以为远端命令已经停止。
    ///
    /// @author kongweiguang
    fn finish_operation_mutation(&mut self) {
        self.mutation_cancellation = None;
        self.busy = false;
    }

    /// 取消仅发出协作请求；命令结果无回执时仍需读取原目标确认实际状态。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_operation_mutation(&mut self, cx: &mut Context<Self>) {
        let Some(cancellation) = self.mutation_cancellation.as_ref() else {
            return;
        };
        cancellation.cancel();
        self.status = Some(
            crate::locale::text(
                "已请求取消；命令完成状态尚未确认，正在等待回执。",
                "Cancellation requested; waiting for the command result to confirm its state.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 保持单一串行后台 driver；把真实 `Option<Instant>` 交给判定器，避免布尔取反让已采样状态永久立即触发。
    pub(super) fn start_server_monitor(&mut self, cx: &mut Context<Self>) {
        if self.server_monitor_started || !self.active || !self.server_section_visible() {
            return;
        }
        self.server_monitor_started = true;
        let cancellation = self.server_monitor_cancellation.clone();
        cx.spawn(async move |this, cx| {
            let mut observed_revision = None;
            let mut observed_generation = None;
            let mut observed_immediate_revision = None;
            let mut observed_panel_active = false;
            let mut observed_section_visible = false;
            let mut last_sampled: Option<Instant> = None;
            loop {
                let Some(configuration) = this
                    .update(cx, |view, _| view.server_monitor_configuration())
                    .ok()
                else {
                    break;
                };
                if cancellation.is_cancelled() {
                    break;
                }
                if observed_revision != Some(configuration.revision) {
                    observed_revision = Some(configuration.revision);
                    if last_sampled.is_some() {
                        last_sampled = Some(Instant::now());
                    }
                }
                let immediate = server_sample_is_immediate(
                    &configuration,
                    observed_generation,
                    observed_immediate_revision,
                    observed_panel_active,
                    observed_section_visible,
                    last_sampled,
                );
                let interval = configuration.interval.period(configuration.panel_active);
                let due = immediate
                    || interval.is_some_and(|interval| {
                        last_sampled.map_or(configuration.panel_active, |sampled| {
                            sampled.elapsed() >= interval
                        })
                    });

                if due && configuration.server_section_visible {
                    let tiers = this
                        .update(cx, |view, _| {
                            view.server_monitor_tiers(
                                configuration.server_generation,
                                configuration.revision,
                            )
                        })
                        .ok()
                        .flatten();
                    if let Some(tiers) = tiers {
                        for tier in tiers {
                            let request = this
                                .update(cx, |view, _| {
                                    view.begin_server_monitor_tier(
                                        configuration.server_generation,
                                        configuration.revision,
                                        tier,
                                    )
                                })
                                .ok()
                                .flatten();
                            let Some(request) = request else {
                                break;
                            };
                            let ticket = request.ticket.clone();
                            let result = run_server_tier_request(request).await;
                            let continue_sampling = matches!(&result, Ok(Some(_)));
                            let _ = this.update(cx, |view, cx| {
                                view.apply_server_tier_result(ticket, result, cx);
                            });
                            if !continue_sampling {
                                break;
                            }
                        }
                    }
                    observed_generation = Some(configuration.server_generation);
                    observed_immediate_revision = Some(configuration.immediate_revision);
                    observed_panel_active = configuration.panel_active;
                    observed_section_visible = configuration.server_section_visible;
                    last_sampled = Some(Instant::now());
                    continue;
                }

                observed_generation = Some(configuration.server_generation);
                observed_immediate_revision = Some(configuration.immediate_revision);
                observed_panel_active = configuration.panel_active;
                observed_section_visible = configuration.server_section_visible;
                let wait = if !configuration.server_section_visible || interval.is_none() {
                    Duration::from_secs(24 * 60 * 60)
                } else {
                    interval
                        .unwrap_or(Duration::from_secs(24 * 60 * 60))
                        .saturating_sub(
                            last_sampled
                                .map(|sampled| sampled.elapsed())
                                .unwrap_or(Duration::ZERO),
                        )
                };
                let wake = configuration.wake.clone();
                let timer = cx.background_executor().timer(wait);
                match cancellation
                    .run_until_cancelled(wake.run_until_cancelled(timer))
                    .await
                {
                    None => break,
                    Some(None) => continue,
                    Some(Some(())) => {}
                }
            }
        })
        .detach();
    }

    /// 目标切换和自动采样按独立 server 世代处理，不借用容器、tmux 或端口请求世代。
    fn server_monitor_tiers(&self, generation: u64, revision: u64) -> Option<Vec<ServerInfoTier>> {
        if self.server_monitor_revision != revision
            || !self.server_section_visible()
            || self.server_generation != generation
            || self.server.phase() == super::state::SectionPhase::Loading
        {
            return None;
        }
        let current = self.server.data();
        let now = unix_now_ms();
        let mut tiers = vec![ServerInfoTier::Fast];
        if sampled_tier_expired(
            current.and_then(|snapshot| snapshot.sample_times.slow_at_unix_ms),
            now,
            Duration::from_secs(15),
        ) {
            tiers.push(ServerInfoTier::Slow);
        }
        if sampled_tier_expired(
            current.and_then(|snapshot| snapshot.sample_times.static_info_at_unix_ms),
            now,
            Duration::from_secs(300),
        ) {
            tiers.push(ServerInfoTier::Static);
        }
        Some(tiers)
    }

    /// 每个层级开始前复核目标和 Loading 状态，避免自动 tick 抢占显式刷新。
    fn begin_server_monitor_tier(
        &mut self,
        generation: u64,
        revision: u64,
        tier: ServerInfoTier,
    ) -> Option<ServerMonitorTierRequest> {
        if self.server_monitor_revision != revision
            || !self.server_section_visible()
            || self.server_generation != generation
            || self.server.phase() == super::state::SectionPhase::Loading
        {
            return None;
        }
        Some(ServerMonitorTierRequest {
            ticket: self.server.begin(generation),
            target: self.server_info_target.clone(),
            tier,
            operations: self.operations.clone(),
            executor: self.executor.clone(),
        })
    }

    /// 合并真实采样层级并只接受当前 server 目标票据的结果。
    fn apply_server_tier_result(
        &mut self,
        ticket: LoadTicket,
        result: kerminal_application::AppResult<Option<ServerInfoSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        if self.server_generation != ticket.target_generation || !self.server.is_current(&ticket) {
            return;
        }
        match result {
            Ok(Some(snapshot)) => {
                let merged = merge_server_info_snapshot(self.server.data().cloned(), snapshot);
                if !self
                    .server_history
                    .complete_if_current(&mut self.server, &ticket, merged)
                {
                    return;
                }
            }
            Ok(None) => {
                self.server.cancel(&ticket);
            }
            Err(error) => {
                self.server
                    .complete(&ticket, Err(error.public_message().to_owned()));
            }
        }
        cx.notify();
    }

    /// 返回当前连接主机或本机资源，避免容器详情改变运维面板的目标语义后无法回退。
    pub(super) fn show_host_server_info(&mut self, cx: &mut Context<Self>) {
        let target = self
            .host_target
            .as_ref()
            .map_or(ServerInfoTarget::Local, |host_id| ServerInfoTarget::Ssh {
                host_id: host_id.clone(),
            });
        self.select_server_info_target(target, cx);
    }

    /// 用户主动刷新失效所有层级缓存；后台自动刷新则由分层 driver 使用 TTL。
    pub(super) fn load_server(&mut self, cx: &mut Context<Self>) {
        if !self.active || !self.server_section_visible() {
            return;
        }
        let ticket = self.server.begin(self.server_generation);
        let cancel_request = ticket.cancellation.clone();
        let generation = self.server_generation;
        let target = self.server_info_target.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = run_section_request(executor, cancel_request, move |cancel| async move {
                operations.server_info.invalidate(&target).await;
                operations
                    .server_info
                    .snapshot(
                        ServerInfoQuery {
                            target,
                            tier: ServerInfoTier::Full,
                        },
                        cancel,
                    )
                    .await
            })
            .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.server_generation != generation || !view.server.is_current(&ticket) {
                        return;
                    }
                    match result {
                        Ok(Some(snapshot)) => {
                            if !view.server_history.complete_if_current(
                                &mut view.server,
                                &ticket,
                                snapshot,
                            ) {
                                return;
                            }
                        }
                        Ok(None) => {
                            view.server.cancel(&ticket);
                            cx.notify();
                            return;
                        }
                        Err(error) => {
                            view.server
                                .complete(&ticket, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 目标变化会单独取消服务器快照，dashboard 其他 section 不因切容器而作废。
    fn select_server_info_target(&mut self, target: ServerInfoTarget, cx: &mut Context<Self>) {
        if self.server_info_target == target {
            if self.active {
                self.load_server(cx);
            }
            return;
        }
        self.server_info_target = target;
        self.server_generation = self.server_generation.wrapping_add(1).max(1);
        self.server.reset(self.server_generation);
        self.server_history.reset();
        self.server_info_panel = super::state::ServerInfoPanel::Overview;
        self.publish_server_monitor_configuration();
        if self.active {
            self.load_server(cx);
        }
    }

    /// Docker/Podman 列表独立刷新且按完整 target 判定旧详情是否失效，失败仍保留原列表。
    /// @author kongweiguang
    /// 宿主模式才读取全量列表；ContainerInfo 和不可用 context 不会触发宿主查询。
    ///
    /// @author kongweiguang
    pub(super) fn load_containers(&mut self, cx: &mut Context<Self>) {
        if !self.active
            || matches!(
                self.container_panel_context.as_ref(),
                Some(ContainerPanelContext::Unavailable { .. })
                    | Some(ContainerPanelContext::ContainerInfo { .. })
            )
            || matches!(
                self.container_panel_context.as_ref(),
                Some(ContainerPanelContext::Host { host_id, .. }) if host_id != &self.host_target
            )
        {
            return;
        }
        let ticket = self.containers.begin(self.container_generation);
        let cancel_request = ticket.cancellation.clone();
        let generation = self.container_generation;
        let target_host = self.host_target.clone();
        let runtime = self.runtime;
        let panel_context = self.container_panel_context.clone();
        let query_host = target_host.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = run_section_request(executor, cancel_request, move |cancel| async move {
                let rows = operations
                    .containers
                    .list(
                        ContainerListQuery {
                            host_id: query_host.clone(),
                            runtime,
                            include_stopped: true,
                        },
                        cancel,
                    )
                    .await?;
                let projects = operations
                    .containers
                    .known_compose_projects(&query_host, runtime)?;
                Ok((rows, projects))
            })
            .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.container_generation != generation
                        || !super::container_panel::same_read_context_scope(
                            view.container_panel_context.as_ref(),
                            panel_context.as_ref(),
                        )
                    {
                        return;
                    }
                    match result {
                        Ok(Some((rows, projects))) => {
                            let inspect_target_missing =
                                view.container_inspect.target().is_some_and(|target| {
                                    !rows.iter().any(|row| &row.target == target)
                                });
                            let output_target_missing =
                                view.container_output.target().is_some_and(|target| {
                                    !rows.iter().any(|row| row.target == target.target)
                                });
                            if view.containers.complete(&ticket, Ok(rows)) {
                                view.compose_projects = projects;
                                if inspect_target_missing {
                                    view.container_inspect_request.cancel_pending();
                                    view.container_inspect.reset();
                                }
                                if output_target_missing {
                                    view.container_output_request.cancel_pending();
                                    view.container_output.reset();
                                    view.container_output_target_label = None;
                                }
                                view.dirty_targets
                                    .container_refreshed(&target_host, runtime);
                            }
                        }
                        Ok(None) => {
                            view.containers.cancel(&ticket);
                            cx.notify();
                            return;
                        }
                        Err(error) => {
                            view.containers
                                .complete(&ticket, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// tmux 的探测和 session 查询只影响 tmux 区域；未安装时显示能力原因而非伪装空成功。
    /// 接纳新快照后复核待确认会话身份，已退出或重建的会话必须重新选择。
    ///
    /// @author kongweiguang
    pub(super) fn load_tmux(&mut self, cx: &mut Context<Self>) {
        if !self.tmux_target_available() {
            return;
        }
        let ticket = self.tmux.begin(self.target_generation);
        let cancel_request = ticket.cancellation.clone();
        let generation = self.target_generation;
        let target = self.tmux_target();
        let target_host = target.host_id.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = run_section_request(executor, cancel_request, move |cancel| async move {
                let capability = operations
                    .tmux
                    .probe(target.clone(), cancel.child_token())
                    .await?;
                let sessions = if capability.available {
                    operations.tmux.list_sessions(target, cancel).await?
                } else {
                    Vec::new()
                };
                Ok(super::TmuxSnapshot {
                    capability,
                    sessions,
                })
            })
            .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.target_generation != generation {
                        return;
                    }
                    match result {
                        Ok(Some(snapshot)) => {
                            let selected_session_missing = view
                                .tmux_details
                                .session_id
                                .as_ref()
                                .is_some_and(|session_id| {
                                    !snapshot
                                        .sessions
                                        .iter()
                                        .any(|session| &session.id == session_id)
                                });
                            if view.tmux.complete(&ticket, Ok(snapshot)) {
                                if selected_session_missing {
                                    view.tmux_details.cancel_pending_reads();
                                    let generation =
                                        view.tmux_details.generation.wrapping_add(1).max(1);
                                    view.tmux_details = TmuxDetailState {
                                        generation,
                                        ..TmuxDetailState::default()
                                    };
                                }
                                view.dirty_targets.tmux_refreshed(&target_host);
                                view.reconcile_tmux_dialog_session(cx);
                            }
                        }
                        Ok(None) => {
                            view.tmux.cancel(&ticket);
                            cx.notify();
                            return;
                        }
                        Err(error) => {
                            view.tmux
                                .complete(&ticket, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 端口转发清单来自同一 runtime service，单独刷新不会受 Docker 或 tmux 错误影响。
    pub(super) fn load_forwards(&mut self, cx: &mut Context<Self>) {
        if !self.active || !self.section_selection.watches_forwards() {
            return;
        }
        if !self.forward_watch_started {
            self.start_forward_watch(cx);
        }
        let ticket = self.forwards.begin(self.target_generation);
        let cancel_request = ticket.cancellation.clone();
        let generation = self.target_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = run_section_request(executor, cancel_request, move |_| async move {
                operations.port_forwards.list_managed().await
            })
            .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.target_generation != generation {
                        return;
                    }
                    match result {
                        Ok(Some(rows)) => {
                            view.forwards.complete(&ticket, Ok(rows));
                            view.sync_proxy_forward_projection(cx);
                        }
                        Ok(None) => {
                            view.forwards.cancel(&ticket);
                            cx.notify();
                            return;
                        }
                        Err(error) => {
                            view.forwards
                                .complete(&ticket, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 在应用执行器中订阅合并后的转发 revision，并在实体仍存活时请求当前全局快照。
    pub(super) fn start_forward_watch(&mut self, cx: &mut Context<Self>) {
        if !self.active || !self.section_selection.watches_forwards() || self.forward_watch_started
        {
            return;
        }
        self.forward_watch_started = true;
        self.forward_watch_generation = self.forward_watch_generation.wrapping_add(1).max(1);
        let watch_generation = self.forward_watch_generation;
        let mut changes = self.operations.port_forwards.subscribe_changes();
        let cancellation = self.forward_watch_cancellation.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let task_cancellation = cancellation.clone();
                let mut pending_changes = changes;
                let next = executor
                    .clone()
                    .run(move |application_cancellation| async move {
                        let changed = application_cancellation
                            .run_until_cancelled(
                                task_cancellation.run_until_cancelled(pending_changes.changed()),
                            )
                            .await;
                        match changed {
                            Some(Some(Ok(()))) => {
                                let _ = pending_changes.borrow_and_update();
                                Ok(pending_changes)
                            }
                            Some(Some(Err(error))) => Err(AppError::infrastructure_with_source(
                                "端口转发状态订阅已关闭",
                                error,
                            )),
                            _ => Err(AppError::cancelled("端口转发状态订阅已取消")),
                        }
                    })
                    .await;
                match next {
                    Ok(receiver) => changes = receiver,
                    Err(error) => {
                        if !cancellation.is_cancelled() {
                            let message = error.public_message().to_owned();
                            cx.update(|app| {
                                let _ = this.update(app, |view, cx| {
                                    if view.forward_watch_generation == watch_generation
                                        && view.active
                                        && view.section_selection.watches_forwards()
                                    {
                                        view.forward_watch_started = false;
                                        view.forward_watch_cancellation.cancel();
                                        view.forward_watch_cancellation = CancellationToken::new();
                                        view.forward_watch_generation =
                                            view.forward_watch_generation.wrapping_add(1).max(1);
                                        view.status = Some(message);
                                        cx.notify();
                                    }
                                });
                            });
                        }
                        break;
                    }
                }
                let updated = cx.update(|app| {
                    this.update(app, |view, cx| {
                        if view.forward_watch_generation == watch_generation
                            && view.active
                            && view.section_selection.watches_forwards()
                        {
                            view.load_forwards(cx);
                        }
                    })
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 隐藏或离开转发区时撤销 watcher；新一轮激活使用新 token 与订阅。
    pub(super) fn stop_forward_watch(&mut self) {
        self.forward_watch_cancellation.cancel();
        self.forward_watch_cancellation = CancellationToken::new();
        self.forward_watch_started = false;
        self.forward_watch_generation = self.forward_watch_generation.wrapping_add(1).max(1);
    }

    /// Runtime 只影响容器 section；tmux、服务器信息和转发保留各自的快照与请求。
    pub(super) fn select_runtime(&mut self, runtime: ContainerRuntime, cx: &mut Context<Self>) {
        if !self.active
            || matches!(
                self.container_panel_context.as_ref(),
                Some(ContainerPanelContext::Unavailable { .. })
                    | Some(ContainerPanelContext::ContainerInfo { .. })
            )
            || self.runtime == runtime
        {
            return;
        }
        self.runtime = runtime;
        self.container_generation = self.container_generation.wrapping_add(1).max(1);
        self.containers.reset(self.container_generation);
        self.compose_projects.clear();
        self.compose_pending_down = None;
        self.compose_pending_context = None;
        self.confirmed_compose_down = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        self.container_inspect_request.cancel_pending();
        self.container_output_request.cancel_pending();
        self.container_inspect.reset();
        self.container_output.reset();
        self.container_output_target_label = None;
        self.status = None;
        self.container_pending_remove = None;
        self.load_containers(cx);
    }

    /// 固定容器工具的行内写操作先冻结 context，由 Root live-context gate 后再进入应用用例。
    ///
    /// @author kongweiguang
    pub(super) fn request_container_mutation(
        &mut self,
        request: ContainerMutationRequest,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, Some(&request.target))
            || !self.container_mutation_allowed(&request.target)
        {
            return;
        }
        if let Some(context) = self.container_panel_context.clone() {
            cx.emit(OperationsEvent::ContainerMutationRequested { context, request });
        } else {
            self.mutate_container(request, cx);
        }
    }

    /// 固定右栏沿用 Root 校验的 host/profile；完整运维页只使用用户显式选择的 host。
    ///
    /// @author kongweiguang
    pub(super) fn tmux_target(&self) -> TmuxTarget {
        if self.is_fixed_tmux_view() {
            match self.tool_rail_target.as_ref() {
                Some(super::ToolRailOperationTarget::Ssh { host_id }) => {
                    if let Some(binding) = self
                        .tmux_attach_binding
                        .as_ref()
                        .filter(|binding| binding.target.host_id() == Some(host_id))
                    {
                        return binding.target.to_tmux_target();
                    }
                    return TmuxTarget {
                        host_id: Some(host_id.clone()),
                        ..TmuxTarget::default()
                    };
                }
                Some(super::ToolRailOperationTarget::Local) => {
                    if let Some(binding) = self
                        .tmux_attach_binding
                        .as_ref()
                        .filter(|binding| binding.target.host_id().is_none())
                    {
                        return binding.target.to_tmux_target();
                    }
                }
                Some(super::ToolRailOperationTarget::Unavailable) | None => {}
            }
        }
        TmuxTarget {
            host_id: self.host_target.clone(),
            ..TmuxTarget::default()
        }
    }

    /// 容器写入结果始终归属原 host/runtime；force 与目标同批冻结，取消后等待命令 worker 回执再释放 busy。
    ///
    /// @author kongweiguang
    pub(super) fn mutate_container(
        &mut self,
        request: ContainerMutationRequest,
        cx: &mut Context<Self>,
    ) {
        if self.busy || !self.active || !self.container_mutation_allowed(&request.target) {
            return;
        }
        let ContainerMutationRequest {
            target,
            action,
            force,
        } = request;
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let target_host = target.host_id.clone();
        let target_runtime = target.runtime;
        let operation_context = self.container_panel_context.clone();
        let operation_generation = self.container_generation;
        self.container_pending_remove = None;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations
                        .containers
                        .mutate_with_force(target, action, force, cancel)
                        .await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let target_is_current = view.dirty_targets.container_mutation_completed(
                        target_host.clone(),
                        target_runtime,
                        &view.host_target,
                        view.runtime,
                    );
                    let panel_is_current = view.container_panel_context == operation_context
                        && view.container_generation == operation_generation;
                    let message = match result {
                        Ok(receipt) if receipt.refresh_required => crate::locale::text(
                            "容器操作完成；运行态刷新可重新读取。",
                            "Container action completed; refresh the runtime state to verify it.",
                        )
                        .to_string(),
                        Ok(receipt) if receipt.applied => {
                            crate::locale::text("容器操作已完成。", "Container action completed.")
                                .to_string()
                        }
                        Ok(_) => crate::locale::text(
                            "容器没有发生变更。",
                            "No container change was applied.",
                        )
                        .to_string(),
                        Err(error) => mutation_error_message(error, cancellation_requested),
                    };
                    if target_is_current && panel_is_current {
                        view.status = Some(message);
                        view.load_containers(cx);
                    } else {
                        view.remember_container_feedback(
                            target_host.clone(),
                            target_runtime,
                            message,
                        );
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// inspect 按容器身份保存最后成功投影；刷新、失败或隐藏取消时保留可辨识的数据。
    ///
    /// @author kongweiguang
    pub(super) fn inspect_container(&mut self, target: ContainerTarget, cx: &mut Context<Self>) {
        if !self.active || !self.container_read_allowed(&target) {
            return;
        }
        let host_generation = self.target_generation;
        let container_generation = self.container_generation;
        let panel_context = self.container_panel_context.clone();
        let detail_target = target.clone();
        let ticket = self.container_inspect_request.begin();
        let operation_cancellation = ticket.cancellation.clone();
        let completed_ticket = ticket.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        self.container_inspect.begin(detail_target.clone());
        cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(move |app_cancel| {
                        run_cancellable_request(
                            app_cancel,
                            operation_cancellation,
                            move |cancel| async move {
                                operations.containers.inspect(target, cancel).await
                            },
                        )
                    })
                    .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.target_generation != host_generation
                        || view.container_generation != container_generation
                        || !super::container_panel::same_read_context_scope(
                            view.container_panel_context.as_ref(),
                            panel_context.as_ref(),
                        )
                        || !view.container_inspect_request.is_current(&completed_ticket)
                    {
                        return;
                    }
                    view.container_inspect_request.complete(&completed_ticket);
                    match result {
                        Ok(Some(details)) => {
                            view.container_inspect.complete(&detail_target, Ok(details));
                        }
                        Ok(None) => {
                            view.container_inspect.cancel(
                                crate::locale::text(
                                    "读取已取消，可再次点击“概览”重试。",
                                    "Read cancelled; click Overview to retry.",
                                )
                                .to_string(),
                            );
                        }
                        Err(error) => {
                            view.container_inspect
                                .complete(&detail_target, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 日志和 stats 按精确容器与查询类型保留最近快照，防止切换类型时展示错对象。
    ///
    /// @author kongweiguang
    pub(super) fn inspect_container_output(
        &mut self,
        target: ContainerTarget,
        logs: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.active || !self.container_read_allowed(&target) {
            return;
        }
        let name = self
            .containers
            .data()
            .and_then(|containers| containers.iter().find(|item| item.target == target))
            .map(|container| container.name.as_str());
        self.container_output_target_label = Some(format!(
            "{} · {}",
            if logs {
                crate::locale::text("容器日志", "Container logs")
            } else {
                crate::locale::text("容器资源", "Container stats")
            },
            container_output_target_label(&target, name),
        ));
        let host_generation = self.target_generation;
        let container_generation = self.container_generation;
        let panel_context = self.container_panel_context.clone();
        let output_target = super::ContainerOutputTarget {
            target: target.clone(),
            kind: if logs {
                super::ContainerOutputKind::Logs
            } else {
                super::ContainerOutputKind::Stats
            },
        };
        let ticket = self.container_output_request.begin();
        let operation_cancellation = ticket.cancellation.clone();
        let completed_ticket = ticket.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        self.container_output.begin(output_target.clone());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |app_cancel| {
                    run_cancellable_request(
                        app_cancel,
                        operation_cancellation,
                        move |cancel| async move {
                            if logs {
                                let output: ContainerLogs =
                                    operations.containers.logs(target, 200, cancel).await?;
                                let truncation = if output.truncated {
                                    crate::locale::text(
                                        "\n（日志输出已截断）",
                                        "\n(Log output truncated)",
                                    )
                                    .to_string()
                                } else {
                                    String::new()
                                };
                                Ok(super::render::bounded_text(
                                    &format!("{}\n{}{}", output.stdout, output.stderr, truncation),
                                    24_000,
                                ))
                            } else {
                                let stats = operations.containers.stats(target, cancel).await?;
                                Ok(format!(
                                    "CPU {} · {} {} / {} · {} {} · {} {} · PIDs {}",
                                    stats.cpu_percent.as_deref().unwrap_or("—"),
                                    crate::locale::text("内存", "memory"),
                                    stats.memory_usage.as_deref().unwrap_or("—"),
                                    stats.memory_percent.as_deref().unwrap_or("—"),
                                    crate::locale::text("网络", "network"),
                                    stats.network_io.as_deref().unwrap_or("—"),
                                    crate::locale::text("磁盘", "disk"),
                                    stats.block_io.as_deref().unwrap_or("—"),
                                    stats.pids.as_deref().unwrap_or("—"),
                                ))
                            }
                        },
                    )
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.target_generation != host_generation
                        || view.container_generation != container_generation
                        || !super::container_panel::same_read_context_scope(
                            view.container_panel_context.as_ref(),
                            panel_context.as_ref(),
                        )
                        || !view.container_output_request.is_current(&completed_ticket)
                    {
                        return;
                    }
                    view.container_output_request.complete(&completed_ticket);
                    match result {
                        Ok(Some(details)) => {
                            view.container_output.complete(&output_target, Ok(details));
                        }
                        Ok(None) => {
                            view.container_output.cancel(
                                crate::locale::text(
                                    "读取已取消，可再次点击日志或资源按钮重试。",
                                    "Read cancelled; click Logs or Resources to retry.",
                                )
                                .to_string(),
                            );
                        }
                        Err(error) => {
                            view.container_output
                                .complete(&output_target, Err(error.public_message().to_owned()));
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 按钮/分页读取在容器面板中先发事件，由 Root 刷新 pane/session 身份后再提交读取。
    ///
    /// @author kongweiguang
    pub(super) fn request_container_read(
        &mut self,
        target: ContainerTarget,
        kind: ContainerReadKind,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, Some(&target))
            || !self.container_read_allowed(&target)
        {
            return;
        }
        if let Some(context) = self.container_panel_context.clone() {
            cx.emit(OperationsEvent::ContainerReadRequested {
                context,
                target,
                kind,
            });
        } else {
            self.submit_container_read(target, kind, cx);
        }
    }
    /// 拆除入口始终只申请根层确认，重复点击不能把已存在的确认视为用户同意。
    pub(super) fn mutate_compose(
        &mut self,
        target: ComposeProjectTarget,
        action: ComposeAction,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if self.busy
            || !self.container_action_ticket_is_current(ticket, None)
            || !matches!(
                self.container_panel_context,
                None | Some(ContainerPanelContext::Host { .. })
            )
            || self.host_target != target.host_id
            || self.runtime != target.runtime
        {
            return;
        }
        if action == ComposeAction::Down {
            self.forward_pending_delete = None;
            self.compose_pending_down = Some(target);
            self.compose_pending_context = self.container_panel_context.clone();
            cx.emit(OperationsEvent::RootConfirmationChanged);
            cx.notify();
            return;
        }
        if let Some(context) = self.container_panel_context.clone() {
            cx.emit(OperationsEvent::ComposeMutationRequested {
                context,
                target,
                action,
            });
        } else {
            self.execute_compose(target, action, cx);
        }
    }

    /// Compose 回执绑定原 host/runtime；仅根确认可到达 Down，取消后仍等 worker 收尾并刷新原目标。
    /// @author kongweiguang
    pub(super) fn execute_compose(
        &mut self,
        target: ComposeProjectTarget,
        action: ComposeAction,
        cx: &mut Context<Self>,
    ) {
        if self.busy
            || !self.active
            || !matches!(
                self.container_panel_context,
                None | Some(ContainerPanelContext::Host { .. })
            )
            || self.host_target != target.host_id
            || self.runtime != target.runtime
        {
            return;
        }
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let target_host = target.host_id.clone();
        let target_runtime = target.runtime;
        let operation_context = self.container_panel_context.clone();
        let operation_generation = self.container_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations
                        .containers
                        .compose_action(target, action, cancel)
                        .await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let target_is_current = view.dirty_targets.container_mutation_completed(
                        target_host.clone(),
                        target_runtime,
                        &view.host_target,
                        view.runtime,
                    );
                    let panel_is_current = view.container_panel_context == operation_context
                        && view.container_generation == operation_generation;
                    let message = match result {
                        Ok(receipt) if receipt.refresh_required => crate::locale::text(
                            "Compose 命令完成；回到此宿主时将重新读取状态。",
                            "Compose action completed; the host state will be reloaded when you return.",
                        )
                        .to_string(),
                        Ok(_) => crate::locale::text(
                            "Compose 操作已完成。",
                            "Compose action completed.",
                        )
                        .to_string(),
                        Err(error) => mutation_error_message(error, cancellation_requested),
                    };
                    if target_is_current && panel_is_current {
                        view.status = Some(message);
                        view.load_containers(cx);
                    } else {
                        view.remember_container_feedback(
                            target_host.clone(),
                            target_runtime,
                            message,
                        );
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 文件入口只发布容器 target 与 POSIX 起始目录，由 Root 组合统一 FileService。
    /// 文件动作只从当前 Host 行发出，并把 context 一并交给 Root 再做 live binding 检查。
    ///
    /// @author kongweiguang
    pub(super) fn request_container_files(
        &self,
        target: ContainerTarget,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, Some(&target))
            || !self.container_read_allowed(&target)
            || matches!(
                self.container_panel_context,
                Some(ContainerPanelContext::ContainerInfo { .. })
                    | Some(ContainerPanelContext::Unavailable { .. })
            )
        {
            return;
        }
        cx.emit(OperationsEvent::OpenContainerFilesRequested {
            container_panel_context: self.container_panel_context.clone(),
            target,
            path: "/".to_owned(),
        });
    }

    /// Compose 配置路径按真实 host 类型构造成文件端点，事件由 Root 交给 FilesPane 校验后预览。
    pub(super) fn request_compose_config(
        &mut self,
        host_id: Option<kerminal_domain::identity::HostId>,
        path: String,
        working_directory: Option<String>,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, None)
            || !matches!(
                self.container_panel_context,
                None | Some(ContainerPanelContext::Host { .. })
            )
            || self.host_target != host_id
        {
            return;
        }
        let Some((endpoint, selection_name)) =
            compose_config_endpoint(host_id, &path, working_directory.as_deref())
        else {
            self.status = Some(
                crate::locale::text(
                    "Compose 配置路径不是可定位的主机文件。",
                    "The Compose config path is not a resolvable host file.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        cx.emit(OperationsEvent::OpenComposeConfigRequested {
            container_panel_context: self.container_panel_context.clone(),
            endpoint,
            selection_name,
        });
    }

    /// 速查发送同时携带 Root 确认的 pane owner/generation；未绑定时只保留复制能力。
    ///
    /// @author kongweiguang
    pub(super) fn send_tmux_quickref(
        &mut self,
        item: super::quickref::TmuxQuickrefItem,
        cx: &mut Context<Self>,
    ) {
        if !self.tmux_target_available() {
            return;
        }
        let Some(session_id) = self.tmux_details.session_id.clone() else {
            self.status = Some(
                crate::locale::text(
                    "先选择一个 tmux session，再发送快捷内容。",
                    "Select a tmux session before sending quickref content.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(binding) = self.tmux_quickref_binding.clone() else {
            self.status = Some(
                crate::locale::text(
                    "没有已确认的 tmux 终端目标；可以复制内容后自行粘贴。",
                    "No confirmed tmux pane is available; copy the item and paste it yourself.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        cx.emit(OperationsEvent::SendTmuxQuickrefRequested {
            pane_id: binding.pane_id,
            binding: binding.binding,
            target: self.tmux_target(),
            session_id,
            text: item.payload.to_owned(),
            kind: item.kind,
        });
        self.status = Some(
            crate::locale::text(
                "已请求发送到所选 tmux session；请核对目标终端回显。",
                "Send requested for the selected tmux session; verify the target terminal output.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 容器 exec 输入先形成程序和逐行 argv，再由 Root 分配共享 SessionService 的 UI owner。
    pub(super) fn request_container_exec(
        &mut self,
        target: ContainerTarget,
        ticket: &ContainerPanelActionTicket,
        cx: &mut Context<Self>,
    ) {
        if !self.container_action_ticket_is_current(ticket, Some(&target))
            || !self.container_read_allowed(&target)
            || !matches!(
                self.container_panel_context,
                None | Some(ContainerPanelContext::Host { .. })
            )
            || self.containers.data().is_some_and(|rows| {
                rows.iter().all(|row| {
                    row.target != target
                        || row.status != kerminal_domain::operations::ContainerStatus::Running
                })
            })
        {
            return;
        }
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let program = self.input_value(&inputs.exec_program, cx);
        let arguments = self.argument_values(cx);
        let command = if program.trim().is_empty()
            && matches!(
                self.container_panel_context,
                Some(ContainerPanelContext::Host { .. })
            ) {
            // 宿主紧凑行提供常用 shell 入口；显式填写程序时仍保留原 argv 校验。
            Ok(vec!["sh".to_owned()])
        } else {
            container_command(&program, arguments)
        };
        match command {
            Ok(command) => {
                self.status = None;
                cx.emit(OperationsEvent::ContainerExecRequested {
                    container_panel_context: self.container_panel_context.clone(),
                    target,
                    command,
                });
            }
            Err(error) => self.status = Some(error.message().to_owned()),
        }
        cx.notify();
    }

    /// 新 session 仅继承同一 host/profile pane 的工作目录；弹窗在提交瞬间复核冻结目标，成功回执才关闭。
    ///
    /// @author kongweiguang
    pub(super) fn create_tmux_session(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.ensure_tmux_dialog_current(cx) {
            return;
        }
        let target_available = self.tmux_target_available();
        let capability = self.tmux.data().map(|snapshot| &snapshot.capability);
        if !tmux_session_creation_enabled(self.busy, target_available, capability) {
            if !self.busy && target_available {
                self.status = Some(
                    crate::locale::text(
                        "请先重新读取 tmux 并确认目标可用后再创建 session。",
                        "Reload tmux and confirm it is available before creating a session.",
                    )
                    .to_string(),
                );
                cx.notify();
            }
            return;
        }
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let name = self.input_value(&inputs.tmux_name, cx).trim().to_owned();
        let working_directory = self
            .input_value(&inputs.tmux_working_directory, cx)
            .trim()
            .to_owned();
        if name.is_empty() {
            self.status = Some(
                crate::locale::text("请填写 tmux session 名称。", "Enter a tmux session name.")
                    .to_string(),
            );
            cx.notify();
            return;
        }
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let target = self.tmux_target();
        let working_directory = (!working_directory.is_empty())
            .then_some(working_directory)
            .or_else(|| {
                self.tmux_attach_binding
                    .as_ref()
                    .filter(|binding| binding.target.matches_tmux_target(&target))
                    .and_then(|binding| binding.working_directory.clone())
            });
        let request = TmuxCreateRequest {
            target,
            name,
            working_directory,
        };
        let target_host = request.target.host_id.clone();
        let tmux_view_generation = self.tmux_view_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations.tmux.create_session(request, cancel).await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let disposition = view.dirty_targets.tmux_mutation_completed(
                        target_host.clone(),
                        &view.host_target,
                        tmux_view_generation,
                        view.tmux_view_generation,
                    );
                    if disposition.update_current_view {
                        if result.is_ok() {
                            view.clear_tmux_dialog();
                            cx.emit(OperationsEvent::RootConfirmationChanged);
                        }
                        view.status = Some(match result {
                            Ok(receipt) => mutation_message(Ok(receipt)),
                            Err(error) => mutation_error_message(error, cancellation_requested),
                        });
                    }
                    if disposition.refresh_current_host {
                        view.load_tmux(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 同一 session 重试保留已有窗口、pane 和 capture 快照；切换 session 才清空旧目标数据。
    ///
    /// @author kongweiguang
    pub(super) fn select_tmux_session(
        &mut self,
        session_id: String,
        session_name: String,
        cx: &mut Context<Self>,
    ) {
        if !self.tmux_target_available() {
            return;
        }
        let same_session = self.tmux_details.session_id.as_deref() == Some(session_id.as_str());
        self.tmux_details.cancel_pending_reads();
        if same_session {
            self.tmux_details.session_name = Some(session_name.clone());
        } else {
            let generation = self.tmux_details.generation.wrapping_add(1).max(1);
            self.tmux_details = TmuxDetailState {
                session_id: Some(session_id.clone()),
                session_name: Some(session_name),
                generation,
                ..TmuxDetailState::default()
            };
        }
        let generation = self.tmux_details.generation.wrapping_add(1).max(1);
        self.tmux_details.generation = generation;
        self.tmux_details.windows_loading = true;
        self.tmux_details.windows_error = None;
        let cancellation = CancellationToken::new();
        self.tmux_details.cancellation = Some(cancellation.clone());
        let target = self.tmux_target();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |app_cancel| {
                    run_cancellable_request(app_cancel, cancellation, move |cancel| async move {
                        operations.tmux.list_windows(target, session_id, cancel).await
                    })
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.tmux_details.generation != generation {
                        return;
                    }
                    view.tmux_details.windows_loading = false;
                    view.tmux_details.cancellation = None;
                    match result {
                        Ok(Some(windows)) => {
                            let selected_window_exists = view
                                .tmux_details
                                .pane_target_window_id
                                .as_ref()
                                .is_none_or(|target_id| windows.iter().any(|window| &window.id == target_id));
                            if !selected_window_exists {
                                view.tmux_details.pane_target_window_id = None;
                                view.tmux_details.panes = None;
                                view.tmux_details.panes_error = None;
                                view.tmux_details.capture_target_pane_id = None;
                                view.tmux_details.capture = None;
                                view.tmux_details.capture_error = None;
                            }
                            view.tmux_details.windows = Some(windows);
                            view.tmux_details.windows_error = None;
                        }
                        Ok(None) => {
                            view.tmux_details.windows_error = Some(crate::locale::text(
                                "读取已取消，可再次点击当前 session 的“窗口 / 分屏”重试。",
                                "Read cancelled; click this session's Windows / panes action to retry.",
                            ).to_string());
                        }
                        Err(error) => {
                            view.tmux_details.windows_error =
                                Some(error.public_message().to_owned());
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 同一窗口重试保留上次 pane/capture；窗口改变时清空旧快照并绑定新的窗口 ID。
    ///
    /// @author kongweiguang
    pub(super) fn load_tmux_panes(&mut self, target_id: String, cx: &mut Context<Self>) {
        if !self.tmux_target_available() {
            return;
        }
        let same_window =
            self.tmux_details.pane_target_window_id.as_deref() == Some(target_id.as_str());
        self.tmux_details.cancel_pending_reads();
        if !same_window {
            self.tmux_details.panes = None;
            self.tmux_details.capture = None;
            self.tmux_details.capture_error = None;
            self.tmux_details.capture_target_pane_id = None;
        }
        let generation = self.tmux_details.generation.wrapping_add(1).max(1);
        self.tmux_details.generation = generation;
        self.tmux_details.pane_target_window_id = Some(target_id.clone());
        self.tmux_details.capture_request.cancel_pending();
        let cancellation = CancellationToken::new();
        self.tmux_details.cancellation = Some(cancellation.clone());
        self.tmux_details.panes_loading = true;
        self.tmux_details.panes_error = None;
        self.tmux_details.capture_loading = false;
        let target = self.tmux_target();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |app_cancel| {
                    run_cancellable_request(app_cancel, cancellation, move |cancel| async move {
                        operations.tmux.list_panes(target, target_id, cancel).await
                    })
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.tmux_details.generation != generation {
                        return;
                    }
                    view.tmux_details.panes_loading = false;
                    view.tmux_details.cancellation = None;
                    match result {
                        Ok(Some(panes)) => {
                            let selected_pane_exists = view
                                .tmux_details
                                .capture_target_pane_id
                                .as_ref()
                                .is_none_or(|target_id| {
                                    panes.iter().any(|pane| &pane.id == target_id)
                                });
                            if !selected_pane_exists {
                                view.tmux_details.capture_target_pane_id = None;
                                view.tmux_details.capture = None;
                                view.tmux_details.capture_error = None;
                            }
                            view.tmux_details.panes = Some(panes);
                        }
                        Ok(None) => {
                            view.tmux_details.panes_error = Some(
                                crate::locale::text(
                                    "分屏读取已取消，可再次点击当前窗口重试。",
                                    "Pane list cancelled; click the current window to retry.",
                                )
                                .to_string(),
                            );
                        }
                        Err(error) => {
                            view.tmux_details.panes_error = Some(error.public_message().to_owned())
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Capture 不自动补发；同一 pane 刷新时保留上次结果，切换 pane 才移除旧输出。
    ///
    /// @author kongweiguang
    pub(super) fn capture_tmux_pane(&mut self, pane_id: String, cx: &mut Context<Self>) {
        if !self.tmux_target_available() {
            return;
        }
        let same_pane =
            self.tmux_details.capture_target_pane_id.as_deref() == Some(pane_id.as_str());
        if !same_pane {
            self.tmux_details.capture = None;
        }
        self.tmux_details.capture_target_pane_id = Some(pane_id.clone());
        let target = self.tmux_target();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        let generation = self.tmux_details.generation;
        let ticket = self.tmux_details.capture_request.begin();
        let operation_cancellation = ticket.cancellation.clone();
        let completed_ticket = ticket.clone();
        self.tmux_details.capture_loading = true;
        self.tmux_details.capture_error = None;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |app_cancel| {
                    run_cancellable_request(
                        app_cancel,
                        operation_cancellation,
                        move |cancel| async move {
                            operations.tmux.capture_pane(target, pane_id, 200, cancel).await
                        },
                    )
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    if view.tmux_details.generation != generation
                        || !view.tmux_details.capture_request.is_current(&completed_ticket)
                    {
                        return;
                    }
                    view.tmux_details.capture_request.complete(&completed_ticket);
                    view.tmux_details.capture_loading = false;
                    match result {
                        Ok(Some(capture)) => {
                            view.tmux_details.capture = Some(capture);
                            view.tmux_details.capture_error = None;
                        }
                        Ok(None) => {
                            view.tmux_details.capture_error = Some(crate::locale::text(
                                "捕获已取消，可再次点击该 pane 的“捕获 200 行”重试。",
                                "Capture cancelled; click this pane's Capture 200 lines action to retry.",
                            ).to_string());
                        }
                        Err(error) => {
                            view.tmux_details.capture_error =
                                Some(error.public_message().to_owned());
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }
    /// 重命名复核弹窗目标；成功才关闭，失败保留输入，pane 代次变化时不写回旧提示或草稿。
    ///
    /// @author kongweiguang
    pub(super) fn rename_tmux_session(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.ensure_tmux_dialog_current(cx) {
            return;
        }
        let Some(session_id) = self.tmux_rename_target.clone() else {
            return;
        };
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let name = self.input_value(&inputs.tmux_rename, cx).trim().to_owned();
        if name.is_empty() {
            self.status = Some(
                crate::locale::text(
                    "请输入新的 tmux session 名称。",
                    "Enter a new tmux session name.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let renamed_session_id = session_id.clone();
        let target = self.tmux_target();
        let target_host = target.host_id.clone();
        let tmux_view_generation = self.tmux_view_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations
                        .tmux
                        .rename_session(target, session_id, name, cancel)
                        .await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let disposition = view.dirty_targets.tmux_mutation_completed(
                        target_host.clone(),
                        &view.host_target,
                        tmux_view_generation,
                        view.tmux_view_generation,
                    );
                    if disposition.update_current_view {
                        if result.is_ok()
                            && view.tmux_rename_target.as_deref()
                                == Some(renamed_session_id.as_str())
                        {
                            view.clear_tmux_dialog();
                            cx.emit(OperationsEvent::RootConfirmationChanged);
                        }
                        view.status = Some(match result {
                            Ok(receipt) => mutation_message(Ok(receipt)),
                            Err(error) => mutation_error_message(error, cancellation_requested),
                        });
                    }
                    if disposition.refresh_current_host {
                        view.load_tmux(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// kill 复核冻结的 server 与会话 ID；失败保留确认，回读期间禁重复提交，实际回执才可清详情。
    ///
    /// @author kongweiguang
    pub(super) fn kill_tmux_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.busy
            || !self.ensure_tmux_dialog_current(cx)
            || self.tmux_pending_kill.as_deref() != Some(session_id.as_str())
        {
            return;
        }
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let killed_session_id = session_id.clone();
        let target = self.tmux_target();
        let target_host = target.host_id.clone();
        let tmux_view_generation = self.tmux_view_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations
                        .tmux
                        .kill_session(target, session_id, cancel)
                        .await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let disposition = view.dirty_targets.tmux_mutation_completed(
                        target_host.clone(),
                        &view.host_target,
                        tmux_view_generation,
                        view.tmux_view_generation,
                    );
                    if disposition.update_current_view {
                        if result.is_ok() {
                            view.clear_tmux_dialog();
                            cx.emit(OperationsEvent::RootConfirmationChanged);
                        }
                        let killed = matches!(&result, Ok(true));
                        view.status = Some(match result {
                            Ok(killed) => kill_session_result_message(Ok(killed)),
                            Err(error) => mutation_error_message(error, cancellation_requested),
                        });
                        if killed
                            && view.tmux_details.session_id.as_deref()
                                == Some(killed_session_id.as_str())
                        {
                            view.tmux_details = TmuxDetailState::default();
                        }
                    }
                    if disposition.refresh_current_host {
                        view.load_tmux(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// detach 只分离选定 session 的客户端；同 host 迟到回执刷新列表，不改写新 pane 状态。
    ///
    /// @author kongweiguang
    pub(super) fn detach_tmux_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.busy || !self.tmux_target_available() {
            return;
        }
        self.busy = true;
        let request_cancellation = self.begin_operation_mutation();
        let cancellation_status = request_cancellation.clone();
        let target = self.tmux_target();
        let target_host = target.host_id.clone();
        let tmux_view_generation = self.tmux_view_generation;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request_cancellation, move |cancel| async move {
                    operations
                        .tmux
                        .detach_session(target, session_id, cancel)
                        .await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let disposition = view.dirty_targets.tmux_mutation_completed(
                        target_host.clone(),
                        &view.host_target,
                        tmux_view_generation,
                        view.tmux_view_generation,
                    );
                    if disposition.update_current_view {
                        view.status = Some(match result {
                            Ok(()) => crate::locale::text("客户端已分离。", "Client detached.")
                                .to_string(),
                            Err(error) => mutation_error_message(error, cancellation_requested),
                        });
                    }
                    if disposition.refresh_current_host {
                        view.load_tmux(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 点击时先选中明确的 session，使折叠卡首次 attach 也能登记后续分离意图；
    /// Root 仍按原 pane owner 和当前选择复核，异步期间改选不会被迟到回执覆盖。
    ///
    /// @author kongweiguang
    pub(super) fn request_tmux_attach(
        &mut self,
        session_id: String,
        session_name: String,
        cx: &mut Context<Self>,
    ) {
        if !self.tmux_target_available() || !self.tmux_attach_ready() {
            return;
        }
        self.select_tmux_session(session_id.clone(), session_name.clone(), cx);
        cx.emit(OperationsEvent::TmuxAttachRequested {
            target: self.tmux_target(),
            session_id,
            session_name,
            terminal: self.tmux_attach_binding.clone(),
        });
    }

    /// 保存定义后可选启动；目标取表单冻结的 HostId，未知保存保留快照并只核对原事务。
    ///
    /// @author kongweiguang
    pub(super) fn save_forward(&mut self, start_after_save: bool, cx: &mut Context<Self>) {
        if self.busy || self.forward_save_pending.is_some() {
            return;
        }
        if !self.can_save_forward() {
            self.status = Some(crate::locale::text(
                "转发表单没有固定的 SSH 主机；请取消后从明确的 SSH 主机重新打开。",
                "The forward form has no frozen SSH host. Cancel it and reopen from an explicit SSH host.",
            ).to_string());
            cx.notify();
            return;
        }
        let Some(inputs) = self.inputs.as_ref() else {
            return;
        };
        let name = self.input_value(&inputs.forward_name, cx);
        let bind_host = self.input_value(&inputs.forward_bind_host, cx);
        let bind_port = self.input_value(&inputs.forward_bind_port, cx);
        let destination_host = self.input_value(&inputs.forward_destination_host, cx);
        let destination_port = self.input_value(&inputs.forward_destination_port, cx);
        let Some(form_host_id) = self.forward_form_host_id() else {
            return;
        };
        let mut config: PortForwardConfig = match forward_config(
            Some(form_host_id),
            &name,
            self.forward_kind,
            &bind_host,
            &bind_port,
            &destination_host,
            &destination_port,
        ) {
            Ok(config) => config,
            Err(error) => {
                self.status = Some(error.message().to_owned());
                cx.notify();
                return;
            }
        };
        if let Some(expected) = self.forward_edit_expected.as_ref() {
            // 此表单不编辑代理应用策略，不能在修改名称或端口时抹掉原意图。
            config.proxy_apply_scope = expected.config.proxy_apply_scope;
        }
        let attempted = PortForwardDefinition {
            id: self
                .forward_edit_expected
                .as_ref()
                .map(|definition| definition.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            config: config.clone(),
        };
        self.busy = true;
        let cancellation = self.begin_operation_mutation();
        let cancellation_status = cancellation.clone();
        let expected = self.forward_edit_expected.clone();
        let new_id = attempted.id.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("保存转发定义已取消"));
                    }
                    let definition = match expected {
                        Some(expected) => operations
                            .port_forwards
                            .update_definition(expected, config)
                            .await?,
                        None => operations.port_forwards.create_definition_with_id(new_id, config).await?,
                    };
                    let start_result = if start_after_save {
                        Some(
                            operations
                                .port_forwards
                                .start_definition(&definition.id, cancel)
                                .await,
                        )
                    } else {
                        None
                    };
                    Ok((definition, start_result))
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    let forward_form_was_open = view
                        .inputs
                        .as_ref()
                        .is_some_and(|inputs| inputs.forward_form_open);
                    match result {
                        Ok((definition, None)) => {
                            view.forward_edit_expected = None;
                            view.forward_add_host_expected = None;
                            if let Some(inputs) = view.inputs.as_mut() {
                                inputs.forward_form_open = next_forward_form_open(
                                    inputs.forward_form_open,
                                    ForwardFormTransition::CreateSucceeded,
                                );
                            }
                            view.status = Some(crate::locale::text(
                                format!("已保存转发定义：{}", definition.config.name),
                                format!("Saved forward definition: {}", definition.config.name),
                            ).to_string());
                        }
                        Ok((definition, Some(Ok(snapshot)))) => {
                            view.forward_edit_expected = None;
                            view.forward_add_host_expected = None;
                            if let Some(inputs) = view.inputs.as_mut() {
                                inputs.forward_form_open = next_forward_form_open(
                                    inputs.forward_form_open,
                                    ForwardFormTransition::CreateSucceeded,
                                );
                            }
                            view.status = Some(crate::locale::text(
                                format!("已启动 {}：{}", definition.config.name, snapshot.actual_bind),
                                format!("Started {}: {}", definition.config.name, snapshot.actual_bind),
                            ).to_string());
                        }
                        Ok((_definition, Some(Err(error)))) => {
                            view.forward_edit_expected = None;
                            view.forward_add_host_expected = None;
                            if let Some(inputs) = view.inputs.as_mut() {
                                inputs.forward_form_open = false;
                            }
                            view.status = Some(crate::locale::text(
                                format!("定义已保存，但启动未完成：{}。请核对清单；清理失败时先点停止。", error.public_message()),
                                format!("Definition saved, but start did not complete: {}. Review the list; stop any entry whose cleanup failed.", error.public_message()),
                            ).to_string());
                        }
                        Err(error) => {
                            if error.public_code() == PublicErrorCode::OutcomeUnknown {
                                view.forward_save_pending = Some(attempted);
                            }
                            if let Some(inputs) = view.inputs.as_mut() {
                                inputs.forward_form_open = next_forward_form_open(
                                    inputs.forward_form_open,
                                    ForwardFormTransition::CreateFailed,
                                );
                            }
                            view.status = Some(if cancellation_requested {
                                mutation_error_message(error, true)
                            } else {
                                error.public_message().to_owned()
                            });
                        }
                    }
                    if forward_form_was_open
                        && view
                            .inputs
                            .as_ref()
                            .is_some_and(|inputs| !inputs.forward_form_open)
                    {
                        cx.emit(OperationsEvent::RootConfirmationChanged);
                    }
                    // 定义清单和运行快照共享同一全局 revision，迟到写回后以仓储读取为准。
                    view.load_forwards(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 保存回执未知时只回读原 ID；确认落盘不触发启动，缺失或内容冲突也不自动重放写入。
    /// @author kongweiguang
    pub(super) fn reconcile_forward_save(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(expected) = self.forward_save_pending.clone() else {
            return;
        };
        self.busy = true;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let id = expected.id.clone();
            let result = executor.run(move |_| async move {
                operations.port_forwards.get_definition(&id).await
            }).await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                let forward_form_was_open = view
                    .inputs
                    .as_ref()
                    .is_some_and(|inputs| inputs.forward_form_open);
                match result {
                    Ok(Some(actual)) if actual == expected => {
                        view.forward_save_pending = None;
                        view.forward_edit_expected = None;
                        view.forward_add_host_expected = None;
                        if let Some(inputs) = view.inputs.as_mut() {
                            inputs.forward_form_open = false;
                        }
                        view.status = Some(crate::locale::text(
                            "已核对：定义已保存。尚未自动启动，可在列表中显式启动。",
                            "Verified: the definition was saved. It was not started automatically; start it from the list.",
                        ).to_string());
                    }
                    Ok(None) => {
                        view.forward_save_pending = None;
                        view.status = Some(crate::locale::text(
                            "已核对：原 ID 未保存。草稿已保留，可明确再次保存；未自动重试。",
                            "Verified: the original ID was not saved. The draft is retained; save again explicitly if needed. No automatic retry was made.",
                        ).to_string());
                    }
                    Ok(Some(_)) => {
                        view.status = Some(crate::locale::text(
                            "原 ID 的配置与提交内容不同。请返回列表核对并重新编辑，不要重复保存。",
                            "The saved content differs from this submission. Return to the list and review the current definition before editing again.",
                        ).to_string());
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                if forward_form_was_open
                    && view
                        .inputs
                        .as_ref()
                        .is_some_and(|inputs| !inputs.forward_form_open)
                {
                    cx.emit(OperationsEvent::RootConfirmationChanged);
                }
                view.load_forwards(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    /// 人工结束核对只归档此次草稿，不把未知保存标为成功，也不覆盖当前磁盘定义。
    /// @author kongweiguang
    pub(super) fn archive_forward_save_review(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.forward_save_pending.is_none() {
            return;
        }
        self.forward_save_pending = None;
        self.forward_edit_expected = None;
        self.forward_add_host_expected = None;
        let forward_form_was_open = self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.forward_form_open);
        if let Some(inputs) = self.inputs.as_mut() {
            inputs.forward_form_open = false;
        }
        if forward_form_was_open {
            cx.emit(OperationsEvent::RootConfirmationChanged);
        }
        self.status = Some(crate::locale::text(
            "已结束本次草稿核对，未确认保存成功；现有磁盘定义保持不变。",
            "The draft review was archived without confirming a successful save; current definitions were not modified.",
        ).to_string());
        self.load_forwards(cx);
        cx.notify();
    }

    /// 使用已保存定义和原 ID 建立转发，失败结果保留为可再次显式启动的停止定义。
    pub(super) fn start_saved_forward(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let cancellation = self.begin_operation_mutation();
        let cancellation_status = cancellation.clone();
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    operations.port_forwards.start_definition(&id, cancel).await
                })
                .await;
            let cancellation_requested = cancellation_status.is_cancelled();
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_operation_mutation();
                    view.status = Some(match result {
                        Ok(snapshot) => crate::locale::text(
                            format!("转发已启动：{}", snapshot.actual_bind),
                            format!("Forward started: {}", snapshot.actual_bind),
                        )
                        .to_string(),
                        Err(error) if cancellation_requested => mutation_error_message(error, true),
                        Err(error) => error.public_message().to_owned(),
                    });
                    view.load_forwards(cx);
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 只删除经二次确认的停止定义；应用层再次核对活动 owner 后才提交持久删除。
    pub(super) fn delete_forward_definition(
        &mut self,
        expected: PortForwardDefinition,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        if self.forward_pending_delete.as_ref() != Some(&expected) {
            return;
        }
        self.busy = true;
        self.forward_pending_delete = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(move |_| async move {
                        operations.port_forwards.delete_definition(expected).await
                    })
                    .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.busy = false;
                    view.status = Some(simple_result_message(
                        result,
                        "转发定义已删除。",
                        "Forward definition deleted.",
                    ));
                    view.load_forwards(cx);
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 只停止行中指定转发 ID；其他隧道保留原有生命周期。
    pub(super) fn stop_forward(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let operations = self.operations.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { operations.port_forwards.stop(&id).await })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |view, cx| {
                    view.busy = false;
                    view.status = Some(simple_result_message(
                        result,
                        "端口转发已关闭。",
                        "Port forward stopped.",
                    ));
                    view.load_forwards(cx);
                });
            });
        })
        .detach();
        cx.notify();
    }
}

/// 创建按钮和动作共用同一 fail-closed 决策；未探测或探测为不可用时只能先显式重读。
///
/// @author kongweiguang
pub(super) fn tmux_session_creation_enabled(
    busy: bool,
    target_available: bool,
    capability: Option<&TmuxCapability>,
) -> bool {
    !busy && target_available && capability.is_some_and(|capability| capability.available)
}

/// 通过共享应用任务执行器读取单个层级，保持该面板自己的取消票据。
async fn run_server_tier_request(
    request: ServerMonitorTierRequest,
) -> kerminal_application::AppResult<Option<ServerInfoSnapshot>> {
    let ServerMonitorTierRequest {
        ticket,
        target,
        tier,
        operations,
        executor,
    } = request;
    run_section_request(executor, ticket.cancellation, move |cancel| async move {
        operations
            .server_info
            .snapshot(ServerInfoQuery { target, tier }, cancel)
            .await
    })
    .await
}

/// 以显式边沿版本保留重显刷新请求；直接接收采样时间，令“未采样”只能由 `None` 表达，避免调用方反转布尔语义。
fn server_sample_is_immediate(
    configuration: &super::ServerMonitorConfiguration,
    observed_generation: Option<u64>,
    observed_immediate_revision: Option<u64>,
    observed_panel_active: bool,
    observed_section_visible: bool,
    last_sampled: Option<Instant>,
) -> bool {
    configuration.server_section_visible
        && configuration.panel_active
        && (observed_immediate_revision != Some(configuration.immediate_revision)
            || observed_generation != Some(configuration.server_generation)
            || !observed_panel_active
            || !observed_section_visible
            || last_sampled.is_none())
}

/// 缺失或未来时间戳按过期处理，防止时钟差异让某一层长期停止刷新。
fn sampled_tier_expired(sampled_at_unix_ms: Option<u64>, now_unix_ms: u64, ttl: Duration) -> bool {
    let Some(sampled_at_unix_ms) = sampled_at_unix_ms else {
        return true;
    };
    if sampled_at_unix_ms > now_unix_ms {
        return true;
    }
    now_unix_ms.saturating_sub(sampled_at_unix_ms) >= ttl.as_millis().min(u64::MAX as u128) as u64
}

/// 获取本机当前 Unix 毫秒，仅用于和快照中的真实采样时间比较。
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// 合并部分层级时只覆盖本轮确实采样的字段；Fast 的未知 uptime 清除旧值以免显示陈旧时长。
///
/// @author kongweiguang
fn merge_server_info_snapshot(
    previous: Option<ServerInfoSnapshot>,
    incoming: ServerInfoSnapshot,
) -> ServerInfoSnapshot {
    let Some(mut merged) = previous.filter(|previous| previous.target == incoming.target) else {
        return incoming;
    };
    let times = incoming.sample_times.clone();
    if times.fast_at_unix_ms.is_some() {
        merged.sample_times.fast_at_unix_ms = times.fast_at_unix_ms;
        merged.uptime_seconds = incoming.uptime_seconds;
        merged.cpu.usage_percent = incoming.cpu.usage_percent;
        merged.cpu.per_core_percent = incoming.cpu.per_core_percent;
        merged.cpu.load_average = incoming.cpu.load_average;
        merged.memory = incoming.memory;
        merged.network_counters = incoming.network_counters;
        merged.network_rates = incoming.network_rates;
        merged.network_probe_status = incoming.network_probe_status;
    }
    if times.slow_at_unix_ms.is_some() {
        merged.sample_times.slow_at_unix_ms = times.slow_at_unix_ms;
        merged.disks = incoming.disks;
        merged.process_count = incoming.process_count;
        merged.running_process_count = incoming.running_process_count;
        merged.processes = incoming.processes;
        merged.disk_probe_status = incoming.disk_probe_status;
        merged.process_probe_status = incoming.process_probe_status;
    }
    if times.static_info_at_unix_ms.is_some() {
        merged.sample_times.static_info_at_unix_ms = times.static_info_at_unix_ms;
        merged.hostname = incoming.hostname;
        merged.operating_system = incoming.operating_system;
        merged.architecture = incoming.architecture;
        merged.kernel = incoming.kernel;
        merged.cpu.model = incoming.cpu.model;
    }
    if times.gpu_at_unix_ms.is_some() {
        merged.sample_times.gpu_at_unix_ms = times.gpu_at_unix_ms;
        merged.gpus = incoming.gpus;
        merged.gpu_probe_status = incoming.gpu_probe_status;
    }
    if times.npu_at_unix_ms.is_some() {
        merged.sample_times.npu_at_unix_ms = times.npu_at_unix_ms;
        merged.npus = incoming.npus;
        merged.npu_probe_status = incoming.npu_probe_status;
    }
    merged.captured_at_unix_ms = incoming.captured_at_unix_ms;
    merged
}

/// 保留应用层区分的未知结果；取消确认不代表远端未执行，因此提示回读原目标。
///
/// @author kongweiguang
fn mutation_error_message(error: AppError, cancellation_requested: bool) -> String {
    match error.public_code() {
        PublicErrorCode::OutcomeUnknown => error.public_message().to_owned(),
        PublicErrorCode::Cancelled if cancellation_requested => crate::locale::text(
            "用例已返回取消结果；远端操作可能已生效，正在重新读取原目标。",
            "The use case returned a cancellation result; the remote action may have taken effect. Refreshing the original target.",
        )
        .to_string(),
        _ => error.public_message().to_owned(),
    }
}

/// application 错误统一通过脱敏 public_message 呈现，成功时保留刷新是否待重试的信息。
fn mutation_message<T>(result: Result<T, AppError>) -> String
where
    T: HasRefreshRequired,
{
    match result {
        Ok(receipt) if receipt.refresh_required() => crate::locale::text(
            "操作已完成；读取状态失败，可单独重试刷新。",
            "The action completed; refresh failed and can be retried separately.",
        )
        .to_string(),
        Ok(_) => crate::locale::text("操作已完成。", "Action completed.").to_string(),
        Err(error) => error.public_message().to_owned(),
    }
}

/// 只为带 refresh_required 回执的 tmux 创建/重命名提供轻量格式化约束。
trait HasRefreshRequired {
    /// 返回 mutation 回执是否要求独立重试状态刷新。
    fn refresh_required(&self) -> bool;
}

impl HasRefreshRequired for TmuxMutationReceipt {
    /// 读取 tmux mutation receipt 标志，避免将成功 mutation 的刷新失败混为写入失败。
    fn refresh_required(&self) -> bool {
        self.refresh_required
    }
}

/// 无回执的端口/会话动作只报告应用用例实际返回的结果。
fn simple_result_message<T>(result: Result<T, AppError>, zh: &str, en: &str) -> String {
    result.map_or_else(
        |error| error.public_message().to_owned(),
        |_| crate::locale::text(zh, en).to_string(),
    )
}

/// kill 返回 false 表示目标已不存在，不能把“未执行删除”包装成成功回执。
fn kill_session_result_message(result: Result<bool, AppError>) -> String {
    match result {
        Ok(true) => crate::locale::text("tmux session 已结束。", "tmux session ended.").to_string(),
        Ok(false) => crate::locale::text(
            "tmux session 已不存在，未执行结束操作。",
            "The tmux session no longer exists; no kill was performed.",
        )
        .to_string(),
        Err(error) => error.public_message().to_owned(),
    }
}

/// 只从显式 Compose host 元数据构造 Local/Remote 文件目标，拒绝含糊相对路径和无文件名路径。
fn compose_config_endpoint(
    host_id: Option<kerminal_domain::identity::HostId>,
    source_path: &str,
    working_directory: Option<&str>,
) -> Option<(FileEndpoint, String)> {
    if source_path.trim().is_empty() || source_path.chars().any(char::is_control) {
        return None;
    }
    match host_id {
        None => {
            let source = PathBuf::from(source_path);
            let path = if source.is_absolute() {
                source
            } else {
                let working_directory = PathBuf::from(working_directory?);
                if !working_directory.is_absolute() {
                    return None;
                }
                working_directory.join(source)
            };
            let selection_name = path.file_name()?.to_str()?.to_owned();
            Some((FileEndpoint::Local { path }, selection_name))
        }
        Some(host_id) => {
            let absolute_path = if source_path.starts_with('/') {
                source_path.to_owned()
            } else {
                let working_directory = working_directory?;
                if !working_directory.starts_with('/') {
                    return None;
                }
                format!("{}/{source_path}", working_directory.trim_end_matches('/'))
            };
            let path = RemotePath::parse(&absolute_path).ok()?;
            if !path.as_str().starts_with('/') {
                return None;
            }
            let selection_name = path.as_str().rsplit('/').next()?.to_owned();
            (!selection_name.is_empty())
                .then_some((FileEndpoint::Remote { host_id, path }, selection_name))
        }
    }
}

/// 输出区标注真实 runtime、host ID、本地/远端目标及容器名和完整 ID。
fn container_output_target_label(target: &ContainerTarget, container_name: Option<&str>) -> String {
    let host = target.host_id.as_ref().map_or_else(
        || crate::locale::text("本机 CLI", "local CLI").to_string(),
        |id| format!("host {id}"),
    );
    let name = container_name.unwrap_or("—");
    format!(
        "{} · {host} · {name} ({})",
        match target.runtime {
            ContainerRuntime::Docker => "Docker",
            ContainerRuntime::Podman => "Podman",
        },
        target.container_id,
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_actions.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/operations_monitor.rs"]
mod monitor_tests;
