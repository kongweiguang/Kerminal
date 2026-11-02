//! MCP 监听草稿在启动时提交，不将访问令牌保存在页面状态。
//!
//! @author kongweiguang

use std::net::Ipv4Addr;

use gpui_kit::{
    AppContext as _, Context, Entity, Subscription, Window,
    component::input::{InputEvent, InputState},
};
use kerminal_application::{
    mcp_runtime::McpRuntimeState,
    preferences::settings::{McpBindMode, McpSettings},
};

use super::{PreferencesEvent, PreferencesPanel};

/// 连接配置展示目标；默认局域网，使运行中的局域网 listener 直接展示网卡配置，
/// 本机监听时渲染强制回到回环目标。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::features::preferences) enum McpConfigTarget {
    Local,
    #[default]
    Lan,
}

/// MCP 草稿只在启动时提交，编辑期间不干扰现有 listener。
pub(in crate::features::preferences) struct McpSettingsState {
    pub(in crate::features::preferences) port: Entity<InputState>,
    _port_subscription: Subscription,
    pub(in crate::features::preferences) bind_mode: McpBindMode,
    baseline: McpSettings,
    pub(in crate::features::preferences) edited: bool,
    pub(in crate::features::preferences) conflict: bool,
    pub(in crate::features::preferences) outcome_unknown: bool,
    pub(in crate::features::preferences) busy: bool,
    pub(in crate::features::preferences) addresses: Vec<Ipv4Addr>,
    pub(in crate::features::preferences) selected_address: Option<Ipv4Addr>,
    pub(in crate::features::preferences) config_target: McpConfigTarget,
    pub(in crate::features::preferences) addresses_loading: bool,
    pub(in crate::features::preferences) addresses_generation: u64,
    pub(in crate::features::preferences) message: Option<String>,
    pub(in crate::features::preferences) is_error: bool,
    generation: u64,
    pending: Option<McpSettings>,
}

impl McpSettingsState {
    /// 建立稳定输入实体，保留数字输入中的中间态与原生焦点。
    pub(super) fn new(window: &mut Window, cx: &mut Context<PreferencesPanel>) -> Self {
        let port = cx.new(|cx| InputState::new(window, cx).placeholder("37657"));
        let subscription = cx.subscribe(&port, |panel, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                panel.mcp_settings.refresh_edit_state(cx);
                cx.notify();
            }
        });
        Self {
            port,
            _port_subscription: subscription,
            bind_mode: McpBindMode::Loopback,
            baseline: McpSettings::default(),
            edited: false,
            conflict: false,
            outcome_unknown: false,
            busy: false,
            addresses: Vec::new(),
            selected_address: None,
            config_target: McpConfigTarget::Lan,
            addresses_loading: false,
            addresses_generation: 0,
            message: None,
            is_error: false,
            generation: 0,
            pending: None,
        }
    }

    /// 输入中间态留在表单内；提交与渲染共用校验，避免按钮可用却提交失败。
    pub(in crate::features::preferences) fn valid_port(&self, cx: &gpui_kit::App) -> Option<u16> {
        let value = self.port.read(cx).value();
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value.parse::<u16>().ok().filter(|port| *port != 0)
    }

    /// 按实际值比较草稿，切回原范围或端口后立即恢复启动操作，不制造无效应用。
    fn refresh_edit_state(&mut self, cx: &gpui_kit::App) {
        self.edited = self.bind_mode != self.baseline.bind_mode
            || self.valid_port(cx) != Some(self.baseline.port);
        if !self.outcome_unknown {
            self.message = None;
            self.is_error = false;
        }
    }

    /// 操作已返回未知结果后，成功的权威回读才能解除门禁；确认未保存时保留输入供显式重试。
    /// 同 revision 的回读也必须处理，否则未落盘的提交会永远停留在“核对结果”。
    pub(super) fn reconcile_readback(
        &mut self,
        settings: &McpSettings,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if !self.outcome_unknown || self.busy {
            return;
        }
        self.outcome_unknown = false;
        if self.pending.take().as_ref() == Some(settings) {
            self.edited = false;
            self.message = None;
            self.is_error = false;
            self.sync(settings, window, cx);
        } else {
            self.conflict = settings != &self.baseline;
            self.refresh_edit_state(cx);
            if !self.conflict {
                self.message = Some(
                    crate::locale::text(
                        "更改尚未保存，点击启动重试。",
                        "Changes were not saved. Choose Start to retry.",
                    )
                    .to_string(),
                );
            }
        }
    }

    /// 外部重读不得覆盖用户正在编辑的端口；不同基线必须显式恢复后再应用。
    pub(super) fn sync(
        &mut self,
        settings: &McpSettings,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if self.outcome_unknown {
            if self.pending.as_ref() != Some(settings) {
                self.conflict = true;
                return;
            }
            self.outcome_unknown = false;
            self.pending = None;
            self.edited = false;
            self.message = None;
            self.is_error = false;
        }
        if self.edited {
            self.conflict = settings != &self.baseline;
            return;
        }
        self.baseline = settings.clone();
        self.bind_mode = settings.bind_mode;
        self.conflict = false;
        super::set_input(&self.port, settings.port.to_string(), window, cx);
    }
}

