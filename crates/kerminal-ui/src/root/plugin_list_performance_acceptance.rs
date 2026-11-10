//! 真实原生列表验收只调整正式滚动句柄；数据仍经脚本运行服务分页返回。
//! @author kongweiguang

use super::*;
use crate::plugins::{actions::OpenContentTab, external::renderer::NativeRenderer};
use kerminal_application::{AppError, AppResult};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::plugins::{
    content::{
        ContentCloseFuture, ContentFactory, ContentInstance, ContentKind, ContentSavedState,
        ContentScope,
    },
    external::{
        renderer::NativeRendererConfig,
        runtime::{
            AsyncViewRuntime, RuntimeEventFuture, RuntimeFuture, RuntimeReply, RuntimeRequest,
            RuntimeSubscription,
        },
    },
};
use gpui_kit::{AnyView, App};
use kerminal_application::{
    executor::ApplicationTaskExecutor,
    plugin_ports::{PluginViewIdentity, PluginViewSavedState, PreparedPlugin},
};
use kerminal_domain::plugins::{PluginId as ExternalPluginId, PluginVersion};
use kerminal_plugin_protocol::{
    ManifestPolicy, ValidatedPackage,
    native::{NativeView, parse_native_view},
    parse_manifest,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const HOST_LIST_ID: &str = "acceptance.host-list";
const HOST_LIST_TOTAL: usize = 100_000;
const HOST_LIST_GENERATION: u64 = 1;
/// 正式 RPC 列表的内容、来源插件和视图是同一轮采样的固定身份，验收快照与报告都按它校验，
/// 不能让宿主 fixture 或其它插件列表以相同形状冒充。
const LIST_PERFORMANCE_CONTENT_ID: &str = "test.list-performance/records";
const LIST_PERFORMANCE_PLUGIN_ID: &str = "test.list-performance";
const LIST_PERFORMANCE_VIEW: &str = "records";

/// 来源绑定只依赖标签、作用域、注册表提供方和内容身份，不读取窗口或渲染实体，
/// 因此可以在无窗口单测中验证“活动标签即本轮外部内容标签”的判定本身。
fn list_source_binding(
    expected_tab: &TabId,
    active_tab: Option<&TabId>,
    active_scope: Option<&ContentScope>,
    provider: Option<&str>,
    content_id: &str,
) -> (bool, bool, Option<&'static str>) {
    let active_binding = active_tab == Some(expected_tab)
        && active_scope == Some(&ContentScope::Tab(expected_tab.clone()));
    let registry_external =
        provider == Some(LIST_PERFORMANCE_PLUGIN_ID) && content_id == LIST_PERFORMANCE_CONTENT_ID;
    let binding_error = if !active_binding {
        Some("活动内容标签不是本轮外部插件列表标签")
    } else if !registry_external {
        Some("内容提供方不是 test.list-performance 外部插件")
    } else {
        None
    };
    (active_binding, registry_external, binding_error)
}

impl RootView {
    /// 使用生产内容工厂和标签路由，不用验收renderer替身承载列表。
    pub fn plugin_list_performance_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<TabId> {
        self.open_plugin_tab(
            &OpenContentTab {
                content_id: LIST_PERFORMANCE_CONTENT_ID.into(),
                title: "Native list 100000".into(),
                state_version: 1,
                state: json!({}),
            },
            window,
            cx,
        )
    }

    /// 采样快照必须绑定打开时返回的外部内容标签：活动标签、内容作用域、内容提供方和
    /// renderer 身份同时一致才暴露列表；否则显式给出绑定错误，避免采到其它标签或宿主 fixture。
    pub fn plugin_list_performance_snapshot_for_tab(
        &self,
        expected_tab: &TabId,
        cx: &Context<Self>,
    ) -> Value {
        let active_tab = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        let descriptor = self
            .services
            .plugins
            .content_type(LIST_PERFORMANCE_CONTENT_ID);
        let provider = descriptor.map(|entry| entry.provider.as_str().to_owned());
        // 内容注册表证明该内容确实由正式外部插件贡献；renderer 的 external 标志证明
        // 活动内容实体来自外部插件工厂，两者缺一都不能当作同一条来源证据。
        let (active_binding, registry_external, binding_error) = list_source_binding(
            expected_tab,
            active_tab.as_ref(),
            self.active_content_scope.as_ref(),
            provider.as_deref(),
            descriptor
                .map(|entry| entry.id.as_str())
                .unwrap_or_default(),
        );
        let list = active_binding
            .then(|| {
                self.content_host
                    .view(&ContentScope::Tab(expected_tab.clone()))
                    .and_then(|view| view.downcast::<NativeRenderer>().ok())
                    .map(|view| {
                        view.read(cx)
                            .external_plugin_list_performance_snapshot(LIST_PERFORMANCE_VIEW, cx)
                    })
            })
            .flatten();
        let list_external = list
            .as_ref()
            .and_then(|entry| entry["external"].as_bool())
            .unwrap_or(false);
        // 先取出所有权字符串，避免来源字段借用与随后把 list 移入快照的所有权冲突。
        let list_plugin = list
            .as_ref()
            .and_then(|entry| entry["plugin"].as_str())
            .map(str::to_owned);
        let list_view = list
            .as_ref()
            .and_then(|entry| entry["view"].as_str())
            .map(str::to_owned);
        let list_runtime = list
            .as_ref()
            .and_then(|entry| entry["runtime"].as_str())
            .map(str::to_owned);
        json!({
            "workspaceReady": self.workspace_controller.is_some(),
            "settingsReady": self.settings_snapshot.is_some(),
            "bindingError": binding_error,
            "content": {
                "tabId": expected_tab.as_str(),
                "activeTabId": active_tab.map(|tab| tab.as_str().to_owned()),
                "activeBinding": active_binding,
                "id": LIST_PERFORMANCE_CONTENT_ID,
                "provider": provider,
                "plugin": list_plugin,
                "view": list_view,
                "runtime": list_runtime,
                "externalPlugin": registry_external && list_external,
            },
            "list": list,
        })
    }

    /// 只绑定活动内容，标签消失立即失败，不换用其他列表实例。
    fn plugin_list_performance_renderer(&self) -> AppResult<Entity<NativeRenderer>> {
        self.active_content_scope
            .as_ref()
            .and_then(|scope| self.content_host.view(scope))
            .and_then(|view| view.downcast::<NativeRenderer>().ok())
            .ok_or_else(|| AppError::not_found("原生列表验收实例"))
    }

    /// 采样读取实际render可见行，工作区尚未准备好时保留显式未就绪状态。
    pub fn plugin_list_performance_snapshot(&self, cx: &Context<Self>) -> Value {
        json!({"workspaceReady": self.workspace_controller.is_some(), "settingsReady":self.settings_snapshot.is_some(), "list":self.plugin_list_performance_renderer().ok().map(|view|view.read(cx).external_plugin_list_performance_snapshot("records",cx))})
    }

    /// 仅将索引交给正式虚拟列表滚动句柄，不直接请求或写入期望数据。
    pub fn plugin_list_performance_scroll(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.plugin_list_performance_renderer()?
            .update(cx, |view, cx| {
                view.external_plugin_list_performance_scroll("records", index, cx)
            })
    }

    /// 宿主贡献与外部插件身份分离，两组必须使用同一个冻结目录键。
    pub const HOST_LIST_PERFORMANCE_CONTENT_ID: &'static str = HOST_LIST_ID;

    /// 工厂使用组合根的执行器；遥测只记录真实请求，不把宿主数据源计作插件。
    pub fn host_list_performance_factory(
        executor: ApplicationTaskExecutor,
    ) -> AppResult<(Arc<dyn ContentFactory>, Arc<Mutex<Value>>)> {
        let manifest = json!({
            "schema": 1, "id": "fixture.host-list", "name": {"en":"Host list fixture"},
            "description": {"en":"In-process acceptance fixture"}, "version":"1.0.0",
            "api":"^1.0", "publisher":"fixture", "runtime":{"kind":"none"},
            "views":{"records":{"kind":"native","file":"views/records.json"}}
        });
        let manifest = parse_manifest(
            &serde_json::to_vec(&manifest).map_err(|_| AppError::invalid("宿主列表清单无效"))?,
            ManifestPolicy::ThirdParty,
        )
        .map_err(|_| AppError::invalid("宿主列表清单无效"))?;
        let view = parse_native_view(
            br#"{"schema":1,"root":{"id":"records","type":"list","key":"id","title":"name","source":"rpc"}}"#,
        )
        .map_err(|_| AppError::invalid("宿主列表视图无效"))?;
        let prepared = Arc::new(PreparedPlugin {
            id: ExternalPluginId::parse("fixture.host-list")
                .map_err(|_| AppError::invalid("宿主列表身份无效"))?,
            version: PluginVersion::parse("1.0.0")
                .map_err(|_| AppError::invalid("宿主列表版本无效"))?,
            directory: PathBuf::new(),
            package: Arc::new(ValidatedPackage {
                manifest,
                files: BTreeMap::new(),
                views: BTreeMap::new(),
                configuration_schemas: BTreeMap::new(),
            }),
            native_views: BTreeMap::new(),
            localizations: BTreeMap::new(),
            resources: BTreeMap::new(),
        });
        let telemetry = Arc::new(Mutex::new(json!({
            "fixtureId":"host-list-100000-v1", "source":"host-in-process",
            "externalPlugin":false, "pluginFactoryOrIpc":false,
            "phase":"new", "instance":null, "instanceEpoch":0, "visibility":null, "fetches":[]
        })));
        let factory = HostListFactory {
            prepared,
            view: Arc::new(view),
            executor,
            telemetry: Arc::clone(&telemetry),
        };
        Ok((Arc::new(factory), telemetry))
    }

    /// 只有实际活动的宿主验收页面才能作为共同负载，不能误采其他插件列表。
    pub fn host_list_performance_snapshot(&self, cx: &Context<Self>) -> Value {
        if self.active_content_scope.as_ref() != Some(&ContentScope::Page(HOST_LIST_ID.into())) {
            return json!({"workspaceReady": self.workspace_controller.is_some(), "settingsReady": self.settings_snapshot.is_some(), "list": null});
        }
        self.plugin_list_performance_snapshot(cx)
    }

    /// 页面经冻结声明和内容宿主打开；不能直接设置 ready 或冒充外部插件标签。
    pub fn host_list_performance_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.open_plugin_page(HOST_LIST_ID, window, cx)
    }
}

