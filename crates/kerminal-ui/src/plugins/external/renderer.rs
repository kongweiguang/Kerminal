//! 外部原生视图实体使用的集合配置和窗口线程交互契约。
//!
//! 集合实体使用 `NativeCollection::new(config, window, cx)` 构造并实现
//! `EventEmitter<CollectionEvent>`。调用方在新页面完成后以
//! `apply_page(query: &CollectionQuery, rows: &[PageRow], total: usize, cx)` 更新缓存投影；
//! 子树查询按 parent_key 保留，不能只接受根查询。内联数据变化时以
//! `apply_data(data: Value, cx)` 更新；展开事件同时给出目标状态。
//! `config.data` 是未经字符串化的原始 JSON；RPC 初始范围在下一帧请求 `0..40`。
//!
//! @author kongweiguang

use std::{cmp::Ordering, collections::BTreeMap, ops::Range};

use kerminal_plugin_protocol::{
    native::NativeNode,
    runtime::{ViewFetchSort, ViewSortDirection},
};
use serde_json::Value;

/// 集合实体独占滚动和选择；节点结构与数据都来自宿主已经冻结的快照。
#[derive(Clone, Debug)]
pub struct NativeCollectionConfig {
    pub node_key: String,
    pub node: NativeNode,
    pub data: Value,
    pub translations: BTreeMap<String, String>,
}

/// 每个树父节点和表格排序都具有独立查询身份，避免不同请求共用分页缓存。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CollectionQuery {
    pub parent_key: Option<String>,
    pub sort: Option<ViewFetchSort>,
}

impl Ord for CollectionQuery {
    /// 用父键、列键和方向共同确定缓存位置；协议类型不需要暴露排序实现。
    fn cmp(&self, other: &Self) -> Ordering {
        /// 列与方向共同决定排序身份，缺省排序与显式排序也必须区分。
        fn key(sort: &Option<ViewFetchSort>) -> Option<(&str, bool)> {
            sort.as_ref().map(|sort| {
                (
                    sort.column.as_str(),
                    matches!(sort.direction, ViewSortDirection::Desc),
                )
            })
        }
        self.parent_key
            .cmp(&other.parent_key)
            .then_with(|| key(&self.sort).cmp(&key(&other.sort)))
    }
}

impl PartialOrd for CollectionQuery {
    /// 与全序共用比较规则，避免树和排序查询在有序缓存中互相覆盖。
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 分页缓存使用无歧义的内部身份；RPC 的 node 参数仍必须使用原始节点 ID。
pub fn collection_cache_key(node: &str, query: &CollectionQuery) -> String {
    /// 可选字符串包含长度与存在位，防止分隔符、空值和 Unicode 造成碰撞。
    fn append_part(key: &mut String, part: Option<&str>) {
        if let Some(part) = part {
            key.push('1');
            key.push_str(&part.len().to_string());
            key.push(':');
            key.push_str(part);
        } else {
            key.push_str("0:");
        }
    }
    let mut key = String::new();
    append_part(&mut key, Some(node));
    append_part(&mut key, query.parent_key.as_deref());
    append_part(
        &mut key,
        query.sort.as_ref().map(|sort| sort.column.as_str()),
    );
    key.push(match query.sort.as_ref().map(|sort| sort.direction) {
        None => '-',
        Some(ViewSortDirection::Asc) => 'a',
        Some(ViewSortDirection::Desc) => 'd',
    });
    key
}

/// 视图实体消费意图并安排 RPC；绘制和布局回调本身不得直接发起请求。
#[derive(Clone, Debug)]
pub enum CollectionEvent {
    Visible {
        node: String,
        range: Range<usize>,
        query: CollectionQuery,
    },
    Sort {
        node: String,
        sort: ViewFetchSort,
    },
    Toggle {
        node: String,
        parent_key: String,
        expanded: bool,
    },
    Activate {
        node: String,
        action: String,
        item_key: String,
    },
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_renderer.rs"]
mod tests;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyView, App, AppContext as _, Context, Entity, IntoElement, Render, Subscription, Window,
    component::button::{Button, ButtonVariants as _},
    div,
    prelude::*,
};
use kerminal_application::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    plugin_ports::{PluginViewIdentity, PluginViewSavedState, PreparedPlugin},
};
use kerminal_domain::plugins::PluginRuntimeState;
use kerminal_plugin_protocol::native::NativeView;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    collections::NativeCollection,
    inputs::FieldControl,
    runtime::{AsyncViewRuntime, RuntimeEvent, RuntimeReply, RuntimeRequest, RuntimeSubscription},
    state::ActionTicket,
    view::NativeViewProjection,
};
use crate::plugins::content::{ContentInstanceIdentity, ContentStateChanged};

