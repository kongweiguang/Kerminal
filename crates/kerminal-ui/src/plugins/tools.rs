//! 工具贡献的界面描述、工厂与窗口内实例所有权。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Instant};

use gpui_kit::{AnyView, App, EntityId, SharedString, Window, assets::IconName};
use kerminal_application::{
    AppError, AppResult, plugins::PluginRegistry, preferences::ToolRailToolId,
    sessions::UiSessionBinding,
};
use kerminal_domain::identity::{HostId, TabId};

#[path = "builtin_tools.rs"]
mod builtin_tools;

/// 内置工具顺序只用于缺省目录；用户设置仍拥有显示顺序及分区。
pub(crate) const BUILTIN_TOOLS: [ToolRailToolId; 8] = [
    ToolRailToolId::AgentLauncher,
    ToolRailToolId::Sftp,
    ToolRailToolId::Snippets,
    ToolRailToolId::Tmux,
    ToolRailToolId::Ports,
    ToolRailToolId::System,
    ToolRailToolId::Logs,
    ToolRailToolId::Containers,
];

/// 工具显示名称和图标与稳定身份分离，语言变化不影响配置引用。
#[derive(Clone)]
pub struct ToolPresentation {
    label: SharedString,
    description: SharedString,
    icon: IconName,
}

impl ToolPresentation {
    /// 由贡献者提供可访问名称与简短说明；宿主使用 Kit 图标保证主题一致。
    pub fn new(
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
        icon: IconName,
    ) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            icon,
        }
    }

    /// 每个调用者取得同一名称，避免右栏和自定义器使用不同身份。
    pub fn label(&self) -> SharedString {
        self.label.clone()
    }

    /// 说明只用于用户选择工具，不参与身份比较。
    pub fn description(&self) -> SharedString {
        self.description.clone()
    }

    /// 图标由已安装的 Kit 资源提供，不接受任意外部路径。
    pub fn icon(&self) -> IconName {
        self.icon
    }
}

/// 每个贡献声明自己的实例归属，防止不同标签意外共享草稿。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolScope {
    Window,
    Tab,
}

/// 适用性只消费宿主已经确认的活动身份，不允许工厂猜测其它标签。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolContext {
    active_tab: Option<TabId>,
    selected_host: Option<HostId>,
    active_terminal: Option<UiSessionBinding>,
}

impl ToolContext {
    /// 显式传入当前身份；无活动标签时按窗口作用域处理或隐藏标签工具。
    pub fn new(active_tab: Option<TabId>, selected_host: Option<HostId>) -> Self {
        Self {
            active_tab,
            selected_host,
            active_terminal: None,
        }
    }

    /// 工厂只能读取活动标签身份，不能借此修改工作区。
    pub fn active_tab(&self) -> Option<&TabId> {
        self.active_tab.as_ref()
    }

    /// 已选择主机只是适用条件，不代表已建立会话。
    pub fn selected_host(&self) -> Option<&HostId> {
        self.selected_host.as_ref()
    }

    /// 终端绑定由宿主验证后显式提供，与目录选中主机相互独立。
    ///
    /// @author kongweiguang
    pub fn with_active_terminal(mut self, binding: Option<UiSessionBinding>) -> Self {
        self.active_terminal = binding;
        self
    }

    /// 获取已验证的当前终端身份；没有活动 pane 时不得推断其他目标。
    ///
    /// @author kongweiguang
    pub fn active_terminal(&self) -> Option<&UiSessionBinding> {
        self.active_terminal.as_ref()
    }
}

/// 回收任务由共享执行器驱动，UI 线程只保留实例和截止时间。
pub type ToolCloseFuture = Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'static>>;

/// 工厂只捕获必要应用端口；其 UI 实例始终在所属窗口线程创建。
pub trait ToolFactory: Send + Sync + 'static {
    /// 身份必须对应已冻结的 application 贡献声明。
    fn tool_id(&self) -> &str;

    /// 每次读取都允许根据当前语言生成名称。
    fn presentation(&self) -> ToolPresentation;

    /// 作用域在注册后稳定，实例不能在窗口与标签之间迁移。
    fn scope(&self) -> ToolScope;

    /// 工具可按活动工作区决定是否可用；不适用时保留用户配置。
    fn is_applicable(&self, _context: &ToolContext) -> bool {
        true
    }

    /// 创建失败由调用方展示错误，不能生成没有实际内容的成功实例。
    fn create(
        &self,
        context: &ToolContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>>;
}