/// 创建占位与回滚共享同一遥测；挂载失败或并发创建都不能留下不可重试的“已创建”状态。
struct HostListCreation {
    telemetry: Arc<Mutex<Value>>,
}

impl HostListCreation {
    /// 全新状态与正常关闭后的状态可以占位；并发创建仍必须在挂载前拒绝。
    /// 返回本次占位分配的单调实例世代：重开时递增，旧 runtime 的回执因此在
    /// creating/created 窗口也无法命中新实例，而不只是比较各自 identity。
    fn reserve(&self, instance: &str) -> AppResult<u64> {
        let mut telemetry = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        let phase = telemetry["phase"].clone();
        let reopening = phase == "closed";
        if phase != "new" && !reopening {
            return Err(AppError::conflict("宿主列表实例已创建"));
        }
        // 世代只增不减：即使挂载失败回滚到 new，下一次占位也不会复用之前的世代，
        // 这样任何迟到的旧 runtime 请求都不会被误认为当前实例。
        let epoch = telemetry["instanceEpoch"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1);
        // 内容宿主在 finalize 后允许同一页面重新打开，但旧实例的分页和显隐证据
        // 不能带入新实例；占位回滚后的重试同样不能继承挂载失败前的迟到证据。
        if reopening || phase == "new" {
            telemetry["fetches"] = json!([]);
            telemetry["visibility"] = Value::Null;
        }
        telemetry["instance"] = json!(instance);
        telemetry["instanceEpoch"] = json!(epoch);
        telemetry["phase"] = json!("creating");
        Ok(epoch)
    }