impl PreferencesPanel {
    /// 停止只收尾原 owner；启动先提交当前草稿，再由 runtime 读取最新持久配置。
    pub(in crate::features::preferences) fn toggle_mcp_settings(&mut self, cx: &mut Context<Self>) {
        if self.mcp_settings.busy || self.connectivity.mcp_operation.is_some() {
            return;
        }
        match self.connectivity.current_mcp_snapshot().state {
            McpRuntimeState::Stopped | McpRuntimeState::Failed if self.mcp_settings.edited => {
                self.start_mcp_with_settings(cx);
            }
            _ => self.connectivity.toggle_mcp(cx),
        }
    }

    /// 草稿的监听模式不触发文件保存或服务重启，避免鼠标误触产生网络副作用。
    pub(in crate::features::preferences) fn select_mcp_bind_mode(
        &mut self,
        mode: McpBindMode,
        cx: &mut Context<Self>,
    ) {
        if self.mcp_settings.busy
            || self.connectivity.mcp_operation.is_some()
            || self.mcp_settings.outcome_unknown
            || self.settings_controls_locked()
        {
            return;
        }
        self.mcp_settings.bind_mode = mode;
        self.mcp_settings.refresh_edit_state(cx);
        cx.notify();
    }

    /// 连接配置目标只影响预览和复制内容，选择时不读取令牌或触发网络操作。
    pub(in crate::features::preferences) fn select_mcp_config_target(
        &mut self,
        target: McpConfigTarget,
        address: Option<Ipv4Addr>,
        cx: &mut Context<Self>,
    ) {
        self.mcp_settings.config_target = target;
        if let Some(address) = address {
            self.mcp_settings.selected_address = Some(address);
        }
        self.connectivity.mcp_copied = false;
        self.connectivity.mcp_copy_error = None;
        cx.notify();
    }

    /// 统一复制入口按当前预览目标选择本机或局域网配置，避免两个按钮产生歧义。
    pub(in crate::features::preferences) fn copy_mcp_config(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.connectivity.current_mcp_snapshot();
        let lan = snapshot.state == McpRuntimeState::Running
            && snapshot.bind_mode == McpBindMode::Lan
            && self.mcp_settings.config_target == McpConfigTarget::Lan
            && self.mcp_settings.selected_address.is_some();
        if lan {
            self.copy_mcp_lan_config(cx);
        } else {
            self.connectivity.copy_mcp_json(cx);
        }
    }

