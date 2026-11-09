//! 外部原生工具和标签的工厂；每个实例拥有独立身份与窗口内实体。
//!
//! @author kongweiguang

use super::{
    renderer::{NativeRenderer, NativeRendererConfig},
    runtime::AsyncViewRuntime,
};
use crate::{
    PluginUiBridge, PluginUiRegistration,
    plugins::{
        content::{
            ContentCloseFuture, ContentFactory, ContentInstance, ContentInstanceIdentity,
            ContentKind, ContentSavedState, ContentScope,
        },
        tools::{
            ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope,
        },
    },
};
use gpui_kit::{AnyView, App, Entity, Window, assets::IconName};
use kerminal_application::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    plugin_ports::{PluginViewIdentity, PluginViewSavedState, PreparedPlugin},
};
use kerminal_plugin_protocol::{
    TabContribution, ToolContribution, ToolScope as ProtocolToolScope, native::NativeView,
};
use serde_json::json;
use std::{sync::Arc, time::Instant};

/// 工厂只持有共享应用端口，视图和草稿由各自的窗口线程实例拥有。
#[derive(Clone)]
pub struct ExternalViewServices {
    pub runtime: Arc<dyn AsyncViewRuntime>,
    pub executor: ApplicationTaskExecutor,
    pub locale: String,
    pub ui_bridge: Option<Arc<PluginUiBridge>>,
}

impl ExternalViewServices {
    /// 桥接器可后续注入；未登记的实例不能调用需要窗口归属的宿主 UI。
    pub fn new(
        runtime: Arc<dyn AsyncViewRuntime>,
        executor: ApplicationTaskExecutor,
        locale: impl Into<String>,
    ) -> Self {
        Self {
            runtime,
            executor,
            locale: locale.into(),
            ui_bridge: None,
        }
    }

    /// 窗口根提供单一桥接器，避免插件视图意外获得其他窗口的目标。
    pub fn with_ui_bridge(mut self, bridge: Arc<PluginUiBridge>) -> Self {
        self.ui_bridge = Some(bridge);
        self
    }
}

/// 读取启动时冻结的 JSON；不得在 GPUI 窗口线程访问插件目录。
pub(crate) fn native_view(prepared: &PreparedPlugin, id: &str) -> AppResult<Arc<NativeView>> {
    let value = prepared
        .native_views
        .get(id)
        .ok_or_else(|| AppError::unsupported("插件贡献不是可用的原生视图"))?;
    let view: NativeView = serde_json::from_value((**value).clone())
        .map_err(|_| AppError::invalid("插件原生视图无效"))?;
    view.validate()
        .map_err(|_| AppError::invalid("插件原生视图无效"))?;
    Ok(Arc::new(view))
}

/// 运行实例采用随机身份；页面范围和进程代次不能复用作实例身份。
fn identity(prepared: &PreparedPlugin, view: &str) -> PluginViewIdentity {
    PluginViewIdentity {
        plugin: prepared.id.clone(),
        view: view.to_owned(),
        instance: uuid::Uuid::new_v4().to_string(),
        generation: 1,
    }
}

/// 注册句柄由实例持有，释放时自动撤销该身份的窗口请求路由。
fn register(
    services: &ExternalViewServices,
    identity: &PluginViewIdentity,
) -> AppResult<Option<PluginUiRegistration>> {
    services
        .ui_bridge
        .as_ref()
        .map(|bridge| bridge.register_instance(identity.clone()))
        .transpose()
}

/// 未匹配当前语言的键回退英文；普通标题不作为表达式执行。
fn title(prepared: &PreparedPlugin, locale: &str, text: &str) -> String {
    let Some(key) = text
        .strip_prefix('%')
        .and_then(|text| text.strip_suffix('%'))
    else {
        return text.to_owned();
    };
    prepared
        .localizations
        .get(locale)
        .and_then(|map| map.get(key))
        .or_else(|| {
            prepared
                .localizations
                .get("en")
                .and_then(|map| map.get(key))
        })
        .cloned()
        .unwrap_or_else(|| text.to_owned())
}

/// 工具清单与真实原生视图一同冻结，注册前即可拒绝缺失资源。
pub struct ExternalToolFactory {
    prepared: Arc<PreparedPlugin>,
    contribution: ToolContribution,
    services: ExternalViewServices,
    id: String,
}