    /// 挂载成功后推进占位；打开请求可能先于此处到达，因此已打开也视为确认。
    /// 世代必须与占位一致，防止旧创建流程在重开后误确认新实例。
    fn confirm(&self, instance: &str, epoch: u64) -> AppResult<()> {
        let mut telemetry = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        if telemetry["instance"] != instance || telemetry["instanceEpoch"].as_u64() != Some(epoch) {
            return Err(AppError::conflict("宿主列表创建状态已变化"));
        }
        match telemetry["phase"].as_str() {
            Some("creating") => telemetry["phase"] = json!("created"),
            Some("open") => {}
            _ => return Err(AppError::conflict("宿主列表创建状态已变化")),
        }
        Ok(())
    }

    /// 只回滚仍由本实例和本世代持有的占位；挂载失败后必须恢复到可重试的新状态。
    fn rollback(&self, instance: &str, epoch: u64) -> AppResult<()> {
        let mut telemetry = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        if telemetry["phase"] == "creating"
            && telemetry["instance"] == instance
            && telemetry["instanceEpoch"].as_u64() == Some(epoch)
        {
            telemetry["phase"] = json!("new");
            telemetry["instance"] = Value::Null;
        }
        Ok(())
    }
}

/// 每个验收窗口只注册一个工厂；资源和状态仍由内容宿主实例持有。
struct HostListFactory {
    prepared: Arc<PreparedPlugin>,
    view: Arc<NativeView>,
    executor: ApplicationTaskExecutor,
    telemetry: Arc<Mutex<Value>>,
}

