//! 插件表单由连接面板持有，统一处理草稿、异步操作和未知提交结果。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use gpui_kit::{App, Context, Window};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::host_types::{PluginHost, PluginHostDraft},
};
use kerminal_domain::{identity::HostId, preferences::ConfigRevision};
use tokio_util::sync::CancellationToken;

use crate::plugins::host_forms::{
    HostFormFactories, HostFormFactory, HostFormInstance, HostFormSeed,
};

use super::panel::{ConnectionsEvent, ConnectionsPanel};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Idle,
    Testing,
    Saving,
    Unknown,
    Reading,
}

struct PendingSave {
    candidate: PluginHostDraft,
    baseline: Option<ConfigRevision>,
}

struct HostedForm {
    factory: Arc<dyn HostFormFactory>,
    instance: Box<dyn HostFormInstance>,
    original: Option<PluginHost>,
    host_id: HostId,
    generation: u64,
    phase: Phase,
    cancellation: Option<CancellationToken>,
    pending_save: Option<PendingSave>,
    readback: Option<Option<PluginHost>>,
}

/// 一个窗口持有各类型草稿；切换协议不销毁其他插件的输入。
#[derive(Default)]
pub(super) struct PluginHostForms {
    factories: Arc<HostFormFactories>,
    forms: BTreeMap<String, HostedForm>,
    active: Option<String>,
    loading: Option<HostId>,
    loading_error: Option<String>,
    generation: u64,
}

impl PluginHostForms {
    /// 加载和已创建的实例均属于编辑任务，Root 需要维持同一弹层。
    pub(super) fn is_open(&self) -> bool {
        self.loading.is_some() || !self.forms.is_empty()
    }

    /// 当前可见表单与其他协议草稿分开持有。
    pub(super) fn has_active(&self) -> bool {
        self.active.is_some()
    }

    /// 加载时仅展示已有弹层，不提前创建未经校验的实例。
    pub(super) fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// 失败时保留原始身份，只有明确重试才重新读取该记录。
    pub(super) fn loading_error(&self) -> Option<&str> {
        self.loading_error.as_deref()
    }

    /// 已保存记录不允许通过协议栏切换成另一种类型。
    pub(super) fn active_is_existing(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.original.is_some())
    }

    /// 未知提交在整个编辑任务范围内阻止关闭、切换和重试。
    pub(super) fn write_blocked(&self) -> bool {
        self.forms
            .values()
            .any(|form| matches!(form.phase, Phase::Saving | Phase::Unknown | Phase::Reading))
    }

    /// 所有未展示的插件草稿也参加丢弃确认。
    pub(super) fn is_dirty(&self, cx: &App) -> bool {
        self.forms.values().any(|form| form.instance.is_dirty(cx))
    }

    /// 显式聚焦才交还给插件，避免重绘时改变用户焦点。
    pub(super) fn focus_active(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(form) = self.active_form_mut() else {
            return false;
        };
        form.instance.focus(window, cx);
        true
    }

    /// 切换协议时取消当前测试，实例和草稿留待用户返回。
    pub(super) fn deactivate(&mut self) {
        if let Some(form) = self.active_form_mut()
            && let Some(cancel) = &form.cancellation
        {
            cancel.cancel();
        }
        self.active = None;
    }

    /// 关闭时传播取消并使已经排队的回执失效。
    pub(super) fn clear(&mut self) {
        for form in self.forms.values_mut() {
            if let Some(cancel) = form.cancellation.take() {
                cancel.cancel();
            }
        }
        self.forms.clear();
        self.active = None;
        self.loading = None;
        self.loading_error = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// 渲染只借用当前实例，不复制或重建 GPUI 实体。
    fn active_form(&self) -> Option<&HostedForm> {
        self.forms.get(self.active.as_ref()?)
    }

    /// 状态变更只作用于当前明确选定的插件类型。
    fn active_form_mut(&mut self) -> Option<&mut HostedForm> {
        self.forms.get_mut(self.active.as_ref()?)
    }

    /// 协议栏使用注册表提供的稳定顺序与名称。
    pub(super) fn options(&self) -> Vec<(String, gpui_kit::SharedString)> {
        self.factories
            .iter()
            .map(|factory| (factory.type_id().to_owned(), factory.display_name()))
            .collect()
    }

    /// 显示与选择使用同一个类型身份，避免仅凭标签判断。
    pub(super) fn active_type(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// 绘制使用插件保留的视图句柄。
    pub(super) fn view(&self) -> Option<gpui_kit::AnyView> {
        self.active_form().map(|form| form.instance.view())
    }

    /// 保存和探测的操作态只由 owner 修改。
    pub(super) fn busy(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.phase != Phase::Idle)
    }

    /// 只有未知结果才能发起或显示读回操作。
    pub(super) fn unknown(&self) -> bool {
        self.active_form()
            .is_some_and(|form| matches!(form.phase, Phase::Unknown | Phase::Reading))
    }

    /// 等待读回时防止同一主机产生并发核对。
    pub(super) fn reading(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.phase == Phase::Reading)
    }

    /// 只有成功读回后才允许用户采用持久状态。
    pub(super) fn has_readback(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.readback.is_some())
    }

    /// 测试按钮由实例能力与实际运行状态共同决定。
    pub(super) fn supports_test(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.instance.supports_test())
    }

    /// 探测取消与保存取消具有不同语义，不能中断可能已提交的写入。
    pub(super) fn testing(&self) -> bool {
        self.active_form()
            .is_some_and(|form| form.phase == Phase::Testing)
    }
}