#[path = "recovery.rs"]
mod recovery;
pub(crate) use recovery::validate_native_content_state;

const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PENDING_UPDATES: usize = 128;

/// 工厂只提供冻结资源、应用端口及唯一身份；不将窗口或 Root 泄漏给插件。
pub struct NativeRendererConfig {
    pub prepared: Arc<PreparedPlugin>,
    pub view: Arc<NativeView>,
    pub identity: PluginViewIdentity,
    pub scope: Value,
    pub saved_state: Option<PluginViewSavedState>,
    pub initial_data: Option<Value>,
    pub runtime: Arc<dyn AsyncViewRuntime>,
    pub executor: ApplicationTaskExecutor,
    pub locale: String,
    pub content_identity: Option<ContentInstanceIdentity>,
    pub embedded_form: Option<(String, AnyView)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum OpenStatus {
    Opening,
    Ready,
    Recovering,
    Failed(String),
}

/// 一份实体负责一份草稿、控件和实例代次，所有 I/O 都经共享应用执行器。
pub struct NativeRenderer {
    pub(super) projection: NativeViewProjection,
    pub(super) prepared: Arc<PreparedPlugin>,
    pub(super) identity: PluginViewIdentity,
    pub(super) runtime: Arc<dyn AsyncViewRuntime>,
    pub(super) executor: ApplicationTaskExecutor,
    pub(super) process_generation: Option<u64>,
    pub(super) forms: BTreeMap<String, Result<super::forms::NativeForm, String>>,
    pub(super) fields: BTreeMap<String, FieldControl>,
    pub(super) code_blocks:
        BTreeMap<String, (Entity<gpui_kit::component::input::EditorState>, String)>,
    pub(super) field_errors: BTreeMap<String, String>,
    pub(super) subscriptions: Vec<Subscription>,
    pub(super) collections: BTreeMap<String, Entity<NativeCollection>>,
    pub(super) collection_queries: BTreeMap<String, (String, CollectionQuery)>,
    pub(super) locale: String,
    pub(super) embedded_form: Option<(String, AnyView)>,
    pub(super) active_tabs: BTreeMap<String, String>,
    pub(super) split_states:
        BTreeMap<String, Entity<gpui_kit::component::resizable::ResizableState>>,
    pub(super) cancellation: CancellationToken,
    pub(super) operation_cancellation: CancellationToken,
    reload_sequence: u64,
    pending_reload: Option<Arc<PreparedPlugin>>,
    reopened_generation: Option<u64>,
    retired_generation: Option<u64>,
    scope: Value,
    saved_state: Option<PluginViewSavedState>,
    owner: gpui_kit::AnyWindowHandle,
    content_identity: Option<ContentInstanceIdentity>,
    pub(super) status: OpenStatus,
    open_attempt: u64,
    open_in_flight: bool,
    reopen_in_flight: bool,
    reopen_attempt: u64,
    opened: bool,
    pending_updates: Vec<(u64, Value)>,
    needs_resync: bool,
    visible: bool,
    focused: bool,
    pub(super) error: Option<String>,
}

impl NativeRenderer {
    /// 验收只读取真实实体和非秘密输入；凭据明文永不进入诊断快照。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn external_plugin_acceptance_snapshot(&self, cx: &App) -> Value {
        let status = match &self.status {
            OpenStatus::Opening => "Opening",
            OpenStatus::Ready => "Ready",
            OpenStatus::Recovering => "Recovering",
            OpenStatus::Failed(_) => "Failed",
        };
        let failure = match &self.status {
            OpenStatus::Failed(message) => Some(message.clone()),
            _ => self.error.clone(),
        };
        let fields: Vec<Value> = self
            .fields
            .iter()
            .map(|(key, control)| {
                let (bind, kind, value) = match control {
                    FieldControl::Input {
                        state,
                        bind,
                        secret,
                        ..
                    } => (
                        bind,
                        if *secret { "secret" } else { "input" },
                        if *secret {
                            Value::Null
                        } else {
                            json!(state.read(cx).value().to_string())
                        },
                    ),
                    FieldControl::Textarea { state, bind, .. } => {
                        (bind, "textarea", json!(state.read(cx).value().to_string()))
                    }
                    FieldControl::Select { state, bind, .. } => {
                        (bind, "select", json!(state.read(cx).selected_value()))
                    }
                };
                let dirty = super::state::BindingPath::parse(bind)
                    .ok()
                    .is_some_and(|path| self.projection.state().is_dirty(&path));
                json!({"path": key, "binding": bind, "kind": kind, "value": value, "dirty": dirty})
            })
            .collect();
        json!({
            "status": status, "error": failure,
            "plugin": self.identity.plugin.as_str(), "view": self.identity.view,
            "instance": self.identity.instance, "instanceGeneration": self.identity.generation,
            "processGeneration": self.process_generation,
            "data": self.projection.state().data(),
            "dirty": self.projection.state().has_drafts(),
            "hasDrafts": self.projection.state().has_drafts(),
            "dirtyPaths": self.projection.state().acceptance_dirty_paths(),
            "fields": fields,
        })
    }