impl ExternalToolFactory {
    /// 贡献必须来自同一清单，防止传入其他插件的视图标识。
    pub fn new(
        prepared: Arc<PreparedPlugin>,
        contribution: ToolContribution,
        services: ExternalViewServices,
    ) -> AppResult<Self> {
        if !prepared
            .package
            .manifest
            .contributes
            .tools
            .contains(&contribution)
        {
            return Err(AppError::invalid("插件工具贡献不属于清单"));
        }
        native_view(&prepared, &contribution.view)?;
        let id = format!("{}/{}", prepared.id.as_str(), contribution.id);
        Ok(Self {
            prepared,
            contribution,
            services,
            id,
        })
    }
}

impl ToolFactory for ExternalToolFactory {
    /// 全局限定身份与注册目录使用同一规则。
    fn tool_id(&self) -> &str {
        &self.id
    }
    /// 每次读取标题都检查当前语言，不依赖实例创建时的静态文字。
    fn presentation(&self) -> ToolPresentation {
        let locale = gpui_kit::component::locale();
        let label = title(&self.prepared, &locale, &self.contribution.title);
        ToolPresentation::new(label.clone(), label, IconName::PanelsTopLeft)
    }
    /// 标签工具不与窗口工具共享实例和草稿。
    fn scope(&self) -> ToolScope {
        match self.contribution.scope {
            ProtocolToolScope::Window => ToolScope::Window,
            ProtocolToolScope::Tab => ToolScope::Tab,
        }
    }
    /// 标签作用域需要真实活动标签，不能猜测其他会话。
    fn is_applicable(&self, context: &ToolContext) -> bool {
        self.scope() == ToolScope::Window || context.active_tab().is_some()
    }
    /// 注册窗口路由先于打开请求，插件首个宿主调用可定位到正确实例。
    fn create(
        &self,
        context: &ToolContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        if !self.is_applicable(context) {
            return Err(AppError::invalid("标签工具需要活动标签"));
        }
        let prepared = self
            .services
            .runtime
            .prepared_plugin(&self.prepared.id)?
            .unwrap_or_else(|| self.prepared.clone());
        let view = native_view(&prepared, &self.contribution.view)?;
        let identity = identity(&prepared, &self.contribution.view);
        let registration = register(&self.services, &identity)?;
        let scope = json!({"kind":"tool","tool":self.contribution.id,"tab":context.active_tab().map(|tab| tab.as_str()),"host":context.selected_host().map(|host| host.as_str())});
        let renderer = NativeRenderer::mount(
            NativeRendererConfig {
                prepared,
                view,
                identity,
                scope,
                saved_state: None,
                initial_data: None,
                runtime: self.services.runtime.clone(),
                executor: self.services.executor.clone(),
                locale: self.services.locale.clone(),
                content_identity: None,
                embedded_form: None,
            },
            window,
            cx,
        )?;
        Ok(Box::new(ExternalToolInstance {
            renderer,
            _registration: registration,
        }))
    }
}

struct ExternalToolInstance {
    renderer: Entity<NativeRenderer>,
    _registration: Option<PluginUiRegistration>,
}
impl ToolInstance for ExternalToolInstance {
    /// 每帧复用实体，侧栏切换不重建输入和分页缓存。
    fn view(&self) -> AnyView {
        self.renderer.clone().into()
    }
    /// 宿主提供的显隐变化只通知该工具实例。
    fn visibility_changed(&mut self, visible: bool, active: bool, cx: &mut App) -> AppResult<()> {
        self.renderer.update(cx, |renderer, cx| {
            renderer.visibility_changed(visible, active && visible, cx)
        });
        Ok(())
    }
    /// 关闭使用宿主提供的同一个绝对截止时间。
    fn begin_close(&mut self, deadline: Instant, cx: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(self.renderer.read(cx).close(deadline))
    }
}

/// 标签工厂校验声明的状态版本，再为每个标签恢复独立状态。
pub struct ExternalContentFactory {
    prepared: Arc<PreparedPlugin>,
    contribution: TabContribution,
    services: ExternalViewServices,
    id: String,
}
impl ExternalContentFactory {
    /// 拒绝其他插件贡献或无法解析的静态视图，不注册半成品工厂。
    pub fn new(
        prepared: Arc<PreparedPlugin>,
        contribution: TabContribution,
        services: ExternalViewServices,
    ) -> AppResult<Self> {
        if !prepared
            .package
            .manifest
            .contributes
            .tabs
            .contains(&contribution)
        {
            return Err(AppError::invalid("插件标签贡献不属于清单"));
        }
        native_view(&prepared, &contribution.view)?;
        let id = format!("{}/{}", prepared.id.as_str(), contribution.id);
        Ok(Self {
            prepared,
            contribution,
            services,
            id,
        })
    }

