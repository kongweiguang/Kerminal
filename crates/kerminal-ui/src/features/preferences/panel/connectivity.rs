//! 外部启动、MCP 与同步密钥页面共用的运行态和局部表单状态。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc, task::Poll, time::Duration};

use gpui_kit::{
    AppContext as _, Context, Entity, Subscription, Window,
    component::input::{InputEvent, TextareaState},
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor, PublicErrorCode,
    configuration_sync::{
        ConfigurationSyncRunResult, ConfigurationSyncService, ConfigurationWorkspaceInfo,
    },
    connections::credentials::{VaultKeyAvailability, VaultKeyDocument, VaultKeyManagementService},
    mcp_runtime::{Instant, McpRuntimeControl, McpRuntimeSnapshot, McpRuntimeState},
    platform::{
        DesktopNotificationEvent, DesktopNotificationService, NotificationImportance,
        WindowVisibility,
    },
};
use tokio_util::sync::CancellationToken;

use super::PreferencesPanel;

const MCP_COPY_FEEDBACK_WINDOW: Duration = Duration::from_millis(1_600);
const KEY_DOCUMENT_MIN_HEIGHT: f32 = 256.0;

/// 只在页面需要时保存 MCP 快照与密钥编辑器，避免把秘密复制进设置快照或日志。
pub(in crate::features::preferences) struct ConnectivityState {
    pub(in crate::features::preferences) key_document: Entity<TextareaState>,
    pub(in crate::features::preferences) key_document_height: f32,
    key_document_resize_origin: Option<(f32, f32)>,
    _key_subscription: Subscription,
    pub(in crate::features::preferences) key_loading: bool,
    pub(in crate::features::preferences) key_saving: bool,
    pub(in crate::features::preferences) key_loaded: bool,
    key_outcome_unknown: bool,
    key_refresh_pending: bool,
    pub(in crate::features::preferences) key_error: Option<String>,
    pub(in crate::features::preferences) key_status: Option<String>,
    pub(in crate::features::preferences) workspace_info: Option<ConfigurationWorkspaceInfo>,
    pub(in crate::features::preferences) sync_running: bool,
    pub(in crate::features::preferences) sync_run_result: Option<ConfigurationSyncRunResult>,
    key_generation: u64,
    key_edit_generation: u64,
    key_edited: bool,
    runtime: Option<Arc<dyn McpRuntimeControl>>,
    executor: ApplicationTaskExecutor,
    notifications: Option<Arc<DesktopNotificationService>>,
    pub(in crate::features::preferences) mcp_snapshot: McpRuntimeSnapshot,
    pub(in crate::features::preferences) mcp_operation: Option<McpOperation>,
    pub(in crate::features::preferences) mcp_status: Option<String>,
    pub(in crate::features::preferences) mcp_status_is_error: bool,
    pub(in crate::features::preferences) mcp_copied: bool,
    pub(in crate::features::preferences) mcp_copy_error: Option<String>,
    mcp_generation: u64,
    mcp_copy_generation: u64,
    pub(in crate::features::preferences) external_compatibility_expanded: bool,
    pub(in crate::features::preferences) mcp_connection_expanded: bool,
}

/// MCP 操作阶段决定正在显示的动作状态，不将 pending endpoint 误标为已运行。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::features::preferences) enum McpOperation {
    Start,
    Stop,
}