    /// 宿主草稿先恢复到独立数据模型；订阅先于打开请求，且插件只接收自身不透明状态。
    pub fn mount(
        config: NativeRendererConfig,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Entity<Self>> {
        let (saved_state, recovered_drafts) =
            recovery::decode(config.saved_state, &config.identity.view)?;
        let policy = recovery::DraftPolicy::from_view(&config.view.root, &config.prepared);
        if recovered_drafts
            .iter()
            .any(|draft| !policy.permits(&draft.path, &draft.value))
        {
            return Err(AppError::invalid("插件草稿与当前视图不兼容"));
        }
        let subscription = config.runtime.subscribe(&config.identity)?;
        let mut projection = NativeViewProjection::new(Arc::clone(&config.view), 1)
            .with_translations(
                Arc::new(config.prepared.localizations.clone()),
                config.locale.clone(),
            );
        if let Some(data) = config.initial_data {
            projection
                .state_mut()
                .replace_remote(data)
                .map_err(|_| AppError::invalid("插件视图初始数据无效"))?;
        }
        for draft in recovered_drafts {
            let path = super::state::BindingPath::parse(&draft.path)
                .map_err(|_| AppError::invalid("插件草稿绑定无效"))?;
            projection
                .state_mut()
                .restore_draft(&path, draft.value)
                .map_err(|_| AppError::invalid("插件草稿与当前数据不兼容"))?;
        }
        let entity = cx.new(|cx| {
            let mut renderer = Self {
                projection,
                prepared: config.prepared,
                identity: config.identity,
                runtime: config.runtime,
                executor: config.executor,
                process_generation: None,
                forms: BTreeMap::new(),
                fields: BTreeMap::new(),
                code_blocks: BTreeMap::new(),
                field_errors: BTreeMap::new(),
                subscriptions: Vec::new(),
                collections: BTreeMap::new(),
                collection_queries: BTreeMap::new(),
                locale: config.locale,
                embedded_form: config.embedded_form,
                active_tabs: BTreeMap::new(),
                split_states: BTreeMap::new(),
                cancellation: CancellationToken::new(),
                operation_cancellation: CancellationToken::new(),
                reload_sequence: 1,
                pending_reload: None,
                reopened_generation: None,
                retired_generation: None,
                scope: config.scope,
                saved_state,
                owner: window.window_handle(),
                content_identity: config.content_identity,
                status: OpenStatus::Opening,
                open_attempt: 0,
                open_in_flight: false,
                reopen_in_flight: false,
                reopen_attempt: 0,
                opened: false,
                pending_updates: Vec::new(),
                needs_resync: false,
                visible: false,
                focused: false,
                error: None,
            };
            renderer.prepare_controls(window, cx);
            renderer.prepare_collections(window, cx);
            renderer.watch(subscription, cx);
            renderer.open(cx);
            renderer
        });
        Ok(entity)
    }

    /// 快照只读取应用已经确认的缓存；打开失败或崩溃恢复期间保留已有插件恢复材料。
    pub fn cached_state(&self) -> AppResult<Option<PluginViewSavedState>> {
        match self.runtime.cached_state(&self.identity) {
            Ok(Some(state)) => Ok(Some(state)),
            Ok(None) => Ok(self.saved_state.clone()),
            Err(error)
                if matches!(
                    self.status,
                    OpenStatus::Opening | OpenStatus::Recovering | OpenStatus::Failed(_)
                ) =>
            {
                if self.saved_state.is_some() {
                    Ok(self.saved_state.clone())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 插件状态保持 opaque；宿主草稿使用独立版本零信封，绝不合并进插件的状态对象。
    pub(crate) fn content_saved_state(&self) -> AppResult<Option<PluginViewSavedState>> {
        let policy = self.draft_policy();
        let drafts = self
            .projection
            .state()
            .draft_values()
            .into_iter()
            .map(|(path, value)| recovery::NativeDraft {
                path: path.segments().join("."),
                value,
            })
            .filter(|draft| {
                !self
                    .field_errors
                    .keys()
                    .any(|binding| binding.strip_prefix('$').unwrap_or(binding) == draft.path)
                    && policy.permits(&draft.path, &draft.value)
            })
            .collect::<Vec<_>>();
        let state = match self.cached_state() {
            Ok(state) => state,
            Err(_)
                if !drafts.is_empty()
                    && matches!(
                        self.status,
                        OpenStatus::Opening | OpenStatus::Recovering | OpenStatus::Failed(_)
                    ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        recovery::encode(&self.identity.view, state, drafts)
    }

    /// 当前静态树和已展开表单共同限定可恢复字段，移除或改为秘密的绑定不会被旧草稿回填。
    fn draft_policy(&self) -> recovery::DraftPolicy {
        let mut policy = recovery::DraftPolicy::default();
        policy.add_node(&self.projection.view().root);
        for form in self.forms.values().filter_map(|form| form.as_ref().ok()) {
            policy.add_node(&form.root);
        }
        policy
    }

    /// 本地有效编辑只通知既有工作区去抖保存路径，不为每次按键请求插件快照。
    pub(super) fn notify_content_state_changed(&self, cx: &mut Context<Self>) {
        if let Some(identity) = self.content_identity.clone() {
            let owner = self.owner;
            cx.defer(move |cx| {
                let _ = owner.update(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(ContentStateChanged { identity }), cx);
                });
            });
        }
    }

    /// 任何在途秘密写入都必须有回执，避免关闭或提交时遗失写入结果。
    pub(crate) fn secret_write_pending(&self) -> bool {
        self.fields.values().any(|field| {
            matches!(field, FieldControl::Input { secret_write: Some(write), .. } if write.is_pending())
        })
    }

    /// 自定义设置视图的秘密和非法输入不进入数据模型，也必须参与关闭草稿门禁。
    pub(crate) fn has_uncommitted_inputs(&self, cx: &App) -> bool {
        self.projection.state().has_drafts() || !self.field_errors.is_empty()
            || self.fields.values().any(|field| matches!(field, FieldControl::Input { state, secret: true, .. } if !state.read(cx).value().is_empty()))
    }

    /// 等待打开回执后再关闭，避免关闭先于实例登记导致迟到的打开请求泄漏资源。
    /// 关闭 future 沿用调用方的绝对截止时间；失败时宿主仍持有实体以便重试。
    pub fn close(&self, deadline: Instant) -> super::runtime::RuntimeFuture<()> {
        if self.open_in_flight || self.reopen_in_flight {
            return Box::pin(async {
                Err(AppError::conflict("插件视图仍在打开，请稍后关闭"))
            });
        }
        if self.secret_write_pending() {
            return Box::pin(async {
                Err(AppError::conflict("插件秘密仍在保存，请稍后关闭"))
            });
        }
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: self.process_generation,
            method: "view.close".into(),
            params: json!({}),
            deadline,
            cancellation: CancellationToken::new(),
        };
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move { runtime.request(request).await.map(|_| ()) })
    }

    /// 显隐在宿主完成导航后通知；打开期间保存最新值，回执到达后统一发送。
    pub fn visibility_changed(&mut self, visible: bool, focused: bool, cx: &mut Context<Self>) {
        if self.visible == visible && self.focused == focused {
            return;
        }
        self.visible = visible;
        self.focused = focused;
        if matches!(self.status, OpenStatus::Ready) {
            self.send_visibility(cx);
        }
    }

    /// 重试只适用于明确失败的打开过程；动作失败不会借此自动重放。
    pub(super) fn open(&mut self, cx: &mut Context<Self>) {
        if self.open_in_flight {
            return;
        }
        let Some(attempt) = self.open_attempt.checked_add(1) else {
            self.status = OpenStatus::Failed("视图重试次数已耗尽".into());
            cx.notify();
            return;
        };
        self.open_attempt = attempt;
        self.open_in_flight = true;
        self.status = OpenStatus::Opening;
        self.error = None;
        let identity = self.identity.clone();
        let params = match &self.saved_state {
            Some(state) => {
                json!({"scope": self.scope, "state_version": state.state_version, "state": state.state})
            }
            None => json!({"scope": self.scope}),
        };
        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        let cancellation = self.cancellation.child_token();
        let deadline = Instant::now() + OPEN_TIMEOUT;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    runtime
                        .request(RuntimeRequest {
                            identity,
                            process_generation: None,
                            method: "view.open".into(),
                            params,
                            deadline,
                            cancellation: token,
                        })
                        .await
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                renderer.finish_open(attempt, result, window, cx)
            });
        })
        .detach();
    }

    /// 已登记的实例只能从应用层快照恢复；重新发送 open 会与现有实例冲突。
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_reload.is_some() {
            self.finish_development_reload(window, cx);
            if self.pending_reload.is_some() || self.process_generation.is_some() {
                return;
            }
            self.retry_reopen(cx);
            return;
        }
        if !self.opened {
            self.open(cx);
            return;
        }
        if self.process_generation.is_none() {
            self.retry_reopen(cx);
            return;
        }
        self.pending_updates.clear();
        if self.resync() {
            self.status = OpenStatus::Ready;
            self.error = None;
            self.sync_controls(window, cx);
            self.sync_collections(cx);
            self.send_visibility(cx);
            self.flush_collections(cx);
            if self.content_identity.is_some() {
                self.request_snapshot(cx);
            }
        } else {
            self.error = Some(
                crate::locale::text(
                    "插件视图数据仍无法恢复",
                    "Plugin view data is still unavailable",
                )
                .to_string(),
            );
        }
        cx.notify();
    }

    /// 单实例重试只恢复 view.open；等待实际回执期间禁止重复请求或关闭。
    fn retry_reopen(&mut self, cx: &mut Context<Self>) {
        if self.reopen_in_flight {
            return;
        }
        let Some(attempt) = self.reopen_attempt.checked_add(1) else {
            self.status = OpenStatus::Failed("插件视图重试代次已耗尽".into());
            cx.notify();
            return;
        };
        self.reopen_attempt = attempt;
        self.reopen_in_flight = true;
        self.status = OpenStatus::Recovering;
        self.error = None;
        let identity = self.identity.clone();
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        let cancellation = self.cancellation.child_token();
        let deadline = Instant::now() + OPEN_TIMEOUT;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| {
                    runtime.retry_reopen(identity, deadline, token)
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                if !renderer.reopen_in_flight || renderer.reopen_attempt != attempt {
                    return;
                }
                renderer.reopen_in_flight = false;
                match result {
                    Ok(process_generation) => {
                        renderer.finish_reopened(process_generation, window, cx)
                    }
                    Err(error) => {
                        renderer.status = OpenStatus::Failed(error.public_message().to_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// 无论来自自动恢复事件还是显式重试回执，都先核对缓存再恢复交互。
    fn finish_reopened(
        &mut self,
        process_generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reopen_in_flight = false;
        self.reopened_generation = Some(process_generation);
        if self.pending_reload.is_some() {
            self.finish_development_reload(window, cx);
        } else {
            self.process_generation = Some(process_generation);
            self.opened = true;
            self.reopened_generation = None;
            if self.resync() {
                self.status = OpenStatus::Ready;
                self.error = None;
                self.sync_controls(window, cx);
                self.sync_collections(cx);
                self.send_visibility(cx);
                self.flush_collections(cx);
            } else {
                self.status = OpenStatus::Failed("插件视图数据无法恢复，请重试".into());
            }
        }
        cx.notify();
    }

    /// 首次数据先于打开期间缓存的推送应用；两者都保留用户已经开始编辑的字段。
    fn finish_open(
        &mut self,
        attempt: u64,
        result: AppResult<RuntimeReply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if attempt != self.open_attempt {
            return;
        }
        self.open_in_flight = false;
        if !matches!(self.status, OpenStatus::Opening) {
            // 进程状态可能先于打开回执变化；成功登记仍需保留实例身份，不能再次发送 open。
            if result.is_ok() {
                self.opened = true;
                self.process_generation = None;
            }
            cx.notify();
            return;
        }
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                self.status = OpenStatus::Failed(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        self.process_generation = Some(reply.process_generation);
        self.opened = true;
        // 适配器读取快照时同步推进订阅修订游标，较旧缓冲补丁不再重放。
        self.pending_updates.clear();
        if !self.resync() {
            self.status = OpenStatus::Failed("插件视图数据无法恢复，请重试".into());
        } else {
            self.status = OpenStatus::Ready;
            self.send_visibility(cx);
            self.flush_collections(cx);
            if self.content_identity.is_some() {
                self.request_snapshot(cx);
            }
        }
        self.sync_controls(window, cx);
        self.sync_collections(cx);
        cx.notify();
    }

    /// 完整快照替换远端基线，同时保留草稿；不可恢复时保持可见错误。
    fn resync(&mut self) -> bool {
        match self.runtime.cached_data(&self.identity) {
            Ok(Some(data))
                if self
                    .projection
                    .state_mut()
                    .replace_remote(data.clone())
                    .is_ok() =>
            {
                self.needs_resync = false;
                true
            }
            _ => false,
        }
    }

    /// 重建控件前保留无法写入数据模型的秘密和无效输入，避免开发重载丢失用户输入。
    fn finish_development_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prepared) = self.pending_reload.clone() else {
            return;
        };
        if self.secret_write_pending() || !self.field_errors.is_empty() || self.fields.values().any(|field| matches!(field, FieldControl::Input { state, secret: true, .. } if !state.read(cx).value().is_empty())) {
            self.status = OpenStatus::Failed("请先处理未保存的秘密或无效输入，再重试加载".into());
            cx.notify();
            return;
        }
        let view = match super::factories::native_view(&prepared, &self.identity.view) {
            Ok(view) => view,
            Err(error) => {
                self.status = OpenStatus::Failed(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        self.projection
            .replace_view(view, Arc::new(prepared.localizations.clone()));
        self.prepared = prepared;
        self.forms.clear();
        self.fields.clear();
        self.code_blocks.clear();
        self.collections.clear();
        self.collection_queries.clear();
        self.split_states.clear();
        self.active_tabs.clear();
        self.subscriptions.clear();
        self.prepare_controls(window, cx);
        self.prepare_collections(window, cx);
        self.pending_reload = None;
        if let Some(generation) = self.reopened_generation.take() {
            self.process_generation = Some(generation);
            self.opened = true;
            if self.resync() {
                self.status = OpenStatus::Ready;
                self.error = None;
                self.sync_controls(window, cx);
                self.sync_collections(cx);
                self.send_visibility(cx);
                self.flush_collections(cx);
            } else {
                self.status = OpenStatus::Failed("插件视图数据无法恢复，请重试".into());
            }
        } else {
            self.status = OpenStatus::Recovering;
        }
        cx.notify();
    }

    /// 收到的事件始终校验完整实例身份；溢出和通道关闭不会被解释成正常状态。
    fn watch(&mut self, mut subscription: Box<dyn RuntimeSubscription>, cx: &mut Context<Self>) {
        let cancellation = self.cancellation.child_token();
        cx.spawn(async move |this, cx| {
            loop {
                let event = subscription.recv(cancellation.clone()).await;
                match event {
                    Ok(Some(event)) => {
                        if this
                            .update_in(cx, |renderer, window, cx| renderer.event(event, window, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) if cancellation.is_cancelled() => break,
                    Ok(None) | Err(_) => {
                        let _ = this.update(cx, |renderer, cx| {
                            renderer.status = OpenStatus::Failed("插件视图事件连接已中断".into());
                            cx.notify();
                        });
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// 旧进程的推送和其他实例的更新直接忽略，进程重启时不丢弃用户草稿。
    fn event(&mut self, event: RuntimeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            RuntimeEvent::ViewUpdate {
                identity,
                process_generation,
                patch,
            } if identity == self.identity => {
                if self.process_generation == Some(process_generation) {
                    if self.projection.apply_patch(&patch).is_err() {
                        self.needs_resync = true;
                    }
                    if self.needs_resync && !self.resync() {
                        self.status = OpenStatus::Failed("插件视图更新不完整".into());
                    } else if matches!(self.status, OpenStatus::Recovering) {
                        self.status = OpenStatus::Ready;
                        self.send_visibility(cx);
                    }
                    self.sync_controls(window, cx);
                    self.sync_collections(cx);
                } else if matches!(self.status, OpenStatus::Opening) {
                    if self.pending_updates.len() < MAX_PENDING_UPDATES {
                        self.pending_updates.push((process_generation, patch));
                    } else {
                        self.pending_updates.clear();
                        self.needs_resync = true;
                    }
                }
            }
            RuntimeEvent::ViewState { identity, state } if identity == self.identity => {
                self.saved_state = Some(state);
                if let Some(identity) = &self.content_identity {
                    window.dispatch_action(
                        Box::new(ContentStateChanged {
                            identity: identity.clone(),
                        }),
                        cx,
                    );
                }
            }
            RuntimeEvent::ProcessState { plugin, state } if plugin == self.identity.plugin => {
                match state {
                    PluginRuntimeState::Crashed { generation }
                        if self.process_generation == Some(generation) =>
                    {
                        self.status = OpenStatus::Recovering;
                        self.process_generation = None;
                        self.retired_generation = Some(generation);
                        self.operation_cancellation.cancel();
                        self.operation_cancellation = CancellationToken::new();
                        self.pending_updates.clear();
                        if let Some(next) = self.reload_sequence.checked_add(1) {
                            self.reload_sequence = next;
                            if self
                                .projection
                                .state_mut()
                                .advance_generation(next)
                                .is_err()
                            {
                                self.status = OpenStatus::Failed("插件视图恢复代次失效".into());
                            }
                        } else {
                            self.status = OpenStatus::Failed("插件视图恢复代次已耗尽".into());
                        }
                    }
                    // 进程就绪不代表对应视图已重新打开，必须等待实例级恢复回执。
                    PluginRuntimeState::Ready { .. }
                        if matches!(self.status, OpenStatus::Recovering) => {}
                    PluginRuntimeState::Failed => {
                        self.status = OpenStatus::Failed("插件进程不可用".into())
                    }
                    _ => {}
                }
            }
            RuntimeEvent::ResourcesReloaded { plugin, prepared }
                if plugin == self.identity.plugin =>
            {
                self.operation_cancellation.cancel();
                self.operation_cancellation = CancellationToken::new();
                self.process_generation = None;
                self.pending_updates.clear();
                self.reopened_generation = None;
                self.pending_reload = Some(prepared);
                self.reload_sequence = self.reload_sequence.saturating_add(1);
                if self
                    .projection
                    .state_mut()
                    .advance_generation(self.reload_sequence)
                    .is_err()
                {
                    self.status = OpenStatus::Failed("插件视图重载代次失效".into());
                } else {
                    self.finish_development_reload(window, cx);
                }
            }
            RuntimeEvent::ViewReopened {
                identity,
                process_generation,
            } if identity == self.identity
                && self
                    .retired_generation
                    .is_none_or(|retired| process_generation > retired)
                && self
                    .process_generation
                    .is_none_or(|current| process_generation >= current) =>
            {
                self.finish_reopened(process_generation, window, cx);
            }
            RuntimeEvent::ViewReopenFailed {
                identity,
                process_generation,
                message,
            } if identity == self.identity
                && self.process_generation.is_none()
                && self
                    .retired_generation
                    .is_none_or(|retired| process_generation > retired) =>
            {
                self.reopen_in_flight = false;
                self.status = OpenStatus::Failed(message);
            }
            RuntimeEvent::ResourcesReloadFailed { plugin, message }
                if plugin == self.identity.plugin =>
            {
                self.status = OpenStatus::Failed(message);
            }
            RuntimeEvent::ResyncRequired { .. } => {
                self.pending_updates.clear();
                self.needs_resync = true;
                if self.resync() {
                    self.sync_controls(window, cx);
                    self.sync_collections(cx);
                } else {
                    self.status = OpenStatus::Failed("插件视图事件丢失，无法恢复数据".into());
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// 非阻塞发送显隐通知；通知失败只显示错误，不虚构成功回执。
    fn send_visibility(&mut self, cx: &mut Context<Self>) {
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: self.process_generation,
            method: "view.visibility".into(),
            params: json!({"visible":self.visible,"focused":self.focused}),
            deadline: Instant::now() + OPEN_TIMEOUT,
            cancellation: self.cancellation.child_token(),
        };
        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request.cancellation.clone(), move |token| async move {
                    runtime
                        .notify(RuntimeRequest {
                            cancellation: token,
                            ..request
                        })
                        .await
                })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |renderer, cx| {
                    renderer.error = Some(error.public_message().to_owned());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 动作快照绑定当前实例和进程代次；等待秘密写入回执后才能提交引用。
    pub(super) fn action(
        &mut self,
        node: String,
        action: String,
        item_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.status, OpenStatus::Ready)
            || !self.field_errors.is_empty()
            || self.secret_write_pending()
        {
            return;
        }
        let form_snapshots = match self.visible_form_snapshots() {
            Ok(snapshots) => snapshots,
            Err(error) => {
                self.error = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        let Ok(ticket) = self.projection.state_mut().begin_action(&node) else {
            return;
        };
        let generation = self.process_generation;
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: generation,
            method: "view.action".into(),
            params: json!({"action":action,"item_key":item_key,"data":self.projection.state().data()}),
            deadline: Instant::now() + ACTION_TIMEOUT,
            cancellation: self.operation_cancellation.child_token(),
        };
        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request.cancellation.clone(), move |token| async move {
                    for (schema, data) in form_snapshots {
                        schema
                            .validate(&data)
                            .map_err(|_| AppError::invalid("插件表单不符合声明的 schema"))?;
                    }
                    runtime
                        .request(RuntimeRequest {
                            cancellation: token,
                            ..request
                        })
                        .await
                })
                .await;
            let _ = this.update_in(cx, |renderer, window, cx| {
                renderer.finish_action(ticket, generation, result, window, cx)
            });
        })
        .detach();
    }

    /// 只应用匹配原代次的动作结果；失败保留草稿并明确提示，不自动重试副作用。
    fn finish_action(
        &mut self,
        ticket: ActionTicket,
        generation: Option<u64>,
        result: AppResult<RuntimeReply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.process_generation != generation
            || self.projection.state_mut().finish_action(&ticket).is_err()
        {
            return;
        }
        match result {
            Ok(reply) if Some(reply.process_generation) == generation => {
                // 应用层已按进程消息顺序提交并广播补丁，不能再次重放较旧回执。
                self.sync_controls(window, cx);
                self.sync_collections(cx);
                if self.content_identity.is_some() {
                    self.request_snapshot(cx);
                }
            }
            Ok(_) => {}
            Err(error) => self.error = Some(error.public_message().to_owned()),
        }
        cx.notify();
    }

    /// 快照由异步应用服务缓存；同步工作区保存只读取确认过的结果。
    fn request_snapshot(&mut self, cx: &mut Context<Self>) {
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: self.process_generation,
            method: "view.snapshot".into(),
            params: json!({}),
            deadline: Instant::now() + OPEN_TIMEOUT,
            cancellation: self.cancellation.child_token(),
        };
        let runtime = Arc::clone(&self.runtime);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request.cancellation.clone(), move |token| async move {
                    runtime
                        .request(RuntimeRequest {
                            cancellation: token,
                            ..request
                        })
                        .await
                        .map(|_| ())
                })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |renderer, cx| {
                    renderer.error = Some(error.public_message().to_owned());
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Drop for NativeRenderer {
    /// 实体结束时协作取消订阅和请求；显式关闭回执仍由宿主生命周期负责。
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.operation_cancellation.cancel();
    }
}

impl Render for NativeRenderer {
    /// 局部错误和重试固定在视图内；原有控件继续保留草稿和布局。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = gpui_kit::component::locale().to_string();
        if self.locale != locale {
            self.locale = locale.clone();
            self.projection.set_locale(locale);
            self.refresh_locale(window, cx);
        }
        let root = self.projection.view().root.clone();
        let content = self.render_node(&root, "root".to_owned(), window, cx);
        let mut container = div().size_full().flex().flex_col().gap_2();
        if let OpenStatus::Failed(message) = &self.status {
            let retry = Button::new("external-view-retry")
                .label("重试")
                .ghost()
                .on_click(cx.listener(|renderer, _, window, cx| renderer.retry(window, cx)));
            container = container.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(message.clone())
                    .child(retry),
            );
        }
        if let Some(error) = &self.error {
            container = container.child(div().child(error.clone()));
        }
        container.child(content)
    }
}