impl ContentFactory for HostListFactory {
    /// 该身份只在受控验收目录中声明，不能覆盖正式插件贡献。
    fn content_id(&self) -> &str {
        HOST_LIST_ID
    }

    /// 验收页面不进入可持久化标签，避免污染隔离根的工作区恢复数据。
    fn supports(&self, kind: ContentKind) -> bool {
        kind == ContentKind::Page
    }

    /// 每次创建生成独立身份，并使用真实 renderer 和其正常异步分页状态机。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        if scope != &ContentScope::Page(HOST_LIST_ID.into()) || state.is_some() {
            return Err(AppError::invalid("宿主列表作用域无效"));
        }
        let identity = PluginViewIdentity {
            plugin: self.prepared.id.clone(),
            view: "records".into(),
            instance: uuid::Uuid::new_v4().to_string(),
            generation: 1,
        };
        let creation = HostListCreation {
            telemetry: Arc::clone(&self.telemetry),
        };
        let epoch = creation.reserve(&identity.instance)?;
        let runtime = Arc::new(HostListRuntime {
            identity: identity.clone(),
            epoch,
            telemetry: Arc::clone(&self.telemetry),
        });
        let renderer = match NativeRenderer::mount(
            NativeRendererConfig {
                prepared: Arc::clone(&self.prepared),
                view: Arc::clone(&self.view),
                identity: identity.clone(),
                scope: json!({"kind":"host-acceptance"}),
                saved_state: None,
                initial_data: None,
                runtime,
                executor: self.executor.clone(),
                locale: "en".into(),
                content_identity: None,
                embedded_form: None,
            },
            window,
            cx,
        ) {
            Ok(renderer) => renderer,
            Err(error) => {
                // 挂载失败先恢复占位；否则后续重试会被误判为实例已存在。
                if creation.rollback(&identity.instance, epoch).is_err() {
                    return Err(AppError::conflict("宿主列表创建失败且状态无法恢复"));
                }
                return Err(error);
            }
        };
        creation.confirm(&identity.instance, epoch)?;
        Ok(Box::new(HostListInstance { renderer }))
    }
}

struct HostListInstance {
    renderer: Entity<NativeRenderer>,
}

impl ContentInstance for HostListInstance {
    /// 保留真实实体，使分页缓存和虚拟滚动句柄跨重绘一致。
    fn view(&self) -> AnyView {
        self.renderer.clone().into()
    }

    /// 页面不持久化插件状态，但仍要求已确认的宿主实例可用。
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState> {
        let state = self
            .renderer
            .read(cx)
            .cached_state()?
            .ok_or_else(|| AppError::conflict("宿主列表尚未打开"))?;
        Ok(ContentSavedState {
            version: state.state_version,
            value: state.state,
        })
    }

    /// 显隐由内容宿主决定，而不是由采样脚本猜测。
    fn visibility_changed(&mut self, visible: bool, active: bool, cx: &mut App) -> AppResult<()> {
        self.renderer.update(cx, |renderer, cx| {
            renderer.visibility_changed(visible, visible && active, cx)
        });
        Ok(())
    }