/// 实例持有保留态视图；视图移动宿主时不能重新创建或丢弃草稿。
pub trait ToolInstance {
    /// 每帧只借用同一视图句柄，不能在此启动后台任务。
    fn view(&self) -> AnyView;

    /// 上下文仅在宿主身份实际变化时通知，已提交的任务仍须绑定原有目标。
    ///
    /// @author kongweiguang
    fn context_changed(&mut self, _context: &ToolContext, _cx: &mut App) -> AppResult<()> {
        Ok(())
    }

    /// 可见和窗口激活由宿主计算，并只在状态改变时通知实例。
    fn visibility_changed(
        &mut self,
        _visible: bool,
        _window_active: bool,
        _cx: &mut App,
    ) -> AppResult<()> {
        Ok(())
    }

    /// 只有显式导航才请求焦点，普通重绘不得抢占用户输入。
    fn focus(&mut self, _window: &mut Window, _cx: &mut App) -> AppResult<()> {
        Ok(())
    }

    /// 插件负责在同一绝对截止时间内回收资源，宿主收到回执后才移除实例。
    fn begin_close(&mut self, deadline: Instant, cx: &mut App) -> AppResult<ToolCloseFuture>;
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum InstanceScope {
    Window,
    NoTab,
    Tab(TabId),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InstanceKey {
    tool: ToolRailToolId,
    scope: InstanceScope,
}

/// 关闭回执绑定实例和代次，过期回执不能移除重试中的工具。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCloseToken {
    key: InstanceKey,
    generation: u64,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPhase {
    Open,
    Closing,
    CloseFailed,
    Closed,
}

struct HostedTool {
    instance: Box<dyn ToolInstance>,
    context: ToolContext,
    visibility: Option<(bool, bool)>,
    phase: ToolPhase,
    closing_generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolProjection {
    pub(crate) context: ToolContext,
    pub(crate) visible: Vec<ToolRailToolId>,
    pub(crate) window_active: bool,
}

/// 工厂可跨线程共享；实例与生命周期状态只保留在该窗口的 UI 线程。
#[derive(Default)]
pub struct PluginTools {
    factories: BTreeMap<ToolRailToolId, Arc<dyn ToolFactory>>,
    instances: BTreeMap<InstanceKey, HostedTool>,
    next_generation: u64,
    projection: Option<ToolProjection>,
    projection_generation: u64,
    builtin_views: BTreeMap<ToolRailToolId, Option<EntityId>>,
}

impl PluginTools {
    /// 显式重新打开后允许相同布局重新应用一次，取消旧 effect 与采样票据。
    /// 普通绘制不清除此门禁，避免失败工厂随每一帧自动重试。
    pub(crate) fn invalidate_projection(&mut self) {
        self.projection = None;
        self.projection_generation = self.projection_generation.wrapping_add(1).max(1);
    }

    /// 绘制阶段仅记录变化后的只读投影，实际创建和通知留给 effect 周期。
    ///
    /// @author kongweiguang
    pub(crate) fn stage_projection(&mut self, projection: ToolProjection) -> bool {
        if self.projection.as_ref() == Some(&projection) {
            return false;
        }
        self.projection = Some(projection);
        self.projection_generation = self.projection_generation.wrapping_add(1).max(1);
        true
    }

    /// 迟到的 effect 不得把工具重定向到已经切换的窗口或标签。
    ///
    /// @author kongweiguang
    pub(crate) fn projection_is_current(&self, projection: &ToolProjection) -> bool {
        self.projection.as_ref() == Some(projection)
    }

    /// 后台订阅只在最近一次真实挂载投影处于活动窗口时抓取画布。
    ///
    /// @author kongweiguang
    pub(crate) fn projected_tool_active(&self, tool: ToolRailToolId) -> bool {
        self.projection.as_ref().is_some_and(|projection| {
            projection.window_active && projection.visible.contains(&tool)
        })
    }

    /// 焦点只交给最近一帧实际挂载的工具；窗口激活由调用方独立校验。
    ///
    /// @author kongweiguang
    pub(crate) fn projected_tool_visible(&self, tool: ToolRailToolId) -> bool {
        self.projection
            .as_ref()
            .is_some_and(|projection| projection.visible.contains(&tool))
    }

    /// 异步画布请求绑定实际挂载代次，隐藏后重新显示也不能接受之前的回执。
    ///
    /// @author kongweiguang
    pub(crate) fn active_projection_generation(&self, tool: ToolRailToolId) -> Option<u64> {
        self.projected_tool_active(tool)
            .then_some(self.projection_generation)
    }

    /// 工厂必须引用已声明的工具；内置和扩展身份都不得覆盖已有工厂。
    ///
    /// @author kongweiguang
    pub fn register(
        &mut self,
        registry: &PluginRegistry,
        factory: Arc<dyn ToolFactory>,
    ) -> AppResult<()> {
        let id = ToolRailToolId::from_id(factory.tool_id())
            .map_err(|_| AppError::invalid("工具贡献身份无效"))?;
        if registry.tool(id.as_str()).is_none() {
            return Err(AppError::invalid("工具工厂没有对应的插件贡献声明"));
        }
        if self.factories.contains_key(&id) {
            return Err(AppError::conflict("工具工厂身份重复"));
        }
        self.factories.insert(id, factory);
        Ok(())
    }

    /// 内置工厂只接收原所有者提供的视图句柄；替换标签不会重建业务实体。
    ///
    /// @author kongweiguang
    pub(crate) fn set_builtin_view(
        &mut self,
        tool: ToolRailToolId,
        scope: ToolScope,
        view: Option<AnyView>,
        context: &ToolContext,
    ) -> AppResult<bool> {
        if !BUILTIN_TOOLS.contains(&tool) {
            return Err(AppError::invalid("不是内置工具身份"));
        }
        if self.factories.contains_key(&tool) && !self.builtin_views.contains_key(&tool) {
            return Err(AppError::conflict("工具工厂身份重复"));
        }
        let key = InstanceKey {
            tool,
            scope: match scope {
                ToolScope::Window => InstanceScope::Window,
                ToolScope::Tab => context
                    .active_tab()
                    .cloned()
                    .map_or(InstanceScope::NoTab, InstanceScope::Tab),
            },
        };
        let view_id = view.as_ref().map(AnyView::entity_id);
        let replaced = self
            .instances
            .get(&key)
            .is_some_and(|entry| Some(entry.instance.view().entity_id()) != view_id);
        if replaced
            && self
                .instances
                .get(&key)
                .is_some_and(|entry| entry.phase != ToolPhase::Open)
        {
            return Err(AppError::conflict("工具实例正在关闭"));
        }
        if self.builtin_views.get(&tool) == Some(&view_id) && !replaced {
            return Ok(false);
        }
        let presentation =
            builtin_presentation(tool).ok_or_else(|| AppError::invalid("内置工具缺少显示声明"))?;
        self.factories.insert(
            tool,
            Arc::new(builtin_tools::BuiltinToolFactory {
                tool,
                scope,
                presentation,
                view: view.as_ref().map(AnyView::downgrade),
            }),
        );
        self.builtin_views.insert(tool, view_id);
        if replaced {
            self.instances.remove(&key);
        }
        Ok(true)
    }

    /// 只返回真正有实现的贡献，尚未安装的合法身份继续留在设置中。
    pub fn registered_tools(&self) -> Vec<ToolRailToolId> {
        BUILTIN_TOOLS
            .iter()
            .copied()
            .chain(
                self.factories
                    .keys()
                    .filter(|tool| !BUILTIN_TOOLS.contains(tool))
                    .copied(),
            )
            .collect()
    }

    /// 内置和扩展工具通过同一入口读取图标及名称，退役身份不出现在目录。
    pub fn presentation(&self, tool: ToolRailToolId) -> Option<ToolPresentation> {
        builtin_presentation(tool).or_else(|| {
            self.factories
                .get(&tool)
                .map(|factory| factory.presentation())
        })
    }

    /// 标签作用域需要真实活动标签；窗口作用域也必须满足提供者的业务条件。
    pub fn is_applicable(&self, tool: ToolRailToolId, context: &ToolContext) -> bool {
        if builtin_presentation(tool).is_some() {
            return true;
        }
        self.factories.get(&tool).is_some_and(|factory| {
            (factory.scope() == ToolScope::Window || context.active_tab().is_some())
                && factory.is_applicable(context)
        })
    }

    /// 首次显式打开时创建实例；重复打开只复用原实体。
    pub fn ensure(
        &mut self,
        tool: ToolRailToolId,
        context: &ToolContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<()> {
        let key = self.instance_key(tool, context)?;
        if let Some(entry) = self.instances.get(&key) {
            return if entry.phase == ToolPhase::Open {
                Ok(())
            } else {
                Err(AppError::conflict("工具实例正在关闭"))
            };
        }
        let factory = self
            .factories
            .get(&tool)
            .cloned()
            .ok_or_else(|| AppError::not_found("工具工厂不存在"))?;
        let instance = factory.create(context, window, cx)?;
        self.instances.insert(
            key,
            HostedTool {
                instance,
                context: context.clone(),
                visibility: None,
                phase: ToolPhase::Open,
                closing_generation: None,
            },
        );
        Ok(())
    }

    /// 只读取已确认上下文的实例；上下文回调失败时不得显示旧目标的视图。
    ///
    /// @author kongweiguang
    pub fn view(&self, tool: ToolRailToolId, context: &ToolContext) -> Option<AnyView> {
        let key = self.instance_key(tool, context).ok()?;
        let entry = self.instances.get(&key)?;
        (entry.phase == ToolPhase::Open && entry.context == *context).then(|| entry.instance.view())
    }

    /// 挂载前还需确认显隐通知成功，失败时保留实例供显式重试而不暴露旧状态。
    ///
    /// @author kongweiguang
    pub fn view_for_mount(
        &self,
        tool: ToolRailToolId,
        context: &ToolContext,
        window_active: bool,
    ) -> Option<AnyView> {
        let key = self.instance_key(tool, context).ok()?;
        let entry = self.instances.get(&key)?;
        let view = entry.instance.view();
        (entry.phase == ToolPhase::Open
            && entry.context == *context
            && entry.visibility == Some((true, window_active))
            && self
                .builtin_views
                .get(&tool)
                .is_none_or(|current| *current == Some(view.entity_id())))
        .then_some(view)
    }

    /// 只在可见状态变化后调用实例，失败时保留旧状态以便下次重试。
    pub fn visibility_changed(
        &mut self,
        tool: ToolRailToolId,
        context: &ToolContext,
        visible: bool,
        window_active: bool,
        cx: &mut App,
    ) -> AppResult<()> {
        let key = self.instance_key(tool, context)?;
        let entry = self
            .instances
            .get_mut(&key)
            .ok_or_else(|| AppError::not_found("工具实例不存在"))?;
        if entry.phase != ToolPhase::Open {
            return Err(AppError::conflict("工具实例正在关闭"));
        }
        if entry.visibility != Some((visible, window_active)) {
            entry
                .instance
                .visibility_changed(visible, window_active, cx)?;
            entry.visibility = Some((visible, window_active));
        }
        Ok(())
    }

    /// 当前宿主变化时同步实例上下文和可见性；后台标签保留自己的上下文，失败时隐藏并保留旧状态供重试。
    ///
    /// @author kongweiguang
    pub fn sync_visibility(
        &mut self,
        context: &ToolContext,
        visible_tools: &[ToolRailToolId],
        window_active: bool,
        cx: &mut App,
    ) -> AppResult<()> {
        let active_keys = visible_tools
            .iter()
            .filter_map(|tool| self.instance_key(*tool, context).ok())
            .collect::<Vec<_>>();
        let mut first_error = None;
        for (key, entry) in &mut self.instances {
            if entry.phase != ToolPhase::Open {
                continue;
            }
            let matches_context = match &key.scope {
                InstanceScope::Window => true,
                InstanceScope::NoTab => context.active_tab().is_none(),
                InstanceScope::Tab(tab) => context.active_tab() == Some(tab),
            };
            let mut context_ready = true;
            if matches_context && entry.context != *context {
                match entry.instance.context_changed(context, cx) {
                    Ok(()) => entry.context = context.clone(),
                    Err(error) => {
                        context_ready = false;
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
            let next = (active_keys.contains(key) && context_ready, window_active);
            if entry.visibility == Some(next) {
                continue;
            }
            match entry.instance.visibility_changed(next.0, next.1, cx) {
                Ok(()) => entry.visibility = Some(next),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// 标签关闭流程可以逐一等待其工具回收，不把其他标签或窗口级实例算进来。
    pub fn tools_for_tab(&self, tab_id: &TabId) -> Vec<ToolRailToolId> {
        self.instances
            .keys()
            .filter_map(|key| {
                matches!(&key.scope, InstanceScope::Tab(tab) if tab == tab_id).then_some(key.tool)
            })
            .collect()
    }

    /// 关闭和重试必须区分等待回执、失败与已关闭，不能把不可用视图当成资源已回收。
    ///
    /// @author kongweiguang
    pub fn phase(&self, tool: ToolRailToolId, context: &ToolContext) -> Option<ToolPhase> {
        let key = self.instance_key_unchecked(tool, context).ok()?;
        self.instances.get(&key).map(|entry| entry.phase)
    }

    /// 退出时枚举所有实例的原作用域，包括隐藏、失败和已关闭但尚未确认的实例。
    ///
    /// @author kongweiguang
    pub fn all_instance_contexts(&self) -> Vec<(ToolRailToolId, ToolContext)> {
        self.instances
            .iter()
            .map(|(key, entry)| (key.tool, entry.context.clone()))
            .collect()
    }

    /// 只向精确作用域交接焦点，找不到时不得回退到其它标签。
    pub fn focus(
        &mut self,
        tool: ToolRailToolId,
        context: &ToolContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<()> {
        let key = self.instance_key(tool, context)?;
        let entry = self
            .instances
            .get_mut(&key)
            .ok_or_else(|| AppError::not_found("工具实例不存在"))?;
        if entry.phase != ToolPhase::Open {
            return Err(AppError::conflict("工具实例正在关闭"));
        }
        entry.instance.focus(window, cx)
    }

    /// 关闭失败时继续持有实例，重试也使用新的代次。
    pub fn begin_close(
        &mut self,
        tool: ToolRailToolId,
        context: &ToolContext,
        deadline: Instant,
        cx: &mut App,
    ) -> AppResult<(ToolCloseToken, ToolCloseFuture)> {
        let key = self.instance_key_unchecked(tool, context)?;
        let entry = self
            .instances
            .get_mut(&key)
            .ok_or_else(|| AppError::not_found("工具实例不存在"))?;
        if matches!(entry.phase, ToolPhase::Closing | ToolPhase::Closed) {
            return Err(AppError::conflict("工具实例正在关闭或已关闭"));
        }
        if deadline <= Instant::now() {
            return Err(AppError::Timeout {
                operation: "关闭工具实例",
            });
        }
        let generation = self
            .next_generation
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("工具关闭代次已耗尽"))?;
        let future = match entry.instance.begin_close(deadline, cx) {
            Ok(future) => future,
            Err(error) => {
                entry.phase = ToolPhase::CloseFailed;
                return Err(error);
            }
        };
        self.next_generation = generation;
        entry.phase = ToolPhase::Closing;
        entry.closing_generation = Some(generation);
        Ok((
            ToolCloseToken {
                key,
                generation,
                deadline,
            },
            future,
        ))
    }

    /// 只接受对应代次且在截止时间内完成的回执，失败不释放资源 owner。
    pub fn complete_close(
        &mut self,
        token: ToolCloseToken,
        result: AppResult<()>,
    ) -> AppResult<()> {
        let entry = self
            .instances
            .get_mut(&token.key)
            .ok_or_else(|| AppError::not_found("工具实例不存在"))?;
        if entry.phase != ToolPhase::Closing || entry.closing_generation != Some(token.generation) {
            return Err(AppError::conflict("工具关闭回执已过期"));
        }
        entry.closing_generation = None;
        if result.is_ok() && Instant::now() > token.deadline {
            entry.phase = ToolPhase::CloseFailed;
            return Err(AppError::Timeout {
                operation: "关闭工具实例",
            });
        }
        match result {
            Ok(()) => {
                entry.phase = ToolPhase::Closed;
                Ok(())
            }
            Err(error) => {
                entry.phase = ToolPhase::CloseFailed;
                Err(error)
            }
        }
    }

    /// 与关闭确认分开提交，外部工作区未提交前不会丢失实例所有权。
    pub fn finalize_close(&mut self, tool: ToolRailToolId, context: &ToolContext) -> AppResult<()> {
        let key = self.instance_key_unchecked(tool, context)?;
        match self.instances.get(&key).map(|entry| entry.phase) {
            Some(ToolPhase::Closed) => {
                self.instances.remove(&key);
                Ok(())
            }
            Some(_) => Err(AppError::conflict("工具实例尚未完成关闭")),
            None => Err(AppError::not_found("工具实例不存在")),
        }
    }

    /// 创建与读取遵守相同适用门禁，避免后台标签接管当前工具。
    fn instance_key(&self, tool: ToolRailToolId, context: &ToolContext) -> AppResult<InstanceKey> {
        if !self.is_applicable(tool, context) {
            return Err(AppError::invalid("工具不适用于当前工作区"));
        }
        self.instance_key_unchecked(tool, context)
    }

    /// 关闭仍需访问已不再适用的实例，因此不重复检查当前业务条件。
    fn instance_key_unchecked(
        &self,
        tool: ToolRailToolId,
        context: &ToolContext,
    ) -> AppResult<InstanceKey> {
        let factory = self
            .factories
            .get(&tool)
            .ok_or_else(|| AppError::not_found("工具工厂不存在"))?;
        let scope = match factory.scope() {
            ToolScope::Window => InstanceScope::Window,
            ToolScope::Tab => match context.active_tab().cloned() {
                Some(tab) => InstanceScope::Tab(tab),
                None if self.builtin_views.contains_key(&tool) => InstanceScope::NoTab,
                None => return Err(AppError::invalid("标签工具需要活动标签")),
            },
        };
        Ok(InstanceKey { tool, scope })
    }
}

/// 内置工具的唯一显示目录；呈现层、工具按钮与自定义器共用这些语义。
pub(crate) fn builtin_presentation(tool: ToolRailToolId) -> Option<ToolPresentation> {
    let (label, description, icon) = match tool {
        ToolRailToolId::AgentLauncher => (
            crate::locale::text("Agent 启动器", "Agent launcher"),
            crate::locale::text(
                "Codex、Claude、自定义 Agent",
                "Codex, Claude, and custom agents",
            ),
            IconName::Bot,
        ),
        ToolRailToolId::Sftp => (
            crate::locale::text("文件", "Files"),
            crate::locale::text(
                "SSH/SFTP 与容器文件浏览",
                "SSH/SFTP and container file browsing",
            ),
            IconName::FolderOpen,
        ),
        ToolRailToolId::Snippets => (
            crate::locale::text("命令片段", "Command snippets"),
            crate::locale::text("可复用脚本索引", "Reusable script index"),
            IconName::FileText,
        ),
        ToolRailToolId::Tmux => (
            crate::locale::text("tmux 会话", "tmux sessions"),
            crate::locale::text(
                "session、window 和 pane 管理",
                "Session, window, and pane management",
            ),
            IconName::PanelsTopLeft,
        ),
        ToolRailToolId::Ports => (
            crate::locale::text("端口转发", "Port forwards"),
            crate::locale::text("SSH 端口转发", "SSH port forwarding"),
            IconName::Network,
        ),
        ToolRailToolId::System => (
            crate::locale::text("服务器信息", "Server information"),
            crate::locale::text(
                "CPU、内存、网络和磁盘状态",
                "CPU, memory, network, and disk status",
            ),
            IconName::Cpu,
        ),
        ToolRailToolId::Logs => (
            crate::locale::text("命令历史", "Command history"),
            crate::locale::text("当前终端的最近命令", "Recent commands from this terminal"),
            IconName::RotateCcwClock,
        ),
        ToolRailToolId::Containers => (
            crate::locale::text("容器", "Containers"),
            crate::locale::text(
                "当前 SSH 主机的 Docker、Podman 和 Compose 容器",
                "Docker, Podman, and Compose containers on the current SSH host",
            ),
            IconName::Container,
        ),
        ToolRailToolId::Context | ToolRailToolId::Plugin(_) => return None,
    };
    Some(ToolPresentation::new(label, description, icon))
}