impl ConnectivityState {
    /// 在窗口创建期建立多行输入与订阅，保证密钥编辑器焦点和 IME 状态稳定。
    pub(super) fn new(
        runtime: Option<Arc<dyn McpRuntimeControl>>,
        executor: ApplicationTaskExecutor,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) -> Self {
        let key_document =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("schema_version = 1"));
        let _key_subscription = cx.subscribe(&key_document, |panel, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                panel.connectivity.key_edit_generation = panel
                    .connectivity
                    .key_edit_generation
                    .wrapping_add(1)
                    .max(1);
                panel.connectivity.key_edited = true;
                panel.connectivity.key_error = None;
                panel.connectivity.key_status = None;
                cx.notify();
            }
        });
        let mcp_snapshot = runtime.as_ref().map_or_else(
            || McpRuntimeSnapshot::unavailable("MCP runtime 未由应用装配"),
            |runtime| runtime.snapshot(),
        );

        Self {
            key_document,
            key_document_height: KEY_DOCUMENT_MIN_HEIGHT,
            key_document_resize_origin: None,
            _key_subscription,
            key_loading: false,
            key_saving: false,
            key_loaded: false,
            key_outcome_unknown: false,
            key_refresh_pending: false,
            key_error: None,
            key_status: None,
            workspace_info: None,
            sync_running: false,
            sync_run_result: None,
            key_generation: 0,
            key_edit_generation: 0,
            key_edited: false,
            runtime,
            executor,
            notifications: None,
            mcp_snapshot,
            mcp_operation: None,
            mcp_status: None,
            mcp_status_is_error: false,
            mcp_copied: false,
            mcp_copy_error: None,
            mcp_generation: 0,
            mcp_copy_generation: 0,
            external_compatibility_expanded: false,
            mcp_connection_expanded: false,
        }
    }

    /// 拖动始终基于起点高度计算，避免每帧取整造成累计漂移。
    pub(in crate::features::preferences) fn begin_key_document_resize(&mut self, pointer_y: f32) {
        self.key_document_resize_origin = Some((pointer_y, self.key_document_height));
    }

    /// 手动尺寸只作用于当前视图，保留 main 的最小高度并让内容继续由外层滚动。
    pub(in crate::features::preferences) fn resize_key_document(&mut self, pointer_y: f32) -> bool {
        let Some((origin_y, origin_height)) = self.key_document_resize_origin else {
            return false;
        };
        if !pointer_y.is_finite() {
            return false;
        }
        let next_height = (origin_height + pointer_y - origin_y).max(KEY_DOCUMENT_MIN_HEIGHT);
        if (next_height - self.key_document_height).abs() < f32::EPSILON {
            return false;
        }
        self.key_document_height = next_height;
        true
    }

    /// 鼠标释放后丢弃瞬时拖动起点，避免之后的指针移动改变编辑器尺寸。
    pub(in crate::features::preferences) fn finish_key_document_resize(&mut self) -> bool {
        self.key_document_resize_origin.take().is_some()
    }

    /// 重开时刷新 Vault 文档；脏编辑、未知写入和在途变更优先于外部回读。
    ///
    /// 首次读取失败后仍允许后续重开重试，晚到的读取结果也不能覆盖开始后的编辑。
    ///
    /// @author kongweiguang
    pub(super) fn load_documents(
        &mut self,
        vault_keys: Option<VaultKeyManagementService>,
        sync: Option<ConfigurationSyncService>,
        refresh: bool,
        workspace_outcome_unknown: bool,
        cx: &mut Context<PreferencesPanel>,
    ) -> bool {
        if workspace_outcome_unknown {
            self.key_refresh_pending |= refresh;
            return false;
        }
        if self.key_loading || self.key_saving || self.sync_running {
            self.key_refresh_pending |= refresh;
            return false;
        }
        if self.key_edited || self.key_outcome_unknown {
            self.key_refresh_pending = false;
            return false;
        }
        if self.key_loaded && !refresh {
            return false;
        }
        self.key_refresh_pending = false;
        let (Some(vault_keys), Some(sync)) = (vault_keys, sync) else {
            self.key_loaded = true;
            self.key_error = Some(
                crate::locale::text(
                    "同步密钥服务不可用。",
                    "The workspace key service is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        };
        self.key_generation = self.key_generation.wrapping_add(1).max(1);
        let generation = self.key_generation;
        let edit_generation = self.key_edit_generation;
        let executor = self.executor.clone();
        self.key_loading = true;
        self.key_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let (availability, key_result) = read_vault_key_document(vault_keys).await;
                    let workspace_result = sync.workspace_info().await;
                    Ok::<_, AppError>((availability, key_result, workspace_result))
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                let workspace_outcome_unknown = panel.sync_outcome_unknown;
                let should_retry = {
                    let state = &mut panel.connectivity;
                    if generation != state.key_generation {
                        return;
                    }
                    state.key_loading = false;
                    state.key_loaded = true;
                    match result {
                        Ok((availability, key_result, workspace_result)) => {
                            panel.vault_availability = availability.clone();
                            panel.vault_error = None;
                            if availability.as_ref().is_some_and(|availability| {
                                matches!(
                                    availability,
                                    VaultKeyAvailability::Ready { .. }
                                        | VaultKeyAvailability::Created { .. }
                                )
                            }) {
                                panel.vault_outcome_unknown = false;
                                panel.vault_status = None;
                            }
                            if availability.is_none()
                                && let Err(error) = &key_result
                            {
                                panel.vault_outcome_unknown =
                                    error.public_code() == PublicErrorCode::OutcomeUnknown;
                                panel.vault_error = Some(super::localized_error(error));
                            }
                            if edit_generation == state.key_edit_generation {
                                match key_result {
                                    Ok(Some(document)) => {
                                        let source = document.as_str().to_owned();
                                        super::set_textarea(
                                            &state.key_document,
                                            source,
                                            window,
                                            cx,
                                        );
                                        state.key_edited = false;
                                        state.key_error = None;
                                    }
                                    Ok(None) => {
                                        super::set_textarea(
                                            &state.key_document,
                                            String::new(),
                                            window,
                                            cx,
                                        );
                                        state.key_edited = false;
                                        state.key_error = None;
                                    }
                                    Err(error) => {
                                        state.key_error = Some(super::localized_error(&error));
                                    }
                                }
                            } else {
                                state.key_edited = true;
                                if let Err(error) = key_result {
                                    state.key_error = Some(super::localized_error(&error));
                                }
                            }
                            match workspace_result {
                                Ok(info) => state.workspace_info = info,
                                Err(error) if state.key_error.is_none() => {
                                    state.key_error = Some(super::localized_error(&error));
                                }
                                Err(_) => {}
                            }
                        }
                        Err(error) => {
                            let message = super::localized_error(&error);
                            state.key_error = Some(message.clone());
                            panel.vault_availability = None;
                            panel.vault_error = Some(message);
                            panel.vault_outcome_unknown =
                                error.public_code() == PublicErrorCode::OutcomeUnknown;
                        }
                    }
                    state.key_refresh_pending
                        && !state.key_edited
                        && !state.key_outcome_unknown
                        && !workspace_outcome_unknown
                };
                if should_retry {
                    panel.connectivity.load_documents(
                        panel.vault_keys.clone(),
                        panel.configuration_sync.clone(),
                        true,
                        workspace_outcome_unknown,
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    /// 用实际 application 快照刷新运行状态，避免设置页保留过期 endpoint。
    pub(super) fn refresh_mcp(&mut self, cx: &mut Context<PreferencesPanel>) {
        if self.mcp_operation.is_some() {
            return;
        }
        self.mcp_snapshot = self.runtime.as_ref().map_or_else(
            || McpRuntimeSnapshot::unavailable("MCP runtime 未由应用装配"),
            |runtime| runtime.snapshot(),
        );
        self.mcp_status = self
            .mcp_snapshot
            .failure_reason
            .clone()
            .or_else(|| self.mcp_snapshot.unavailable_reason.clone());
        self.mcp_status_is_error = self.mcp_snapshot.failure_reason.is_some()
            || self.mcp_snapshot.unavailable_reason.is_some();
        self.mcp_copy_generation = self.mcp_copy_generation.wrapping_add(1).max(1);
        self.mcp_copied = false;
        self.mcp_copy_error = None;
        cx.notify();
    }

    /// 延后的一次设置打开回读只在保护状态解除后执行，不把脏草稿或未知写入当成新值。
    ///
    /// @author kongweiguang
    pub(super) fn resume_pending_documents(
        &mut self,
        vault_keys: Option<VaultKeyManagementService>,
        sync: Option<ConfigurationSyncService>,
        workspace_outcome_unknown: bool,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if self.key_refresh_pending {
            self.load_documents(vault_keys, sync, true, workspace_outcome_unknown, cx);
        }
    }

    /// 点击时重读共享状态；停止超时只重试原 owner 的收尾，不能误路由到被门禁拒绝的启动。
    pub(in crate::features::preferences) fn toggle_mcp(
        &mut self,
        cx: &mut Context<PreferencesPanel>,
    ) {
        match self.current_mcp_snapshot().state {
            McpRuntimeState::Running | McpRuntimeState::Stopping => self.stop_mcp(cx),
            McpRuntimeState::Stopped | McpRuntimeState::Synchronizing | McpRuntimeState::Failed => {
                self.start_mcp(cx);
            }
            McpRuntimeState::Unavailable | McpRuntimeState::Starting => {}
        }
    }

    /// 启动共享 MCP listener，按 runtime 实际状态门禁，旧操作结果不能覆盖新状态。
    pub(in crate::features::preferences) fn start_mcp(
        &mut self,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if let Some(runtime) = self.runtime.as_ref() {
            self.mcp_snapshot = runtime.snapshot();
        }
        if self.mcp_operation.is_some()
            || !matches!(
                self.mcp_snapshot.state,
                McpRuntimeState::Stopped | McpRuntimeState::Synchronizing | McpRuntimeState::Failed
            )
        {
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.mcp_status = Some("MCP runtime 未由应用装配".to_owned());
            cx.notify();
            return;
        };
        self.begin_mcp_operation(McpOperation::Start, cx);
        let executor = self.executor.clone();
        let generation = self.mcp_generation;
        cx.spawn(async move |this, cx| {
            let user_cancellation = CancellationToken::new();
            let result = executor
                .run(move |task_cancellation| {
                    let operation_cancellation = CancellationToken::new();
                    let operation_cancellation_for_start = operation_cancellation.clone();
                    let operation =
                        async move { runtime.start(operation_cancellation_for_start).await };
                    await_operation_with_cooperative_cancellation(
                        operation,
                        operation_cancellation,
                        user_cancellation,
                        task_cancellation,
                    )
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel
                    .connectivity
                    .finish_mcp_operation(generation, result, cx);
            });
        })
        .detach();
    }

    /// 停止共享 listener 时保留原 owner 的有界清理语义，超时后允许再次停止。
    pub(in crate::features::preferences) fn stop_mcp(
        &mut self,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if let Some(runtime) = self.runtime.as_ref() {
            self.mcp_snapshot = runtime.snapshot();
        }
        if self.mcp_operation.is_some()
            || (self.mcp_snapshot.endpoint.is_none() && !self.mcp_snapshot.endpoint_sync_pending)
        {
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.mcp_status = Some("MCP runtime 未由应用装配".to_owned());
            cx.notify();
            return;
        };
        self.begin_mcp_operation(McpOperation::Stop, cx);
        let executor = self.executor.clone();
        let generation = self.mcp_generation;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    runtime
                        .stop_until(Instant::now() + Duration::from_secs(5))
                        .await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel
                    .connectivity
                    .finish_mcp_operation(generation, result, cx);
            });
        })
        .detach();
    }

    /// 启动与停止共用代次状态，只允许一个 listener 操作在途并清除已失效的复制成功提示。
    fn begin_mcp_operation(&mut self, operation: McpOperation, cx: &mut Context<PreferencesPanel>) {
        self.mcp_generation = self.mcp_generation.wrapping_add(1).max(1);
        self.mcp_operation = Some(operation);
        self.mcp_status = None;
        self.mcp_status_is_error = false;
        self.mcp_copy_generation = self.mcp_copy_generation.wrapping_add(1).max(1);
        self.mcp_copied = false;
        self.mcp_copy_error = None;
        cx.notify();
    }

    /// 仅接受当前操作回执；停止未完成给出本地化恢复动作，底层原因仍由 runtime 快照保留。
    fn finish_mcp_operation(
        &mut self,
        generation: u64,
        result: Result<McpRuntimeSnapshot, AppError>,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if generation != self.mcp_generation {
            return;
        }
        let operation = self.mcp_operation.take();
        let mut start_failure_reason = None;
        let mut status_is_error = false;
        self.mcp_snapshot = self.runtime.as_ref().map_or_else(
            || McpRuntimeSnapshot::unavailable("MCP runtime 未由应用装配"),
            |runtime| runtime.snapshot(),
        );
        self.mcp_status = match result {
            Ok(snapshot) => {
                self.mcp_snapshot = snapshot;
                match operation {
                    Some(McpOperation::Start)
                        if self.mcp_snapshot.state == McpRuntimeState::Failed =>
                    {
                        status_is_error = true;
                        let reason =
                            self.mcp_snapshot.failure_reason.clone().unwrap_or_else(|| {
                                crate::locale::text(
                                    "MCP 服务启动失败。",
                                    "MCP server failed to start.",
                                )
                                .to_string()
                            });
                        start_failure_reason = Some(reason.clone());
                        Some(reason)
                    }
                    Some(McpOperation::Start) => Some(
                        crate::locale::text("MCP 服务已启动", "MCP server started").to_string(),
                    ),
                    Some(McpOperation::Stop) => Some(
                        crate::locale::text("MCP 服务已停止", "MCP server stopped").to_string(),
                    ),
                    None => None,
                }
            }
            Err(error) => {
                if operation == Some(McpOperation::Start)
                    && error.public_code() != PublicErrorCode::Cancelled
                {
                    start_failure_reason = Some(error.public_message().to_owned());
                }
                status_is_error = error.public_code() != PublicErrorCode::Cancelled;
                Some(
                    if operation == Some(McpOperation::Stop)
                        && self.mcp_snapshot.state == McpRuntimeState::Stopping
                    {
                        crate::locale::text(
                            "停止未完成，请点击“重试停止”继续清理。",
                            "Stopping did not finish. Choose Retry stop to finish cleanup.",
                        )
                        .to_string()
                    } else {
                        super::localized_error(&error)
                    },
                )
            }
        }
        .or_else(|| self.mcp_snapshot.failure_reason.clone())
        .or_else(|| self.mcp_snapshot.unavailable_reason.clone());
        self.mcp_status_is_error = status_is_error
            || self.mcp_snapshot.failure_reason.is_some()
            || self.mcp_snapshot.unavailable_reason.is_some();
        if let (Some(notifications), Some(reason)) =
            (self.notifications.clone(), start_failure_reason)
        {
            cx.spawn(async move |_, _| {
                let visibility = notifications
                    .window_visibility()
                    .unwrap_or(WindowVisibility::Foreground);
                if let Err(error) = notifications
                    .notify(
                        DesktopNotificationEvent::McpServerFailed {
                            port: None,
                            reason: Some(reason),
                            importance: Some(NotificationImportance::High),
                        },
                        visibility,
                    )
                    .await
                {
                    tracing::debug!(
                        code = error.public_code().as_str(),
                        "MCP start failure notification could not be submitted"
                    );
                }
            })
            .detach();
        }
        cx.notify();
    }

    /// 在途状态禁止复制旧配置；只有剪贴板回读匹配才确认，避免报告未完成的系统写入。
    pub(in crate::features::preferences) fn copy_mcp_json(
        &mut self,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if self.mcp_operation.is_some() {
            return;
        }
        if let Some(runtime) = self.runtime.as_ref() {
            self.mcp_snapshot = runtime.snapshot();
        }
        let Some(json) = self
            .mcp_snapshot
            .client_json
            .as_deref()
            .map(pretty_mcp_json)
        else {
            self.mcp_copy_generation = self.mcp_copy_generation.wrapping_add(1).max(1);
            self.mcp_copied = false;
            self.mcp_copy_error = None;
            self.mcp_status_is_error = false;
            self.mcp_status = Some(
                crate::locale::text(
                    "服务启动并完成 endpoint 同步后才能复制 JSON",
                    "Start the server and finish endpoint synchronization before copying JSON",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.mcp_copy_generation = self.mcp_copy_generation.wrapping_add(1).max(1);
        self.mcp_copied = false;
        self.mcp_copy_error = None;
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(json.clone()));
        let copied_text = cx
            .read_from_clipboard()
            .and_then(|clipboard| clipboard.text());
        if copied_text.as_deref() != Some(json.as_str()) {
            self.mcp_copy_error = Some(
                crate::locale::text(
                    "复制 JSON 失败，请重试。",
                    "Could not copy JSON. Try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.mcp_status = None;
        self.mcp_status_is_error = false;
        self.mcp_copy_error = None;
        let generation = self.mcp_copy_generation;
        self.mcp_copied = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(MCP_COPY_FEEDBACK_WINDOW)
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.connectivity.mcp_copy_generation == generation {
                    panel.connectivity.mcp_copied = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// 页面从共享 controller 重新读到当前 listener 回执，不复用旧 render 快照。
    pub(in crate::features::preferences) fn current_mcp_snapshot(&self) -> McpRuntimeSnapshot {
        if self.mcp_operation.is_none() {
            self.runtime.as_ref().map_or_else(
                || McpRuntimeSnapshot::unavailable("MCP runtime 未由应用装配"),
                |runtime| runtime.snapshot(),
            )
        } else {
            self.mcp_snapshot.clone()
        }
    }

    /// 让页面预览与复制共用同一缩进 JSON，避免两种呈现方式产生不同配置文本。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn pretty_client_json(
        &self,
        snapshot: &McpRuntimeSnapshot,
    ) -> Option<String> {
        snapshot.client_json.as_deref().map(pretty_mcp_json)
    }

    /// 只需暴露启动/停止方向用于对应控件的进度标签。
    pub(in crate::features::preferences) fn mcp_starting(&self) -> bool {
        self.mcp_operation == Some(McpOperation::Start)
    }

    /// 保存密钥时使用零化文档并在未知回执后回读比对；同步占用时拒绝并发文件操作。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn save_key(
        &mut self,
        service: Option<VaultKeyManagementService>,
        workspace_sync_loading: bool,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if self.key_loading || self.key_saving || self.sync_running || workspace_sync_loading {
            return;
        }
        let Some(service) = service else {
            self.key_error = Some("Vault 密钥服务不可用".to_owned());
            cx.notify();
            return;
        };
        let source = self.key_document.read(cx).value().to_string();
        if source.trim().is_empty() {
            self.key_error = Some(
                crate::locale::text("密钥内容不能为空。", "The key document cannot be empty.")
                    .to_string(),
            );
            cx.notify();
            return;
        }

        let document = VaultKeyDocument::new(source.clone());
        let expected = VaultKeyDocument::new(source);
        let edit_generation = self.key_edit_generation;
        let executor = self.executor.clone();
        self.key_generation = self.key_generation.wrapping_add(1).max(1);
        let generation = self.key_generation;
        self.key_saving = true;
        self.key_error = None;
        self.key_status = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    match service.save(document).await {
                        Ok(receipt) => Ok(KeySaveReceipt::Saved(receipt.backup_created)),
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            match service.read().await {
                                Ok(current) if current.as_str() == expected.as_str() => {
                                    Ok(KeySaveReceipt::Confirmed)
                                }
                                _ => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    }
                })
                .await;
            let refresh_workspace_status = result.is_ok();
            let _ = this.update(cx, |panel, cx| {
                let workspace_outcome_unknown = panel.sync_outcome_unknown;
                let should_retry = {
                    let state = &mut panel.connectivity;
                    if state.key_generation != generation {
                        return;
                    }
                    state.key_saving = false;
                    match result {
                        Ok(KeySaveReceipt::Saved(backup)) => {
                            let unchanged = state.key_edit_generation == edit_generation;
                            state.key_edited = !unchanged;
                            state.key_outcome_unknown = false;
                            state.key_status = Some(if !unchanged {
                                crate::locale::text(
                                    "已保存提交的内容；当前编辑仍未保存",
                                    "The submitted content was saved; newer edits are still unsaved",
                                ).to_string()
                            } else if backup {
                                crate::locale::text("密钥已保存，并已创建旧文件备份", "Key saved with a backup of the previous file").to_string()
                            } else {
                                crate::locale::text("密钥已保存", "Key saved").to_string()
                            });
                            state.key_error = None;
                        }
                        Ok(KeySaveReceipt::Confirmed) => {
                            let unchanged = state.key_edit_generation == edit_generation;
                            state.key_edited = !unchanged;
                            state.key_outcome_unknown = false;
                            state.key_status = Some(if unchanged {
                                crate::locale::text(
                                    "密钥回读与当前编辑内容一致，保存结果已确认",
                                    "Readback matches the edited key; the save is confirmed",
                                ).to_string()
                            } else {
                                crate::locale::text(
                                    "已确认提交的内容；当前编辑仍未保存",
                                    "The submitted content was confirmed; newer edits are still unsaved",
                                ).to_string()
                            });
                        }
                        Err(error) => {
                            state.key_outcome_unknown |=
                                error.public_code() == PublicErrorCode::OutcomeUnknown;
                            state.key_error = Some(super::localized_error(&error));
                        }
                    }
                    if state.key_edited || state.key_outcome_unknown {
                        state.key_refresh_pending = false;
                    }
                    state.key_refresh_pending
                        && !state.key_edited
                        && !state.key_outcome_unknown
                        && !workspace_outcome_unknown
                };
                if should_retry {
                    panel.connectivity.load_documents(
                        panel.vault_keys.clone(),
                        panel.configuration_sync.clone(),
                        true,
                        workspace_outcome_unknown,
                        cx,
                    );
                }
                if refresh_workspace_status {
                    panel.refresh_configuration_sync(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl PreferencesPanel {
    /// Root 注入同一桌面通知策略，避免设置页复制通知开关、可见性和节流规则。
    pub(crate) fn attach_notifications(
        &mut self,
        notifications: Option<Arc<DesktopNotificationService>>,
    ) {
        self.connectivity.notifications = notifications;
    }
}

enum KeySaveReceipt {
    Saved(bool),
    Confirmed,
}

/// 空白工作区沿用 application 的受锁 bootstrap；其他 readiness 状态只读取并保留原错误。
///
/// `ensure` 会拒绝在已有 vault 密文或损坏文件时创建替代 key，因此这里只对明确的
/// `NotInitialized` 状态调用它，避免设置页打开时掩盖恢复问题。
///
/// @author kongweiguang
async fn read_vault_key_document(
    service: VaultKeyManagementService,
) -> (
    Option<VaultKeyAvailability>,
    Result<Option<VaultKeyDocument>, AppError>,
) {
    let availability = match service.availability().await {
        Ok(availability) => availability,
        Err(error) => return (None, Err(error)),
    };

    let availability = if matches!(&availability, VaultKeyAvailability::NotInitialized) {
        match service.ensure().await {
            Ok(availability) => availability,
            Err(error) => return (None, Err(error)),
        }
    } else {
        availability
    };

    let document = if matches!(&availability, VaultKeyAvailability::NotInitialized) {
        Ok(None)
    } else {
        service.read().await.map(Some)
    };

    (Some(availability), document)
}

/// 对 runtime 生成的 JSON 做格式化；无法解析时保留原串，避免隐藏运行态返回值。
///
/// @author kongweiguang
fn pretty_mcp_json(source: &str) -> String {
    serde_json::from_str::<serde_json::Value>(source)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| source.to_owned())
}

/// 取消时通知 runtime 进入安全收尾，并等待它返回资源归属的最终回执。
async fn await_operation_with_cooperative_cancellation<T, Fut>(
    operation: Fut,
    operation_cancellation: CancellationToken,
    user_cancellation: CancellationToken,
    task_cancellation: CancellationToken,
) -> T
where
    T: Send,
    Fut: Future<Output = T> + Send,
{
    let mut operation = Box::pin(operation);
    let mut user_cancelled = Box::pin(user_cancellation.cancelled());
    let mut task_cancelled = Box::pin(task_cancellation.cancelled());
    let mut cancellation_sent = false;

    std::future::poll_fn(|context| {
        if let Poll::Ready(result) = operation.as_mut().poll(context) {
            return Poll::Ready(result);
        }
        if !cancellation_sent
            && (user_cancelled.as_mut().poll(context).is_ready()
                || task_cancelled.as_mut().poll(context).is_ready())
        {
            cancellation_sent = true;
            operation_cancellation.cancel();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_sync_bootstrap.rs"]
mod bootstrap_tests;