    /// 等待 renderer 的真实关闭回执，失败时内容宿主继续持有实例。
    fn begin_close(&mut self, deadline: Instant, cx: &mut App) -> AppResult<ContentCloseFuture> {
        Ok(self.renderer.read(cx).close(deadline))
    }
}

/// 此适配器只实现宿主内存分页，绝不创建脚本进程或注册外部插件。
struct HostListRuntime {
    identity: PluginViewIdentity,
    /// 创建时分配的共享实例世代；与 identity 一起构成旧回执的防护。
    epoch: u64,
    telemetry: Arc<Mutex<Value>>,
}

impl HostListRuntime {
    /// 共享遥测可能已被重开的新实例接管。只比较自身 identity 时，旧 runtime 的
    /// open/close 会命中新实例的 creating/created/open 阶段；因此每个入口都必须
    /// 同时核对当前实例身份与单调世代，任一不符即视为过期实例。
    fn owns_state(identity: &PluginViewIdentity, epoch: u64, state: &Value) -> bool {
        state["instance"].as_str() == Some(identity.instance.as_str())
            && state["instanceEpoch"].as_u64() == Some(epoch)
    }

    /// 同步入口复用同一判定；异步入口捕获身份与世代后调用 `owns_state`。
    fn owns(&self, state: &Value) -> bool {
        Self::owns_state(&self.identity, self.epoch, state)
    }
}

struct HostListSubscription;
impl RuntimeSubscription for HostListSubscription {
    /// 数据源不主动推送；订阅保留至 renderer 协作取消，不能立即伪报断开。
    fn recv(&mut self, cancellation: CancellationToken) -> RuntimeEventFuture<'_> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Ok(None)
        })
    }
}

impl AsyncViewRuntime for HostListRuntime {
    /// 打开、分页和关闭严格核对身份、代次、期限和状态；仅生成请求的行。
    fn request(&self, request: RuntimeRequest) -> RuntimeFuture<RuntimeReply> {
        let identity = self.identity.clone();
        let epoch = self.epoch;
        let telemetry = Arc::clone(&self.telemetry);
        Box::pin(async move {
            if request.identity != identity {
                return Err(AppError::not_found("宿主列表实例"));
            }
            if request.cancellation.is_cancelled() {
                return Err(AppError::cancelled("宿主列表请求"));
            }
            if Instant::now() >= request.deadline {
                return Err(AppError::timeout("宿主列表请求"));
            }
            let mut state = telemetry
                .lock()
                .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
            // 重开后的新实例由同一遥测承载；旧 runtime 即使身份字段自洽也不能
            // 读写当前阶段，否则旧 open 可在 creating/created 窗口污染新实例。
            if !HostListRuntime::owns_state(&identity, epoch, &state) {
                return Err(AppError::not_found("宿主列表实例"));
            }
            let value = match request.method.as_str() {
                "view.open"
                    if request.process_generation.is_none()
                        && (state["phase"] == "creating" || state["phase"] == "created") =>
                {
                    state["phase"] = json!("open");
                    json!({"data":{}})
                }
                "view.fetch"
                    if request.process_generation == Some(HOST_LIST_GENERATION)
                        && state["phase"] == "open" =>
                {
                    let range = request.params["range"]
                        .as_array()
                        .filter(|range| range.len() == 2)
                        .ok_or_else(|| AppError::invalid("宿主列表分页范围无效"))?;
                    let start = range[0]
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or_else(|| AppError::invalid("宿主列表分页范围无效"))?;
                    let end = range[1]
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or_else(|| AppError::invalid("宿主列表分页范围无效"))?;
                    if request.params["node"] != "records"
                        || !request.params["parent_key"].is_null()
                        || !request.params["sort"].is_null()
                        || end <= start
                        || end - start > 500
                    {
                        return Err(AppError::invalid("宿主列表分页请求无效"));
                    }
                    let rows: Vec<Value> = (start..end.min(HOST_LIST_TOTAL)).map(|index| json!({"id":format!("record-{index}"),"name":format!("Record {index}")})).collect();
                    if request.cancellation.is_cancelled() {
                        return Err(AppError::cancelled("宿主列表分页"));
                    }
                    if Instant::now() >= request.deadline {
                        return Err(AppError::timeout("宿主列表分页"));
                    }
                    let fetches = state["fetches"]
                        .as_array_mut()
                        .ok_or_else(|| AppError::conflict("宿主列表遥测无效"))?;
                    if fetches.len() >= 50_000 {
                        return Err(AppError::conflict("宿主列表分页遥测超限"));
                    }
                    fetches.push(json!({"start":start,"end":end,"returned":rows.len()}));
                    json!({"total":HOST_LIST_TOTAL,"rows":rows})
                }
                "view.close"
                    if request.process_generation == Some(HOST_LIST_GENERATION)
                        && state["phase"] == "open" =>
                {
                    state["phase"] = json!("closed");
                    json!({"state_version":1,"state":{}})
                }
                _ => return Err(AppError::conflict("宿主列表请求或实例状态无效")),
            };
            Ok(RuntimeReply {
                process_generation: HOST_LIST_GENERATION,
                value,
            })
        })
    }