    /// 原生宿主独占零版本草稿信封，内部插件版本仍须匹配声明；未知材料留给恢复界面。
    fn mount(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        content_identity: Option<ContentInstanceIdentity>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        let ContentScope::Tab(tab) = scope else {
            return Err(AppError::invalid("插件标签需要标签作用域"));
        };
        if let Some(state) = state {
            if state.version == 0 {
                super::renderer::validate_native_content_state(
                    &state.value,
                    &self.contribution.view,
                    self.contribution.state_version,
                )?;
            } else if state.version != self.contribution.state_version {
                return Err(AppError::unsupported("插件标签状态版本不受支持"));
            }
        }
        let prepared = self
            .services
            .runtime
            .prepared_plugin(&self.prepared.id)?
            .unwrap_or_else(|| self.prepared.clone());
        let view = native_view(&prepared, &self.contribution.view)?;
        let identity = identity(&prepared, &self.contribution.view);
        let registration = register(&self.services, &identity)?;
        let saved_state = state.map(|state| PluginViewSavedState {
            state_version: state.version,
            state: state.value.clone(),
        });
        let renderer = NativeRenderer::mount(
            NativeRendererConfig {
                prepared,
                view,
                identity,
                scope: json!({"kind":"tab","tab":tab.as_str()}),
                saved_state,
                initial_data: None,
                runtime: self.services.runtime.clone(),
                executor: self.services.executor.clone(),
                locale: self.services.locale.clone(),
                content_identity,
                embedded_form: None,
            },
            window,
            cx,
        )?;
        Ok(Box::new(ExternalContentInstance {
            renderer,
            _registration: registration,
        }))
    }
}
impl ContentFactory for ExternalContentFactory {
    /// 保存与恢复均使用清单中稳定的全局贡献身份。
    fn content_id(&self) -> &str {
        &self.id
    }
    /// 此贡献只用于可持久化标签，不接管中央页面。
    fn supports(&self, kind: ContentKind) -> bool {
        kind == ContentKind::Tab
    }
    /// 零版本只保留给原生宿主信封，挂载时继续校验信封格式及内部插件版本。
    fn supports_state_version(&self, version: u32) -> bool {
        version == 0 || version == self.contribution.state_version
    }
    /// 兼容没有变化通知需求的直接调用方，仍保持独立实例。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.mount(scope, state, None, window, cx)
    }
    /// 带宿主代次的身份用于状态变化通知，旧实例无法更新新标签。
    fn create_with_identity(
        &self,
        identity: &ContentInstanceIdentity,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.mount(identity.scope(), state, Some(identity.clone()), window, cx)
    }
}
struct ExternalContentInstance {
    renderer: Entity<NativeRenderer>,
    _registration: Option<PluginUiRegistration>,
}
impl ContentInstance for ExternalContentInstance {
    /// 返回同一实体使草稿、焦点和事件订阅跨渲染保留。
    fn view(&self) -> AnyView {
        self.renderer.clone().into()
    }
    /// 插件状态与本地有效草稿分开保存；快照只读内存，不能为每次按键等待插件进程。
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState> {
        let state = self
            .renderer
            .read(cx)
            .content_saved_state()?
            .ok_or_else(|| AppError::conflict("插件标签尚未提供状态快照"))?;
        Ok(ContentSavedState {
            version: state.state_version,
            value: state.state,
        })
    }
    /// 可见性来源于宿主导航与窗口状态，不从当前聚焦控件推断。
    fn visibility_changed(&mut self, visible: bool, active: bool, cx: &mut App) -> AppResult<()> {
        self.renderer.update(cx, |renderer, cx| {
            renderer.visibility_changed(visible, visible && active, cx)
        });
        Ok(())
    }
    /// 宿主保留实例直到关闭任务回执，避免超时被当作资源已回收。
    fn begin_close(&mut self, deadline: Instant, cx: &mut App) -> AppResult<ContentCloseFuture> {
        Ok(self.renderer.read(cx).close(deadline))
    }
}