    /// 放弃本地 MCP 草稿并重新采用已确认的设置基线，不触碰正在运行的 listener。
    pub(in crate::features::preferences) fn reset_mcp_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mcp_settings.outcome_unknown {
            return;
        }
        let Some(settings) = self
            .settings
            .as_ref()
            .map(|draft| draft.value().mcp.clone())
        else {
            return;
        };
        self.mcp_settings.edited = false;
        self.mcp_settings.message = None;
        self.mcp_settings.sync(&settings, window, cx);
        cx.notify();
    }

    /// 启动前用 CAS 保存当前输入；保存失败或未知结果不继续启动，避免重复监听。
    fn start_mcp_with_settings(&mut self, cx: &mut Context<Self>) {
        if self.mcp_settings.busy
            || self.connectivity.mcp_operation.is_some()
            || self.mcp_settings.conflict
            || self.mcp_settings.outcome_unknown
            || !matches!(
                self.connectivity.current_mcp_snapshot().state,
                McpRuntimeState::Stopped | McpRuntimeState::Failed
            )
        {
            return;
        }
        let Some(draft) = self.settings.as_ref() else {
            return;
        };
        if draft.is_dirty()
            || !matches!(
                draft.status(),
                super::super::DraftStatus::Ready | super::super::DraftStatus::Failed
            )
        {
            self.mcp_settings.is_error = true;
            self.mcp_settings.message = Some(
                crate::locale::text(
                    "请先完成或重新读取其他设置。",
                    "Finish or reload the other settings first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let port = self.mcp_settings.valid_port(cx);
        let Some(port) = port else {
            self.mcp_settings.is_error = true;
            self.mcp_settings.message = Some(
                crate::locale::text(
                    "请输入 1 至 65535 的端口。",
                    "Enter a port from 1 to 65535.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(runtime) = self.connectivity.runtime.clone() else {
            return;
        };
        let expected_revision = draft.revision().clone();
        let requested = McpSettings {
            bind_mode: self.mcp_settings.bind_mode,
            port,
            token_ref: draft.value().mcp.token_ref.clone(),
        };
        let mut pending = requested.clone();
        if pending.bind_mode == McpBindMode::Lan && pending.token_ref.is_none() {
            pending.token_ref =
                Some(kerminal_application::mcp_runtime::MCP_TOKEN_REFERENCE.to_owned());
        }
        let executor = self.connectivity.executor.clone();
        self.mcp_settings.generation = self.mcp_settings.generation.wrapping_add(1).max(1);
        let generation = self.mcp_settings.generation;
        self.mcp_settings.busy = true;
        self.mcp_settings.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let revision = expected_revision.clone();
            let result = executor
                .run(move |cancellation| async move {
                    runtime
                        .apply_settings(requested, expected_revision, cancellation)
                        .await
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.mcp_settings.generation != generation {
                    return;
                }
                panel.mcp_settings.busy = false;
                match result {
                    Ok(mut saved) => {
                        Self::resolve_legacy_terminal_theme(&mut saved.settings, window, cx);
                        panel.mcp_settings.edited = false;
                        panel.mcp_settings.is_error = false;
                        // watcher 可能先收到本次写入；同一保存回声不能让已提交的启动意图失效。
                        let can_install = panel.settings.as_ref().is_some_and(|draft| {
                            !draft.is_dirty()
                                && (draft.revision() == &revision
                                    || (draft.revision() == &saved.revision
                                        && draft.value() == &saved.settings))
                        });
                        if can_install {
                            let settings = saved.settings.clone();
                            panel.install_settings(saved, window, cx);
                            cx.emit(PreferencesEvent::SettingsSaved(settings));
                            panel.connectivity.start_mcp(cx);
                        } else {
                            panel.mcp_settings.conflict = true;
                        }
                    }
                    Err(error) => {
                        panel.mcp_settings.is_error = true;
                        if error.public_code()
                            == kerminal_application::PublicErrorCode::OutcomeUnknown
                        {
                            panel.mcp_settings.outcome_unknown = true;
                            panel.mcp_settings.pending = Some(pending);
                        }
                        if matches!(
                            error.public_code(),
                            kerminal_application::PublicErrorCode::Conflict
                                | kerminal_application::PublicErrorCode::OutcomeUnknown
                        ) {
                            panel.spawn_settings_revision_check(cx);
                        }
                        panel.mcp_settings.message = Some(super::localized_error(&error));
                        panel.connectivity.refresh_mcp(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 未知提交结果只触发只读核对，不重复执行上一次监听或文件写入。
    pub(in crate::features::preferences) fn check_mcp_settings(&mut self, cx: &mut Context<Self>) {
        self.spawn_settings_revision_check(cx);
    }

    /// 地址从当前运行 listener 获取，避免把 0.0.0.0 或失效网卡写进客户端配置。
    pub(in crate::features::preferences) fn refresh_mcp_addresses(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.connectivity.current_mcp_snapshot();
        if snapshot.state != McpRuntimeState::Running || snapshot.bind_mode != McpBindMode::Lan {
            self.mcp_settings.addresses_generation = self
                .mcp_settings
                .addresses_generation
                .wrapping_add(1)
                .max(1);
            self.mcp_settings.addresses_loading = false;
            self.mcp_settings.addresses.clear();
            self.mcp_settings.selected_address = None;
            self.connectivity.mcp_copied = false;
            self.connectivity.mcp_copy_error = None;
            cx.notify();
            return;
        }
        let Some(runtime) = self.connectivity.runtime.clone() else {
            return;
        };
        let executor = self.connectivity.executor.clone();
        self.mcp_settings.addresses_generation = self
            .mcp_settings
            .addresses_generation
            .wrapping_add(1)
            .max(1);
        let generation = self.mcp_settings.addresses_generation;
        self.mcp_settings.addresses_loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { runtime.lan_addresses().await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.mcp_settings.addresses_generation != generation {
                    return;
                }
                panel.mcp_settings.addresses_loading = false;
                panel.connectivity.mcp_copied = false;
                panel.connectivity.mcp_copy_error = None;
                match result {
                    Ok(addresses) => {
                        if panel.mcp_settings.is_error && !panel.mcp_settings.outcome_unknown {
                            panel.mcp_settings.is_error = false;
                            panel.mcp_settings.message = None;
                        }
                        let selection = panel
                            .mcp_settings
                            .selected_address
                            .filter(|ip| addresses.contains(ip))
                            .or_else(|| addresses.iter().copied().find(Ipv4Addr::is_private))
                            .or_else(|| addresses.first().copied());
                        panel.mcp_settings.addresses = addresses;
                        panel.mcp_settings.selected_address = selection;
                    }
                    Err(error) => {
                        panel.mcp_settings.addresses.clear();
                        panel.mcp_settings.selected_address = None;
                        panel.mcp_settings.is_error = true;
                        panel.mcp_settings.message = Some(super::localized_error(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 只在用户点击复制后接触令牌，成功状态以系统剪贴板回读为准。
    pub(in crate::features::preferences) fn copy_mcp_lan_config(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.mcp_settings.selected_address else {
            return;
        };
        let Some(runtime) = self.connectivity.runtime.clone() else {
            return;
        };
        let executor = self.connectivity.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { runtime.lan_client_config(address).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                // 令牌读取期间目标或运行态可能已切换；剪贴板仍完成原请求，但反馈只作用于当前目标。
                let running_lan = panel.connectivity.runtime.as_ref().is_some_and(|runtime| {
                    let snapshot = runtime.snapshot();
                    snapshot.state == McpRuntimeState::Running
                        && snapshot.bind_mode == McpBindMode::Lan
                });
                let still_current = panel.mcp_settings.config_target == McpConfigTarget::Lan
                    && panel.mcp_settings.selected_address == Some(address)
                    && running_lan;
                match result {
                    Ok(config) => {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                            config.as_str().to_owned(),
                        ));
                        let matches = cx
                            .read_from_clipboard()
                            .and_then(|clipboard| clipboard.text())
                            .as_deref()
                            == Some(config.as_str());
                        if still_current {
                            panel.connectivity.mcp_copied = matches;
                            panel.connectivity.mcp_copy_error = (!matches).then(|| {
                                crate::locale::text("复制失败，请重试。", "Copy failed. Try again.")
                                    .to_string()
                            });
                        }
                    }
                    Err(error) => {
                        if still_current {
                            panel.connectivity.mcp_copied = false;
                            panel.connectivity.mcp_copy_error =
                                Some(super::localized_error(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