impl ConnectionsPanel {
    /// 组合根注入已核对的工厂；表单不会获得 Root 或配置仓储句柄。
    pub fn with_host_form_factories(mut self, factories: Arc<HostFormFactories>) -> Self {
        self.plugin_forms.factories = factories;
        self
    }

    /// 新建入口保留现有协议草稿；重复选择恢复原实例和焦点。
    pub(super) fn activate_plugin_form(
        &mut self,
        type_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugin_forms.write_blocked()
            || self.editor_protocol_locked()
            || self.refuse_conflict_reload_mutation(cx)
        {
            return;
        }
        if self.plugin_forms.active_type() == Some(type_id) {
            return;
        }
        if !self.plugin_forms.forms.contains_key(type_id) {
            let Some(factory) = self.plugin_forms.factories.by_type(type_id) else {
                return;
            };
            let seed = HostFormSeed {
                original: None,
                default_group: self
                    .editor
                    .as_ref()
                    .and_then(|editor| editor.selected_group.clone()),
            };
            let mut instance = match factory.create(&seed, window, cx) {
                Ok(instance) => instance,
                Err(error) => {
                    self.status = Some(error.public_message().to_owned());
                    cx.notify();
                    return;
                }
            };
            instance.set_enabled(
                self.recovery.mode
                    != kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
                window,
                cx,
            );
            self.plugin_forms.generation = self.plugin_forms.generation.wrapping_add(1);
            self.plugin_forms.forms.insert(
                type_id.to_owned(),
                HostedForm {
                    factory,
                    instance,
                    original: None,
                    host_id: HostId::new(),
                    generation: self.plugin_forms.generation,
                    phase: Phase::Idle,
                    cancellation: None,
                    pending_save: None,
                    readback: None,
                },
            );
        }
        self.reset_connection_probe();
        self.cancel_local_host_background_requests();
        self.invalidate_host_file_picker();
        self.plugin_forms.deactivate();
        self.plugin_forms.active = Some(type_id.to_owned());
        self.status = None;
        self.plugin_forms.focus_active(window, cx);
        cx.notify();
    }

    /// 编辑始终按主机 ID 重新读取配置，避免从旧导航快照产生写入候选。
    pub fn begin_edit_plugin_host(
        &mut self,
        host_id: HostId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_open() || self.refuse_host_editor_replacement(cx) {
            return;
        }
        self.start_plugin_host_load(host_id, cx);
    }

    /// 读取失败保留同一个目标，避免重试时跟随变化的导航选择。
    pub(super) fn retry_plugin_host_load(&mut self, cx: &mut Context<Self>) {
        if self.plugin_forms.loading_error.is_none() {
            return;
        }
        if let Some(host_id) = self.plugin_forms.loading.clone() {
            self.start_plugin_host_load(host_id, cx);
        }
    }