    /// 仅接受当前已打开实例的显隐通知，防止过期实例影响新实例。
    fn notify(&self, request: RuntimeRequest) -> RuntimeFuture<()> {
        let identity = self.identity.clone();
        let epoch = self.epoch;
        let telemetry = Arc::clone(&self.telemetry);
        Box::pin(async move {
            if request.identity != identity
                || request.method != "view.visibility"
                || request.process_generation != Some(HOST_LIST_GENERATION)
            {
                return Err(AppError::invalid("宿主列表通知无效"));
            }
            if request.cancellation.is_cancelled() {
                return Err(AppError::cancelled("宿主列表通知"));
            }
            if Instant::now() >= request.deadline {
                return Err(AppError::timeout("宿主列表通知"));
            }
            let mut state = telemetry
                .lock()
                .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
            // 旧实例的迟到显隐通知不能写入新实例；世代核对在阶段检查之前。
            if !HostListRuntime::owns_state(&identity, epoch, &state) {
                return Err(AppError::not_found("宿主列表实例"));
            }
            if state["phase"] != "open" {
                return Err(AppError::conflict("宿主列表未打开"));
            }
            state["visibility"] = request.params;
            Ok(())
        })
    }

    /// 订阅必须先验证完整身份和当前共享世代，且不会生成虚构的插件事件。
    fn subscribe(&self, identity: &PluginViewIdentity) -> AppResult<Box<dyn RuntimeSubscription>> {
        if identity != &self.identity {
            return Err(AppError::not_found("宿主列表实例"));
        }
        let state = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        if !self.owns(&state) || state["phase"] == "closed" || state["phase"] == "new" {
            return Err(AppError::not_found("宿主列表实例"));
        }
        Ok(Box::new(HostListSubscription))
    }

    /// 无草稿的固定快照仅在当前实例打开期间有效。
    fn cached_state(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewSavedState>> {
        if identity != &self.identity {
            return Err(AppError::not_found("宿主列表实例"));
        }
        let state = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        if !self.owns(&state) {
            return Err(AppError::not_found("宿主列表实例"));
        }
        Ok((state["phase"] == "open").then(|| PluginViewSavedState {
            state_version: 1,
            state: json!({}),
        }))
    }

    /// 空对象是真实宿主视图的数据快照，分页行由单独请求生成。
    fn cached_data(&self, identity: &PluginViewIdentity) -> AppResult<Option<Value>> {
        if identity != &self.identity {
            return Err(AppError::not_found("宿主列表实例"));
        }
        let state = self
            .telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?;
        if !self.owns(&state) {
            return Err(AppError::not_found("宿主列表实例"));
        }
        Ok((state["phase"] == "open").then(|| json!({})))
    }

    /// 列表不接受秘密写入，不能用占位成功掩盖意外调用。
    fn store_secret(
        &self,
        _: PluginViewIdentity,
        _: String,
        _: Zeroizing<String>,
        _: Instant,
        _: CancellationToken,
    ) -> RuntimeFuture<String> {
        Box::pin(async { Err(AppError::unsupported("宿主列表不支持秘密")) })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_list_performance_acceptance.rs"]
mod tests;