    /// 每次读取使用新世代，关闭或再次尝试后不能接纳旧结果。
    fn start_plugin_host_load(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        self.plugin_forms.generation = self.plugin_forms.generation.wrapping_add(1);
        let generation = self.plugin_forms.generation;
        self.plugin_forms.loading = Some(host_id.clone());
        self.plugin_forms.loading_error = None;
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let target = host_id.clone();
            let result = executor.run(move |_| async move { hosts.load_plugin_host(&target).await }).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.plugin_forms.generation != generation || panel.plugin_forms.loading.as_ref() != Some(&host_id) { return; }
                let result = result.and_then(|host| host.ok_or_else(|| AppError::not_found("插件主机不存在")));
                match result {
                    Ok(host) if host.is_available() => {
                        if let Some(factory) = panel.plugin_forms.factories.get(&host.provider_id, &host.type_id) {
                            let seed = HostFormSeed { original: Some(host.clone()), default_group: host.group_id.clone() };
                            match factory.create(&seed, window, cx) {
                                Ok(mut instance) => {
                                    instance.set_enabled(
                                        panel.recovery.mode != kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
                                        window,
                                        cx,
                                    );
                                    let type_id = host.type_id.clone();
                                    panel.plugin_forms.forms.insert(type_id.clone(), HostedForm {
                                        factory, instance, original: Some(host), host_id,
                                        generation, phase: Phase::Idle, cancellation: None,
                                        pending_save: None, readback: None,
                                    });
                                    panel.plugin_forms.loading = None;
                                    panel.plugin_forms.active = Some(type_id);
                                    panel.status = None;
                                    panel.plugin_forms.focus_active(window, cx);
                                }
                                Err(error) => panel.plugin_forms.loading_error = Some(error.public_message().to_owned()),
                            }
                        } else {
                            panel.plugin_forms.loading_error = Some(crate::locale::text("插件主机表单不可用。", "The plugin host form is unavailable.").to_string());
                        }
                    }
                    Ok(_) => panel.plugin_forms.loading_error = Some(crate::locale::text("插件主机配置不可用。", "The plugin host configuration is unavailable.").to_string()),
                    Err(error) => panel.plugin_forms.loading_error = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    /// 测试使用未保存输入并传播取消，完成前不允许覆盖该实例的字段。
    pub(super) fn test_plugin_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(type_id) = self.plugin_forms.active.clone() else {
            return;
        };
        let Some(form) = self.plugin_forms.forms.get_mut(&type_id) else {
            return;
        };
        if form.phase == Phase::Testing {
            if let Some(cancel) = &form.cancellation {
                cancel.cancel();
            }
            return;
        }
        if form.phase != Phase::Idle || !form.instance.supports_test() {
            return;
        }
        let operation = match form.instance.begin_test(cx) {
            Ok(operation) => operation,
            Err(error) => {
                self.status = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        let generation = form.generation;
        let cancel = CancellationToken::new();
        form.cancellation = Some(cancel.clone());
        form.phase = Phase::Testing;
        form.instance.set_enabled(false, window, cx);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor.run_cancellable(cancel.clone(), operation).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                let is_active = panel.plugin_forms.active.as_deref() == Some(type_id.as_str());
                let Some(form) = panel.plugin_forms.forms.get_mut(&type_id) else {
                    return;
                };
                if form.generation != generation || form.phase != Phase::Testing {
                    return;
                }
                form.phase = Phase::Idle;
                form.cancellation = None;
                form.instance.set_enabled(true, window, cx);
                if is_active {
                    panel.status = Some(if cancel.is_cancelled() {
                        crate::locale::text("测试已取消。", "Test cancelled.").to_string()
                    } else {
                        match result {
                            Ok(()) => {
                                crate::locale::text("测试成功。", "Test succeeded.").to_string()
                            }
                            Err(error) => error.public_message().to_owned(),
                        }
                    });
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 预分配 ID 并冻结候选后执行 CAS；未知结果保留原候选供精确读回。
    pub(super) fn save_plugin_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving
            || self.plugin_forms.write_blocked()
            || self.recovery.mode
                == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        let Some(type_id) = self.plugin_forms.active.clone() else {
            return;
        };
        let Some(form) = self.plugin_forms.forms.get_mut(&type_id) else {
            return;
        };
        if form.phase != Phase::Idle {
            return;
        }
        let mut draft = match form.instance.snapshot(cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.status = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        if draft.provider_id != form.factory.provider_id()
            || draft.type_id != form.factory.type_id()
            || draft.id.as_ref().is_some_and(|id| id != &form.host_id)
        {
            self.status = Some(
                crate::locale::text(
                    "插件表单返回的主机身份不匹配。",
                    "The plugin form returned an invalid host identity.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        draft.id = Some(form.host_id.clone());
        let prepare = match form.instance.begin_prepare(cx) {
            Ok(prepare) => prepare,
            Err(error) => {
                self.status = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        let baseline = form.original.as_ref().map(|host| host.revision.clone());
        let generation = form.generation;
        form.pending_save = Some(PendingSave {
            candidate: draft.clone(),
            baseline: baseline.clone(),
        });
        form.readback = None;
        form.phase = Phase::Saving;
        form.instance.set_enabled(false, window, cx);
        self.saving = true;
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let draft = prepare(draft, cancel).await?;
                    let result = hosts.save_plugin_host(draft.clone(), baseline).await;
                    Ok((draft, result))
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                let Some(form) = panel.plugin_forms.forms.get_mut(&type_id) else {
                    return;
                };
                if form.generation != generation || form.phase != Phase::Saving {
                    return;
                }
                panel.saving = false;
                let result = match result {
                    Ok((candidate, result)) => {
                        if let Some(pending) = form.pending_save.as_mut() {
                            pending.candidate = candidate;
                        }
                        result
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(host)
                        if host.id == form.host_id
                            && host.provider_id == form.factory.provider_id()
                            && host.type_id == type_id =>
                    {
                        form.phase = Phase::Idle;
                        form.pending_save = None;
                        panel.finish_plugin_form_save(host, window, cx);
                    }
                    Ok(_) => {
                        form.phase = Phase::Unknown;
                        panel.status = Some(
                            crate::locale::text(
                                "保存回执身份异常，请读回核对。",
                                "The save receipt identity is invalid. Read back the host.",
                            )
                            .to_string(),
                        );
                    }
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        form.phase = Phase::Unknown;
                        panel.status = Some(
                            crate::locale::text(
                                "保存结果未知，请按原主机读回核对。",
                                "The save outcome is unknown. Read back the original host.",
                            )
                            .to_string(),
                        );
                    }
                    Err(error) => {
                        form.phase = Phase::Idle;
                        form.pending_save = None;
                        form.instance.set_enabled(true, window, cx);
                        panel.status = Some(error.public_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 已确认回执才关闭表单并通知导航刷新，避免把受理当作保存完成。
    fn finish_plugin_form_save(
        &mut self,
        host: PluginHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = host.id;
        self.close_unified_editor(window, cx);
        cx.emit(ConnectionsEvent::PluginHostSaved { host_id });
        self.reload(cx);
    }

    /// 未知结果只读原 ID；与候选一致或确认仍为基线时才自动解除对应门禁。
    pub(super) fn readback_plugin_form(&mut self, cx: &mut Context<Self>) {
        let Some(type_id) = self.plugin_forms.active.clone() else {
            return;
        };
        let Some(form) = self.plugin_forms.forms.get_mut(&type_id) else {
            return;
        };
        if form.phase != Phase::Unknown {
            return;
        }
        form.phase = Phase::Reading;
        form.readback = None;
        let generation = form.generation;
        let host_id = form.host_id.clone();
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor.run(move |_| async move { hosts.load_plugin_host(&host_id).await }).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                let Some(form) = panel.plugin_forms.forms.get_mut(&type_id) else { return; };
                if form.generation != generation || form.phase != Phase::Reading { return; }
                form.phase = Phase::Unknown;
                match result {
                    Ok(readback) => {
                        let Some(pending) = form.pending_save.as_ref() else { return; };
                        let same_as_candidate = readback.as_ref().is_some_and(|host| plugin_readback_matches(host, &pending.candidate, pending.baseline.as_ref()));
                        let same_as_baseline = match (&readback, &pending.baseline) {
                            (None, None) => true,
                            (Some(host), Some(baseline)) => &host.revision == baseline,
                            _ => false,
                        };
                        if same_as_candidate {
                            form.phase = Phase::Idle;
                            form.pending_save = None;
                            if let Some(host) = readback { panel.finish_plugin_form_save(host, window, cx); }
                        } else if same_as_baseline {
                            form.phase = Phase::Idle;
                            form.pending_save = None;
                            form.instance.set_enabled(true, window, cx);
                            panel.status = Some(crate::locale::text("读回仍为保存前状态，可以重试。", "The stored host is unchanged; retry is available.").to_string());
                        } else {
                            form.readback = Some(readback);
                            panel.status = Some(crate::locale::text("读回配置与保存候选不同，可采用当前存储状态。", "The stored host differs from the candidate. You can adopt the stored state.").to_string());
                        }
                    }
                    Err(error) => panel.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    /// 用户明确采用读回后即可结束未知提交；类型变化时关闭旧表单，避免把新配置送入旧工厂。
    pub(super) fn use_plugin_readback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(type_id) = self.plugin_forms.active.clone() else {
            return;
        };
        let Some(form) = self.plugin_forms.forms.get_mut(&type_id) else {
            return;
        };
        if form.phase != Phase::Unknown {
            return;
        }
        let Some(readback) = form.readback.take() else {
            return;
        };
        if let Some(host) = readback {
            if host.provider_id != form.factory.provider_id()
                || host.type_id != type_id
                || !host.is_available()
            {
                form.phase = Phase::Idle;
                form.pending_save = None;
                self.finish_plugin_form_save(host, window, cx);
                self.status = Some(
                    crate::locale::text(
                        "已采用当前存储状态。主机类型或提供方已变化，可从列表重新打开。",
                        "The stored state was adopted. The host type or provider changed; reopen it from the list.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            let seed = HostFormSeed {
                original: Some(host.clone()),
                default_group: host.group_id.clone(),
            };
            match form.factory.create(&seed, window, cx) {
                Ok(mut instance) => {
                    instance.set_enabled(
                        self.recovery.mode
                            != kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
                        window,
                        cx,
                    );
                    form.instance = instance;
                    form.original = Some(host);
                    form.phase = Phase::Idle;
                    form.pending_save = None;
                    self.status = None;
                }
                Err(error) => {
                    form.phase = Phase::Idle;
                    form.pending_save = None;
                    self.finish_plugin_form_save(host, window, cx);
                    self.status = Some(error.public_message().to_owned());
                }
            }
        } else {
            form.phase = Phase::Idle;
            form.pending_save = None;
            self.close_unified_editor(window, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_host_form_recovery.rs"]
mod tests;

/// 读回必须匹配主机身份、公共字段及已解码配置，不能仅凭 revision 宣称候选已提交。
fn plugin_readback_matches(
    host: &PluginHost,
    draft: &PluginHostDraft,
    baseline: Option<&ConfigRevision>,
) -> bool {
    draft.id.as_ref() == Some(&host.id)
        && host.provider_id == draft.provider_id
        && host.type_id == draft.type_id
        && host.name == draft.name.trim()
        && host.group_id == draft.group_id
        && host.sort_order == draft.sort_order
        && host.configuration_version == draft.configuration_version
        && host.configuration.as_ref() == Some(&draft.configuration)
        && baseline != Some(&host.revision)
}

#[cfg(feature = "right-rail-acceptance")]
impl ConnectionsPanel {
    /// 原生验收选择插件主机类型；沿用协议栏的生产入口，不直接构造表单项。
    pub fn acceptance_activate_plugin_form(
        &mut self,
        type_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_plugin_form(type_id, window, cx);
    }

    /// 返回活动表单身份、阶段、控件值与面板文案；未打开时明确返回空。
    pub fn acceptance_plugin_form_snapshot(&self, cx: &App) -> Option<serde_json::Value> {
        let type_id = self.plugin_forms.active.as_ref()?;
        let form = self.plugin_forms.forms.get(type_id)?;
        let phase = match form.phase {
            Phase::Idle => "idle",
            Phase::Testing => "testing",
            Phase::Saving => "saving",
            Phase::Unknown => "unknown",
            Phase::Reading => "reading",
        };
        Some(serde_json::json!({
            "typeId": type_id,
            "hostId": form.host_id.as_str(),
            "phase": phase,
            "writeBlocked": self.plugin_forms.write_blocked(),
            "status": self.status,
            "form": form.instance.acceptance_snapshot(cx).unwrap_or(serde_json::Value::Null),
        }))
    }

    /// 按 schema 指针写入活动表单；生产默认实现拒绝，只有支持验收的表单接受。
    pub fn acceptance_plugin_form_set_field(
        &mut self,
        path: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let Some(type_id) = self.plugin_forms.active.clone() else {
            return Err(AppError::not_found("插件主机表单未打开"));
        };
        let form = self
            .plugin_forms
            .forms
            .get_mut(&type_id)
            .ok_or_else(|| AppError::not_found("插件主机表单实例不存在"))?;
        form.instance.acceptance_set_field(path, value, window, cx)
    }

    /// 走生产测试按钮同一入口，异步回执由面板状态和被冻结控件证明。
    pub fn acceptance_plugin_form_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.test_plugin_form(window, cx);
    }

    /// 走生产保存按钮同一入口，Vault 准备和 CAS 仍由正式服务完成。
    pub fn acceptance_plugin_form_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_plugin_form(window, cx);
    }
}
