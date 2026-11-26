//! 扩展页面只保存交互状态；安装和网络由应用用例完成。
//! @author kongweiguang

use crate::theme::ThemeTokens;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
    Focusable as _, FontWeight, InteractiveElement as _, KeyDownEvent, ParentElement as _,
    PathPromptOptions, Pixels, ScrollHandle, Styled as _, Subscription, Window,
    component::{
        Disableable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use kerminal_application::{
    ApplicationTaskExecutor, PublicErrorCode,
    extensions::{
        BuiltinUnit, CatalogIdentity, CatalogPlugin, CatalogSelection, CatalogSnapshot,
        DEFAULT_PLUGIN_CATALOG_URL, DropInInspection, ExtensionChange, ExtensionIcon,
        ExtensionsService, InstallProgress, LocalPackageInspection,
    },
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginId, PluginInstallStatus, PluginSource,
    PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use kerminal_plugin_protocol::Platform;
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

mod markdown;
mod render;

/// 重启由 Root 按会话关闭契约处理，页面不能自行结束进程。
#[derive(Clone, Debug)]
pub enum ExtensionsEvent {
    RestartRequested,
    /// 写操作完成时通知设置面板重绘，让退出拒绝提示随写入结束立即消失。
    WriteFinished,
    /// 商店来源必须走设置面板的草稿 CAS 与权威回读；页面只发请求，不直接写配置。
    CatalogSourceSaveRequested {
        url: String,
    },
}

/// 列表筛选只影响目录组的可见性，不改变安装状态或选中身份。
///
/// 统一列表不再有市场/已安装两种阅读模式；分类只收窄“可安装”目录组，
/// 已安装、内置与待重启组始终完整可见，避免筛选把本地管理入口藏起来。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Filter {
    category: Option<String>,
}

/// 选择必须带来源：内置单元与磁盘冲突记录可能共用同一身份，
/// 只按 id 记录会让两行同时高亮，并让先匹配的详情分支挡住另一个入口。
#[derive(Clone, Debug, Eq, PartialEq)]
enum SelectedEntry {
    /// 注册表内置行，详情走内置启停而非磁盘记录。
    Builtin(PluginId),
    /// 目录、安装记录或 pending 行，详情按目录与记录规则投影。
    Plugin(PluginId),
}

/// 详情页签：把概览说明、权限声明和扩展元信息分栏展示，
/// 避免把所有信息堆在顶部固定区挤占正文阅读空间。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DetailTab {
    #[default]
    Overview,
    Permissions,
    Info,
}

impl DetailTab {
    /// TabBar 按连续下标选中；保持显式映射避免依赖枚举底层序数。
    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Permissions => 1,
            Self::Info => 2,
        }
    }

    /// 下标越界时回退概览，避免异常点击把详情带入未知页签。
    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Permissions,
            2 => Self::Info,
            _ => Self::Overview,
        }
    }
}

impl SelectedEntry {
    /// 列表、README 回执与详情查询只需要稳定身份，来源由调用方另行判断。
    fn id(&self) -> &PluginId {
        match self {
            Self::Builtin(id) | Self::Plugin(id) => id,
        }
    }
}

/// 未经市场审核的文件以及不可逆数据删除必须经过明确确认。
#[derive(Clone)]
enum Confirmation {
    File(PathBuf, LocalPackageInspection),
    Install(CatalogSelection),
    /// 批量更新在确认时冻结整份计划：同一目录身份与逐项目标版本一次确认，
    /// 执行阶段不再重新解析目录，避免确认后静默换包或换版本。
    BatchUpdate(BatchUpdatePlan),
    DeleteData(PluginId),
    /// 手动投放记录在确认时仍绑定观察到的指纹与修订，目录被替换就不能继续。
    DropIn(DropInInspection),
}

/// 单项与批量共用同一可更新判定后的候选，界面只负责展示与冻结。
///
/// 批量计划直接冻结这一结构，不再维护字段完全相同的第二份条目类型，避免新增
/// 候选字段时两处结构和映射漂移。
#[derive(Clone, Debug)]
struct UpdateCandidate {
    id: PluginId,
    name: String,
    from: String,
    selection: CatalogSelection,
    /// 完全访问在确认弹层逐项标出；清单权限只是声明，不是沙箱。
    full_access: bool,
}

/// 批量计划一旦确认就不可变；重试必须重新计算候选并重新确认，不能重放旧身份。
///
/// 每个候选的 `selection.identity` 都已冻结确认时看到的目录身份，安装器会逐项
/// 复核；计划本身不再重复保存聚合身份，避免出现“计划身份”与逐项身份两套事实。
#[derive(Clone, Debug)]
struct BatchUpdatePlan {
    items: Vec<UpdateCandidate>,
}

/// 批量停止原因保留到收尾，反馈才能区分已提交、未知与未处理。
#[derive(Clone, Debug)]
struct BatchFailure {
    id: PluginId,
    name: String,
    reason: String,
    /// 提交结果未知（含执行器外层失败），必须回读后才能重新开放写入。
    unknown: bool,
    /// 安装已返回成功，但随后的权威回读失败；该项已提交事实不能丢。
    committed: bool,
}

/// 批量执行中的当前项；行内进度与工具栏活动槽都从这里取同一事实。
#[derive(Clone, Debug)]
struct BatchProgress {
    index: usize,
    total: usize,
    id: PluginId,
}

/// 单次绘制最多补取多少条市场图标。
///
/// 目录可能包含数千个条目，但图标只是辅助识别信息：无上限地预取会把一次滚动
/// 变成成百上千个网络请求，并让列表长时间停留在占位图标上。安装记录始终全量
/// 读取，目录条目只补前若干条；加载失败或超出预算的条目统一显示 Puzzle，
/// 不会阻塞列表，也不会因图标迟到改变行高。
const MAX_CATALOG_ICON_PREFETCH: usize = 32;

/// 图标队列中的单项请求：已安装插件读取包内图标，市场条目读取目录图标。
///
/// 两种来源的失败语义相同（回退占位），但写回不同的缓存，所以请求种类必须
/// 一直带到结果落地，不能在失败分支里靠猜测还原。
#[derive(Clone)]
enum IconJob {
    Installed(PluginId),
    Catalog(PluginId),
}

/// 已完成读取的图标结果；`None` 是“确认没有可渲染图标”，不是未读取。
enum IconLoaded {
    Installed(Option<ExtensionIcon>),
    Catalog(Option<Arc<[u8]>>),
}

/// 实例拥有搜索焦点与异步代次；服务和 executor 由组合根注入。
pub struct ExtensionsPage {
    service: Arc<ExtensionsService>,
    executor: ApplicationTaskExecutor,
    host_api: String,
    search: Entity<InputState>,
    _search_subscription: Subscription,
    /// 商店来源输入与搜索框分开：弹层关闭后保留的 URL 不能串到列表搜索。
    source_input: Entity<InputState>,
    _source_subscription: Subscription,
    /// 来源弹层与确认弹层共用窗口级 Dialog 生命周期，因此必须互斥打开。
    source_dialog_open: bool,
    /// 延迟打开时核对代次，避免连续点击让旧 defer 打开新弹层。
    source_dialog_generation: u64,
    /// 保存请求已发出、等待设置面板回执；期间禁止重复提交与关闭弹层。
    source_saving: bool,
    /// 保存失败或校验失败的原因；保留在弹层内，输入内容不丢。
    source_error: Option<String>,
    /// 最近一次提交的 URL；回执必须与它匹配，旧保存结果不能覆盖新来源。
    source_pending: Option<String>,
    catalog: Option<CatalogSnapshot>,
    offline: bool,
    /// 目录读取/刷新失败独立保存：市场不可用不能抹掉已安装列表或内置单元。
    catalog_error: Option<String>,
    /// 已安装状态读取失败独立保存：市场仍可浏览，但涉及写入的动作必须禁用并说明原因。
    state_error: Option<String>,
    state: Option<PluginStoreState>,
    /// 内置单元名单来自静态注册表，只提供身份与可关闭性；实际启停仍看 state.builtins。
    builtin_units: Vec<BuiltinUnit>,
    /// 手动投放记录的只读观察；确认按钮必须使用这里的指纹与修订。
    drop_ins: Vec<DropInInspection>,
    /// 已安装外部插件的图标结果；`None` 表示已确认没有可渲染图标，统一回退 Puzzle。
    ///
    /// 图标属于装饰信息，读取失败不进入页面错误区：缺失或损坏只会让该行使用
    /// 统一占位图标，列表与详情仍可正常操作。
    installed_icons: BTreeMap<PluginId, Option<ExtensionIcon>>,
    /// 图标结果对应的安装版本；更新后版本变化时丢弃旧图标，避免继续显示上一版本。
    installed_icon_versions: BTreeMap<PluginId, PluginVersion>,
    /// 市场条目图标按稳定身份缓存；`None` 表示条目没有图标或读取失败。
    catalog_icons: BTreeMap<PluginId, Option<Arc<[u8]>>>,
    /// 图标读取队列串行执行：一次只做一个读盘/网络请求，避免滚动列表时并发风暴。
    icon_cancel: Option<CancellationToken>,
    /// 离开分类或目录换代时推进，迟到的图标结果不能写回新页面状态。
    icon_generation: u64,
    /// drop-in 观察有独立代次，不能被目录刷新或普通写入的代次推进误作废。
    drop_in_generation: u64,
    drop_in_cancel: Option<CancellationToken>,
    /// drop-in 观察失败与权威状态失败分开，市场故障或状态故障不能互相遮蔽。
    drop_in_error: Option<String>,
    /// 本次 drop-in 重读是否持有页面 busy；离开分类时据它释放读盘占用的忙态。
    reloading_drop_ins: bool,
    filter: Filter,
    selected: Option<SelectedEntry>,
    /// 当前详情页签；切换扩展时重置为概览，确保先看扩展说明而不是上一项的权限或元信息。
    detail_tab: DetailTab,
    readme: Option<String>,
    /// README 失败只影响详情正文，保留基本信息、安装与其它选择。
    readme_error: Option<String>,
    readme_cancel: Option<CancellationToken>,
    readme_generation: u64,
    /// 记录当前选择是否已经发起过 README 读取；同项重复 Enter 不再发请求，
    /// 显式重试仍可绕过这一去重。
    readme_attempted: bool,
    status: Option<String>,
    /// 清理残留属于可继续使用的警告，不能与失败错误共用同一种语义色。
    status_is_warning: bool,
    /// 反馈归属必须带选择来源：内置单元与同 ID 外部记录共用稳定身份，
    /// 只按 ID 记录会把一方的失败显示到另一方的详情里。
    feedback_entry: Option<SelectedEntry>,
    busy: bool,
    /// 只有目录读取显示刷新转圈；安装和文件预检不冒充目录刷新。
    refreshing: bool,
    /// 初始缓存读取走不计时的本地读取，界面不能提供实际无效的取消按钮。
    initial_catalog_loading: bool,
    /// 只有安装、更新、启停或卸载等会改插件目录的操作才阻止退出；只读加载不阻止。
    writing: bool,
    cancel: Option<CancellationToken>,
    generation: u64,
    /// 目录与权威状态各自持有代次和取消，刷新目录不能作废正在进行的状态读取。
    catalog_generation: u64,
    catalog_cancel: Option<CancellationToken>,
    state_generation: u64,
    state_cancel: Option<CancellationToken>,
    progress: Option<InstallProgress>,
    active_install: Option<PluginId>,
    /// 批量更新的当前项与总数；单项安装保持 None，行内进度文案据此区分。
    batch: Option<BatchProgress>,
    /// 批量失败或取消后允许“重新检查更新”，但绝不重放旧计划或旧目录身份。
    retry_batch: bool,
    /// 本地包预检与目录安装共用取消门禁，页面级活动条据此显示校验状态。
    inspecting: bool,
    /// 页面是否仍属于当前设置分类；离开后迟到的预检不能升级为窗口级确认。
    active: bool,
    /// 重试同时保存反馈来源，避免同 ID 的内置与外部记录在重试后把结果挂到另一侧。
    retry_operation: Option<RetryOperation>,
    confirmation: Option<Confirmation>,
    /// Kit 窗口级确认只允许一个实例；标记防止重建页面或连续点击重复打开。
    dialog_open: bool,
    /// 延迟打开时核对代次，避免连续确认让旧 defer 打开新弹层。
    dialog_generation: u64,
    window_handle: AnyWindowHandle,
    list_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
    /// 正文实际可用宽度；分栏或单栏按真实内容区决定，不用整窗宽度猜。
    content_width: Rc<Cell<Option<Pixels>>>,
    /// 列表键盘焦点；方向键、Home/End 与 Enter 都作用在这一个焦点上。
    list_focus: FocusHandle,
    /// 详情焦点：窄窗里列表被卸载后，Enter/点击进入详情必须把焦点交给它，
    /// Escape 也由它所在的详情子树处理，避免冒泡到 Root 关闭整个设置。
    detail_focus: FocusHandle,
    /// 内置组默认收起；搜索命中时由渲染层临时展开，不改变用户选择。
    builtin_expanded: bool,
    /// 窄窗详情打开后，返回列表只隐藏详情，不丢失模式、搜索、选中项与滚动位置。
    back_to_list: bool,
    /// 上一帧列表是否挂载；只在挂载面发生切换的边沿做一次焦点交接，
    /// 不能每帧抢走用户刚放到搜索、工具栏或确认弹层上的焦点。
    last_list_visible: Option<bool>,
}
impl EventEmitter<ExtensionsEvent> for ExtensionsPage {}

impl ExtensionsPage {
    /// 创建页面时不读取磁盘，首次加载由显式 activate 启动。
    pub fn new(
        service: Arc<ExtensionsService>,
        executor: ApplicationTaskExecutor,
        host_api: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("搜索扩展", "Search extensions"))
        });
        let subscription = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        // 来源输入独立于搜索：弹层打开时只编辑 URL，关闭后也不污染列表查询。
        let source_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "https://…/catalog.json",
                "https://…/catalog.json",
            ))
        });
        let source_subscription = cx.subscribe(&source_input, |page, _, event: &InputEvent, cx| {
            // 用户一旦修改地址就撤掉上一次失败提示，避免旧错误看起来仍属于新输入。
            if matches!(event, InputEvent::Change) && page.source_error.is_some() {
                page.source_error = None;
                cx.notify();
            }
        });
        let window_handle = window.window_handle();
        Self {
            service,
            executor,
            host_api,
            search,
            _search_subscription: subscription,
            source_input,
            _source_subscription: source_subscription,
            source_dialog_open: false,
            source_dialog_generation: 0,
            source_saving: false,
            source_error: None,
            source_pending: None,
            catalog: None,
            offline: false,
            catalog_error: None,
            state_error: None,
            state: None,
            builtin_units: Vec::new(),
            drop_ins: Vec::new(),
            installed_icons: BTreeMap::new(),
            installed_icon_versions: BTreeMap::new(),
            catalog_icons: BTreeMap::new(),
            icon_cancel: None,
            icon_generation: 0,
            drop_in_generation: 0,
            drop_in_cancel: None,
            drop_in_error: None,
            reloading_drop_ins: false,
            filter: Filter::default(),
            selected: None,
            detail_tab: DetailTab::Overview,
            readme: None,
            readme_error: None,
            readme_cancel: None,
            readme_generation: 0,
            // 同一选择重复 Enter 不能反复请求 README；只有显式重试才重新发起。
            readme_attempted: false,
            status: None,
            status_is_warning: false,
            feedback_entry: None,
            busy: false,
            refreshing: false,
            initial_catalog_loading: false,
            writing: false,
            cancel: None,
            generation: 0,
            catalog_generation: 0,
            catalog_cancel: None,
            state_generation: 0,
            state_cancel: None,
            progress: None,
            active_install: None,
            batch: None,
            retry_batch: false,
            inspecting: false,
            active: false,
            retry_operation: None,
            confirmation: None,
            dialog_open: false,
            dialog_generation: 0,
            window_handle,
            list_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            content_width: Rc::new(Cell::new(None)),
            // 列表容器本身是 Tab 停靠点；行内按钮仍是各自独立的 Tab 停靠点。
            list_focus: cx.focus_handle().tab_stop(true),
            // 详情根是焦点容器而不是 Tab 停靠点：行点击/Enter 可程序化聚焦它，
            // Tab 则直接进入返回按钮与详情动作，避免多一次空停靠。
            detail_focus: cx.focus_handle().tab_stop(false),
            builtin_expanded: false,
            back_to_list: false,
            last_list_visible: None,
        }
    }

    /// 语言切换只更新占位符，不重建搜索实体，避免用户输入与焦点被重置。
    ///
    /// @author kongweiguang
    pub(crate) fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索扩展", "Search extensions"),
                window,
                cx,
            )
        });
        // 来源弹层可能跨语言切换存活；占位符与标题一样跟随当前语言刷新。
        self.source_input.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("https://…/catalog.json", "https://…/catalog.json"),
                window,
                cx,
            )
        });
    }

    /// 首次进入分别读取可信缓存与权威状态；任一失败保留另一侧可用内容并提供独立重试。
    ///
    /// 目录、状态与 drop-in 观察各自使用独立任务、代次与取消令牌并行执行：目录缓存
    /// 损坏不能挡住已安装列表，状态仓储失败也不能让市场浏览变成空白，drop-in 只补充
    /// 磁盘证据，失败不应阻塞列表与详情。先完成的一侧立即落地，共享计数只决定何时
    /// 释放页面 busy。
    ///
    /// @author kongweiguang
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        // 切回设置或重新进入时，遗留的窗口级确认必须先关闭，不能让它覆盖新页面状态。
        self.close_open_confirmation(cx);
        // 来源弹层同样属于窗口级 Dialog；重新进入时旧弹层不能覆盖新页面状态。
        self.close_source_dialog(cx);
        // 重新进入不清理旧警告与重试：未知提交和残留材料仍需要用户看见并处理；
        // 只有明确的刷新动作才会丢弃旧重试意图。
        if self.busy {
            return;
        }
        // 上一次网络刷新可能仍在途；重新进入要由新的初始读取接管，先取消旧任务，
        // 否则旧请求会继续占用网络与工作线程，直到自己结束。
        if let Some(token) = self.catalog_cancel.take() {
            token.cancel();
        }
        if let Some(token) = self.state_cancel.take() {
            token.cancel();
        }
        // 内置名单是编译期静态数据；读取失败说明注册表自身无效，不能带着空名单继续。
        match self.service.builtin_units() {
            Ok(units) => self.builtin_units = units,
            Err(error) => {
                self.state_error = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        }
        // drop-in 观察与目录/状态读取并行启动：三者是彼此独立的只读来源，
        // 任何一个慢或失败都不应推迟另外两个的结果落地。
        self.supersede_drop_in_inspection();
        let drop_in_generation = self.drop_in_generation;
        let drop_in_token = CancellationToken::new();
        self.drop_in_cancel = Some(drop_in_token.clone());
        let inspect_service = self.service.clone();
        let inspect_executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let inspections = inspect_executor
                .run_cancellable(drop_in_token, move |cancel| async move {
                    inspect_service.inspect_drop_ins(cancel).await
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.drop_in_generation != drop_in_generation {
                    return;
                }
                page.drop_in_cancel = None;
                match inspections {
                    Ok(items) => {
                        page.drop_ins = items;
                        page.drop_in_error = None;
                        page.prune_selection(cx);
                    }
                    // 页面离开或被新一次激活取代时静默收尾，不把取消写成可重试错误。
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.drop_in_error = Some(error.public_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();

        self.busy = true;
        self.refreshing = true;
        self.initial_catalog_loading = true;
        // 上一次写入后的读盘若还没结束，已被本次初始读取的代次作废；清掉标记，
        // 避免它继续影响后续离开分类时的 busy 释放判断。
        self.reloading_drop_ins = false;
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        self.state_generation = self.state_generation.wrapping_add(1);
        let catalog_generation = self.catalog_generation;
        let state_generation = self.state_generation;
        let catalog_token = CancellationToken::new();
        let state_token = CancellationToken::new();
        self.catalog_cancel = Some(catalog_token.clone());
        self.state_cancel = Some(state_token.clone());
        // 两个读取各持独立任务、代次与取消，先完成的一侧立即落地；共享计数只用于
        // 在两者都结束后释放页面 busy，不让慢的一侧把已到达的结果压在手里。
        let pending = Rc::new(Cell::new(2u8));
        let state_service = self.service.clone();
        let state_executor = self.executor.clone();
        let state_pending = pending.clone();
        cx.spawn(async move |this, cx| {
            let result = state_executor
                .run_cancellable(state_token, move |token| async move {
                    let _ = token;
                    state_service.state().await
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.state_generation != state_generation {
                    return;
                }
                page.state_cancel = None;
                match result {
                    Ok(state) => {
                        page.state = Some(state);
                        page.state_error = None;
                        page.prune_selection(cx);
                    }
                    // 取消属于页面的正常空闲路径，不留下错误文案或重试入口。
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.state_error = Some(error.public_message().to_owned());
                    }
                }
                finish_initial_load(page, &state_pending, cx);
                cx.notify();
            });
        })
        .detach();
        let catalog_service = self.service.clone();
        let catalog_executor = self.executor.clone();
        let catalog_pending = pending.clone();
        cx.spawn(async move |this, cx| {
            let result = catalog_executor
                .run_cancellable(catalog_token, move |token| async move {
                    let _ = token;
                    catalog_service.cached_catalog().await
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.catalog_generation != catalog_generation {
                    return;
                }
                page.catalog_cancel = None;
                page.refreshing = false;
                page.initial_catalog_loading = false;
                match result {
                    Ok(catalog) => {
                        page.offline = catalog.as_ref().is_some_and(|snapshot| snapshot.stale);
                        page.catalog = catalog;
                        page.catalog_error = None;
                        page.reconcile_catalog_change(cx);
                        page.prune_selection(cx);
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.catalog_error = Some(error.public_message().to_owned());
                    }
                }
                finish_initial_load(page, &catalog_pending, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 新的状态读取或写入会覆盖旧观察；使在途 drop-in 检查按代次作废并释放取消 token。
    fn supersede_drop_in_inspection(&mut self) {
        self.drop_in_generation = self.drop_in_generation.wrapping_add(1);
        if let Some(token) = self.drop_in_cancel.take() {
            token.cancel();
        }
    }

    /// 离开扩展分类或关闭设置时立即失去可见性；迟到的文件预检不能弹出窗口级确认。
    /// 只取消预检 token，安装、刷新和写操作仍按原有回执与退出门禁收尾。
    pub(crate) fn deactivate(&mut self, cx: &mut Context<Self>) {
        self.active = false;
        self.close_open_confirmation(cx);
        // 离开分类后来源弹层没有展示目标；保存回执仍会通过面板回到页面状态，
        // 但窗口级 Dialog 必须先收起，避免它盖住其它设置分类。
        self.close_source_dialog(cx);
        // 离开分类后 drop-in 观察没有展示目标，继续读取只会占用工作线程。
        self.supersede_drop_in_inspection();
        // 图标队列同样没有展示目标；推进代次让在途回执失效并取消剩余读取。
        // 已落地的结果保留，重新进入时只补未完成的条目。
        self.icon_generation = self.icon_generation.wrapping_add(1);
        if let Some(token) = self.icon_cancel.take() {
            token.cancel();
        }
        // 目录与状态读取各自取消并推进代次，迟到回执不能写回已离开的页面。
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        self.state_generation = self.state_generation.wrapping_add(1);
        self.initial_catalog_loading = false;
        if let Some(token) = self.catalog_cancel.take() {
            token.cancel();
        }
        if let Some(token) = self.state_cancel.take() {
            token.cancel();
        }
        // README 是详情局部请求；离开分类后取消并推进代次，迟到回执不能写回。
        // 去重标记必须一起复位，否则重新进入或宽窄转换让详情可见时永远不会补读。
        self.readme_generation = self.readme_generation.wrapping_add(1);
        if let Some(token) = self.readme_cancel.take() {
            token.cancel();
        }
        self.readme_attempted = false;
        // 目录刷新是独立只读任务，离开分类后它的回执会因代次失配被丢弃；刷新可见
        // 状态必须在这里结束，否则在途写入期间离开再回来会永久显示刷新中并挡住刷新
        // 入口。写忙态仍由 writing 保护，不能在这里释放。
        self.refreshing = false;
        // 只读加载没有写操作需要等待；写入仍由 writing 门禁保护，不能在这里释放。
        if !self.writing {
            self.busy = false;
            // 预检是协作式取消：底层校验可能仍返回 Ok，必须推进操作代次让迟到回执
            // 失效，否则重新进入后旧结果会弹出确认并提前释放新一轮读取的 busy。
            // 在途写入不能推进代次，否则写回执永远等不到自己的 generation，writing
            // 会永久挂住。
            self.generation = self.generation.wrapping_add(1);
            if self.inspecting {
                self.inspecting = false;
                if let Some(token) = self.cancel.take() {
                    token.cancel();
                }
            }
        }
        // drop-in 重读的完成回调会因代次失配而提前返回，不能指望它清 busy；
        // 读盘 busy 由发起者自己释放，否则重新进入分类会被 busy 永久挡住。
        if self.reloading_drop_ins && !self.writing {
            self.reloading_drop_ins = false;
            self.busy = false;
        }
    }

    /// 工具栏刷新是用户显式动作，始终重新请求目录并保留旧缓存可见。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_catalog(false, cx);
    }

    /// 进入分类时只按缓存有效期刷新，避免每次切换设置页都重复下载市场。
    fn refresh_if_due(&mut self, cx: &mut Context<Self>) {
        self.refresh_catalog(true, cx);
    }

    /// 强制与按需刷新共用同一取消、代次和错误投影，避免两条路径出现不同状态机。
    ///
    /// 刷新只推进目录代次，不触碰权威状态读取：市场刷新失败不能作废已安装列表，
    /// 也不能把状态读取的错误改写成目录错误。
    ///
    /// @author kongweiguang
    fn refresh_catalog(&mut self, due_only: bool, cx: &mut Context<Self>) {
        // 目录刷新是只读来源，只占 refreshing 槽位；不能借用写忙态，否则市场网络
        // 请求期间本地已安装管理会被无关任务一直挡住。写操作仍由 run_operation
        // 的 busy 门禁串行化。
        if self.busy || self.refreshing {
            return;
        }
        self.refreshing = true;
        self.initial_catalog_loading = false;
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        let generation = self.catalog_generation;
        let cancel = CancellationToken::new();
        self.catalog_cancel = Some(cancel.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancel, move |token| async move {
                    if due_only {
                        service.refresh_if_due(token).await
                    } else {
                        service.refresh_catalog(token).await
                    }
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.catalog_generation != generation {
                    return;
                }
                page.refreshing = false;
                page.catalog_cancel = None;
                match result {
                    // 刷新返回 stale 只表示网络/落盘失败后回退到同源缓存，不是一次
                    // 成功刷新。页面已有目录时覆盖会推进目录身份并可能让分类、选择
                    // 和重试入口一起消失，所以保留用户正在看的内容；页面还没有目录
                    // （初始读取失败或同源缓存刚恢复）时则补上这份经过校验的缓存，
                    // 否则提示“正在展示旧内容”却不展示任何目录。两种分支都只把
                    // 失败挂到同一个刷新按钮，offline 仍阻止基于过期目录发起更新。
                    Ok(catalog) if catalog.stale => {
                        page.offline = true;
                        let message = if page.catalog.is_none() {
                            page.catalog = Some(catalog);
                            crate::locale::text(
                                "市场来源未能更新，显示上次缓存的内容",
                                "The marketplace source could not be updated; showing the cached catalog",
                            )
                        } else {
                            crate::locale::text(
                                "市场来源未能更新，仍显示上次加载的内容",
                                "The marketplace source could not be updated; showing the previously loaded catalog",
                            )
                        };
                        page.catalog_error = Some(message.to_string());
                    }
                    Ok(catalog) => {
                        page.offline = false;
                        page.catalog = Some(catalog);
                        page.catalog_error = None;
                        // 显式刷新代表用户要求重新观察，成功后才清掉基于旧目录/旧状态的
                        // 重试入口；失败或 stale 回退仍需保留非目录操作的重试路径。
                        if !due_only {
                            page.retry_operation = None;
                        }
                        page.prune_selection(cx);
                        page.reconcile_catalog_change(cx);
                        // 用户显式刷新后重读磁盘观察，修好或移除投放文件无需离开分类再回来；
                        // 进入分类的按需刷新已由 activate 的后继观察覆盖，避免并发读取。
                        if !due_only {
                            page.reload_drop_in_state(cx);
                        }
                    }
                    Err(error) => {
                        // 用户取消刷新时静默回到空闲；只有真实失败才保留目录内容并记录原因。
                        if error.public_code() != PublicErrorCode::Cancelled {
                            page.offline = page.catalog.is_some();
                            page.catalog_error = Some(error.public_message().to_owned());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 用户所选文件先经过界面确认，路径只传给应用服务进行校验。
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        // 文件安装最终会写权威状态；状态未知时连选择入口都不应继续，避免用户
        // 走完预检才在安装阶段被静默拒绝。
        if self.busy || self.state.is_none() || self.state_error.is_some() {
            return;
        }
        self.retry_operation = None;
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择插件安装包",
                "Choose an extension package",
            )),
        });
        cx.spawn(async move |this, cx| {
            let result = selected.await;
            let _ = this.update(cx, |page, cx| {
                match result {
                    Ok(Ok(Some(mut paths))) => {
                        if let Some(path) = paths.pop() {
                            page.inspect_file(path, cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        page.retry_operation = None;
                        // 文件选择失败没有对象归属，必须清掉上一对象的反馈槽位，
                        // 否则错误会显示在无关详情里或随选择切换彻底消失。
                        page.feedback_entry = None;
                        page.status = Some(
                            crate::locale::text(
                                "无法读取所选路径",
                                "Unable to read the selected path",
                            )
                            .to_string(),
                        );
                        page.status_is_warning = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 确认页展示真实清单及摘要；选中的文件在安装时仍由服务按预检摘要复核。
    fn inspect_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.busy || self.state.is_none() || self.state_error.is_some() {
            return;
        }
        // 预检会推进页面代次；旧 drop-in 观察不能跨这次操作继续落地。
        self.supersede_drop_in_inspection();
        self.busy = true;
        self.inspecting = true;
        self.retry_operation = None;
        // 清理残留等警告仍与磁盘材料有关，开始新预检不能把它一并抹掉；
        // 只有普通错误会被新操作替换。
        if !self.status_is_warning {
            self.status = None;
        }
        self.confirmation = None;
        self.dialog_open = false;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancellation = CancellationToken::new();
        self.cancel = Some(cancellation.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    service
                        .inspect_file(path.clone(), token)
                        .await
                        .map(|inspection| (path, inspection))
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.busy = false;
                page.inspecting = false;
                page.cancel = None;
                match result {
                    Ok((path, inspection)) => {
                        page.open_confirmation(Confirmation::File(path, inspection), cx)
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.retry_operation = None;
                        // 预检失败没有对象归属；清掉旧反馈槽位，避免把错误挂到
                        // 上一次操作的对象或让它在切换选择后消失。
                        page.feedback_entry = None;
                        page.status = Some(error.public_message().to_owned());
                        page.status_is_warning = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 验收只注入系统文件选择结果，后续仍经过正式预检、确认和安装流程。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn acceptance_inspect_file(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        if self.busy {
            return Err(kerminal_application::AppError::conflict(
                "扩展页面正在处理操作",
            ));
        }
        self.inspect_file(path, cx);
        Ok(())
    }

    /// 打开详情的选择入口；详情一旦打开就实际挂载，因此 README 可以读取。
    fn select(&mut self, entry: SelectedEntry, cx: &mut Context<Self>) {
        self.select_entry(entry, true, true, cx);
    }

    /// 选择与“是否打开详情”分开：窄窗里方向键只移动高亮，Enter 或点击才进入详情，
    /// 否则列表会在第一次按键后立刻被详情替换，后续方向键失去列表焦点。
    ///
    /// `detail_visible` 由持有窗口宽度的调用方给出：宽窗详情始终可见，窄窗只有
    /// 打开详情后才可见。详情不可见时不发起 README 请求，既避免不可见 I/O，也避免
    /// 随后 Enter 对同一项重复请求；同一选择已经请求过时，重复 Enter 直接复用结果，
    /// 显式重试走 `retry_readme`。
    fn select_entry(
        &mut self,
        entry: SelectedEntry,
        open_detail: bool,
        detail_visible: bool,
        cx: &mut Context<Self>,
    ) {
        // 窄窗中选中条目才打开详情；返回列表只隐藏详情，不改变这次选择。
        self.back_to_list = !open_detail;
        // 详情内容随选择整体替换，沿用上一个插件的滚动位置会让人误以为没切换成功。
        self.detail_scroll.set_offset(Default::default());
        let same = self.selected.as_ref() == Some(&entry);
        if same && detail_visible && self.readme_attempted {
            // 同一项重复 Enter/点击只恢复详情可见性，不重发已经完成或仍在途的请求。
            cx.notify();
            return;
        }
        if !same {
            // 切换到新扩展时回到概览页签，避免沿用上一扩展的权限/信息页签。
            self.detail_tab = DetailTab::Overview;
            // 选择变化时旧 README 属于上一项，必须取消并清空，不能让新详情短暂
            // 显示上一项的说明，也不能让旧回执覆盖新选择。
            if let Some(previous) = self.readme_cancel.take() {
                previous.cancel();
            }
            self.readme_generation = self.readme_generation.wrapping_add(1);
            self.readme = None;
            self.readme_error = None;
            self.readme_attempted = false;
        }
        self.selected = Some(entry);
        if !detail_visible {
            // 窄窗方向键只移动高亮，详情仍被列表遮住；此时读取不可见，Enter 打开
            // 详情时再按需加载。
            cx.notify();
            return;
        }
        // 详情优先级已由 selected_plugin 统一判定：内置、未确认投放和目录缺席都没有目录条目，
        // 选它们只切换详情；已确认的 drop-in 若同时出现在市场目录，详情由目录条目接管，
        // 仍需加载签名 README，不能仅凭安装来源提前跳过。
        if self.selected_plugin().is_none() {
            cx.notify();
            return;
        }
        self.load_readme(cx);
    }

    /// 切换详情页签并回到顶部，避免沿用上一页签的滚动偏移。
    ///
    /// @author kongweiguang
    fn select_detail_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        if self.detail_tab == tab {
            return;
        }
        self.detail_tab = tab;
        self.detail_scroll.set_offset(Default::default());
        cx.notify();
    }

    /// 显式重试只针对当前选择重新读取 README；与普通选择不同，它允许覆盖
    /// `readme_attempted` 的去重，因为用户明确要求重新请求。
    fn retry_readme(&mut self, cx: &mut Context<Self>) {
        if self.selected_plugin().is_none() {
            return;
        }
        self.load_readme(cx);
    }

    /// 发起当前选择的 README 读取；调用方负责确认详情可见且存在目录条目。
    fn load_readme(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let id = selected.id().clone();
        if let Some(previous) = self.readme_cancel.take() {
            previous.cancel();
        }
        self.readme_generation = self.readme_generation.wrapping_add(1);
        let generation = self.readme_generation;
        self.readme = None;
        self.readme_error = None;
        self.readme_attempted = true;
        let token = CancellationToken::new();
        self.readme_cancel = Some(token.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        let selected_id = id.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(token, move |cancel| async move {
                    service.readme(&id, cancel).await.map(|text| (id, text))
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.readme_generation != generation
                    || !matches!(
                        page.selected.as_ref(),
                        Some(SelectedEntry::Plugin(selected)) if selected == &selected_id
                    )
                {
                    return;
                }
                page.readme_cancel = None;
                match result {
                    Ok((_, text)) => {
                        page.readme = text;
                        page.readme_error = None;
                    }
                    // 取消只说明用户换了选择或离开分类，不留下可重试错误。
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.readme = None;
                        page.readme_error = Some(error.public_message().to_owned());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 详情因宽窄转换或重新进入而真正可见时补读 README。
    ///
    /// 选择动作只覆盖“用户主动选中”的路径；窗口变宽、返回列表后重开详情或重新
    /// 进入设置页时，高亮项可能早已存在但详情此前被遮住。这里以
    /// `readme_attempted` 做一次性去重，避免每帧重发；显式重试仍走
    /// `retry_readme`，不受该标记限制。
    fn ensure_readme_for_visible_detail(&mut self, detail_visible: bool, cx: &mut Context<Self>) {
        if !detail_visible || self.readme_attempted || self.selected_plugin().is_none() {
            return;
        }
        self.load_readme(cx);
    }

    /// 为已安装记录与可浏览目录条目补取图标。
    ///
    /// 页面只消费应用层给出的“内置名或已验字节”，不在这里读文件或拼 URL。
    /// 一次绘制只启动一条串行队列，已加载或已确认缺失的条目不会重复请求；
    /// 结果到达前图标槽始终使用统一占位，所以列表与详情不会因为图标迟到跳动。
    ///
    /// 安装记录按当前版本做键：更新完成后旧图标立即失效并重新读取，避免列表
    /// 继续展示上一版本的视觉身份。市场图标按 `MAX_CATALOG_ICON_PREFETCH`
    /// 限量预取，剩余条目保持占位，这是刻意取舍而不是遗漏。
    ///
    /// @author kongweiguang
    fn ensure_icons(&mut self, cx: &mut Context<Self>) {
        // 已经有一条队列在跑时不追加任务：它结束时会重绘，下一帧再补新出现的条目。
        if self.icon_cancel.is_some() {
            return;
        }
        let installed: Vec<(PluginId, PluginVersion)> = self
            .state
            .as_ref()
            .map(|state| {
                state
                    .plugins
                    .iter()
                    .filter(|(id, record)| {
                        // 内置身份与占用内置身份的冲突记录由内置映射或 Puzzle 表达，
                        // 不需要也不应该去读一个不属于外部包的目录。
                        !is_reserved_identity(record)
                            && !self.builtin_units.iter().any(|unit| &unit.id == *id)
                    })
                    .map(|(id, record)| (id.clone(), record.active.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let installed_ids: BTreeSet<&PluginId> =
            installed.iter().map(|(id, _version)| id).collect();
        let catalog: Vec<(PluginId, bool)> = self
            .catalog
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .catalog
                    .plugins
                    .iter()
                    .filter(|plugin| {
                        // 未安装条目始终按目录图标补取；已安装插件只在确认包内没有
                        // 图标后才回退市场图标，避免覆盖本地图标或多余发网。
                        !installed_ids.contains(&plugin.id)
                            || matches!(self.installed_icons.get(&plugin.id), Some(None))
                    })
                    .map(|plugin| (plugin.id.clone(), plugin.icon_url.is_some()))
                    .collect()
            })
            .unwrap_or_default();

        let mut jobs = Vec::new();
        for (id, version) in installed {
            let current = self.installed_icon_versions.get(&id);
            if current == Some(&version) && self.installed_icons.contains_key(&id) {
                continue;
            }
            if current != Some(&version) {
                self.installed_icons.remove(&id);
                self.installed_icon_versions.insert(id.clone(), version);
            }
            jobs.push(IconJob::Installed(id));
        }
        let mut budget = MAX_CATALOG_ICON_PREFETCH;
        for (id, declares_icon) in catalog {
            if self.catalog_icons.contains_key(&id) {
                continue;
            }
            if !declares_icon {
                // 没有声明图标是确定结果，写入 None 防止每帧重复判断。
                self.catalog_icons.insert(id, None);
                continue;
            }
            if budget == 0 {
                break;
            }
            budget -= 1;
            jobs.push(IconJob::Catalog(id));
        }
        if jobs.is_empty() {
            return;
        }

        self.icon_generation = self.icon_generation.wrapping_add(1);
        let generation = self.icon_generation;
        let token = CancellationToken::new();
        self.icon_cancel = Some(token.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            for job in jobs {
                if token.is_cancelled() {
                    break;
                }
                // 失败分支仍需要知道结果该写回哪一张缓存表，因此在消费请求前记录。
                let installed_job = matches!(job, IconJob::Installed(_));
                let (id, loaded) = match job {
                    IconJob::Installed(id) => {
                        let request = id.clone();
                        let service = service.clone();
                        let executor = executor.clone();
                        let cancel = token.clone();
                        let result = executor
                            .run_cancellable(cancel, move |cancel| async move {
                                service.installed_icon(&request, cancel).await
                            })
                            .await;
                        (id, result.map(IconLoaded::Installed))
                    }
                    IconJob::Catalog(id) => {
                        let request = id.clone();
                        let service = service.clone();
                        let executor = executor.clone();
                        let cancel = token.clone();
                        let result = executor
                            .run_cancellable(cancel, move |cancel| async move {
                                service.catalog_icon(&request, cancel).await
                            })
                            .await;
                        (id, result.map(IconLoaded::Catalog))
                    }
                };
                let keep_running = this.update(cx, |page, cx| {
                    // 离开分类或目录换代后，旧结果不能再写回；直接结束队列。
                    if page.icon_generation != generation {
                        return false;
                    }
                    match loaded {
                        Ok(IconLoaded::Installed(icon)) => {
                            page.installed_icons.insert(id, icon);
                        }
                        Ok(IconLoaded::Catalog(icon)) => {
                            page.catalog_icons.insert(id, icon);
                        }
                        // 取消通常是页面离开或来源切换，保持“未读取”让下次进入重试。
                        Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                            return false;
                        }
                        // 图标是装饰信息：普通失败直接落到统一占位，不制造错误条。
                        Err(_) if installed_job => {
                            page.installed_icons.insert(id, None);
                        }
                        Err(_) => {
                            page.catalog_icons.insert(id, None);
                        }
                    }
                    cx.notify();
                    true
                });
                if !keep_running.unwrap_or(false) {
                    break;
                }
            }
            let _ = this.update(cx, |page, cx| {
                if page.icon_generation == generation {
                    page.icon_cancel = None;
                    // 队列结束前可能又出现了新条目或超出预算的目录项，重绘让下一帧补取。
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 挂载面切换时把焦点交给仍可见的一侧；只在边沿执行，不按帧抢焦点。
    ///
    /// 宽窗列表持焦后缩窄会把列表卸载，若继续把焦点留在失效句柄上，Escape
    /// 和后续键盘操作都到不了详情。反向从详情切回列表也对称处理。延迟到当前
    /// 效果周期结束，并再次核对焦点仍属于被卸载的一侧，用户刚放到搜索、工具栏
    /// 或确认弹层上的焦点不会被抢走。
    fn hand_over_focus_on_layout_change(
        &mut self,
        list_visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.last_list_visible.replace(list_visible);
        if previous == Some(list_visible) {
            return;
        }
        let focus_detail = previous == Some(true) && !list_visible;
        let focus_list = previous == Some(false) && list_visible;
        if !focus_detail && !focus_list {
            return;
        }
        let page = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let _ = page.update(cx, |page, cx| {
                if !page.active {
                    return;
                }
                if focus_detail
                    && page.list_focus.is_focused(window)
                    && page.is_narrow(window)
                    && page.selected.is_some()
                    && !page.back_to_list
                {
                    window.focus(&page.detail_focus, cx);
                } else if focus_list
                    && page.detail_focus.is_focused(window)
                    && page.is_narrow(window)
                    && (page.selected.is_none() || page.back_to_list)
                {
                    window.focus(&page.list_focus, cx);
                }
            });
        });
    }

    /// 所有写操作完成后回读权威状态；失败保留明确错误，禁止自动重放。
    ///
    /// 中央门禁不依赖按钮的 disabled：旧帧、键盘回执或状态弹层都可能带着过期
    /// 入口到达，只要权威状态未知（读取失败、提交后回读失败、未知提交）就拒绝
    /// 开始任何写入，统一引导用户先刷新状态。
    ///
    /// @author kongweiguang
    fn run_operation(&mut self, operation: Operation, cx: &mut Context<Self>) {
        self.run_operation_with_feedback(operation, None, cx);
    }

    /// 带反馈来源的写入口：内置单元与同 ID 外部记录共用稳定身份，必须由调用方
    /// 指明这次操作属于哪一行，失败提示才能回到正确的详情。
    ///
    /// @author kongweiguang
    fn run_operation_with_feedback(
        &mut self,
        operation: Operation,
        feedback: Option<SelectedEntry>,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.state_error.is_some() || self.state.is_none() {
            return;
        }
        // 批量更新复用同一写入门禁，但执行语义是串行多步与部分完成回执，
        // 不能套用单次写入的“一个回执对应一个对象”反馈模型。
        if let Operation::BatchUpdate(plan) = &operation {
            let plan = plan.clone();
            self.run_batch_update(plan, cx);
            return;
        }
        // 单项操作重新建立“本次结果”语义；旧的批量重试入口不能继续指向过期计划。
        self.retry_batch = false;
        // 写操作基于当前观察；在途 drop-in 检查不能把旧指纹重新写回页面。
        self.supersede_drop_in_inspection();
        self.busy = true;
        self.inspecting = false;
        // 写操作尚未收到权威回读时退出会丢失提交结果，只有这一阶段需要阻止退出。
        self.writing = true;
        // 清理残留警告描述的是磁盘材料，开始新写入也不能把它静默抹掉；
        // 普通失败会被本次结果替换。
        if !self.status_is_warning {
            self.status = None;
        }
        self.retry_operation = None;
        self.dialog_open = false;
        self.active_install = match &operation {
            Operation::Install(selection) => Some(selection.id.clone()),
            Operation::File(_, inspection) => Some(inspection.id.clone()),
            _ => None,
        };
        // 失败反馈绑定到发起操作的对象；文件预检等无对象入口由页头状态承载。
        let feedback_entry = feedback.or_else(|| operation_entry(&operation));
        self.feedback_entry = feedback_entry.clone();
        self.confirmation = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = matches!(&operation, Operation::Install(_) | Operation::File(_, _))
            .then(|| cancel.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        let retry_candidate = operation.clone();
        let retry_feedback = feedback_entry.clone();
        let (progress_tx, mut progress_rx) =
            kerminal_application::extensions::install_progress_channel();
        if matches!(&operation, Operation::Install(_) | Operation::File(_, _)) {
            self.progress = Some(InstallProgress::Preparing);
            cx.spawn(async move |this, cx| {
                while progress_rx.changed().await.is_ok() {
                    let progress = progress_rx.borrow_and_update().clone();
                    if this
                        .update(cx, |page, cx| {
                            if page.generation == generation && page.busy {
                                page.progress = Some(progress);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancel, move |token| async move {
                    let write = match operation {
                        Operation::Install(selection) => service.install(selection, progress_tx, token).await.map(|receipt| receipt.cleanup_warning),
                        Operation::File(path, inspected) => service.install_file(path, inspected, true, progress_tx, token).await.map(|receipt| receipt.cleanup_warning),
                        Operation::Change(change) => service.change(change).await.map(|receipt| receipt.cleanup_warning),
                        Operation::ConfirmDropIn { id, fingerprint, revision } => service.confirm_drop_in(id, fingerprint, revision).await.map(|receipt| receipt.cleanup_warning),
                        Operation::Delete(id) => service.delete_data(id, true).await.map(|()| None),
                        // 批量计划在入口处已分流；旧帧或内部调用若绕过入口，明确拒绝，
                        // 不能伪造一次单次写入回执。
                        Operation::BatchUpdate(_) => Err(
                            kerminal_application::AppError::invalid("批量更新必须通过批量入口执行"),
                        ),
                    };
                    Ok(match write {
                        Err(error) => Err((error, false, None)),
                        Ok(warning) => service.state().await.map(|state| (state, warning.clone())).map_err(|error| (error, true, warning)),
                    })
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.busy = false;
                page.writing = false;
                page.cancel = None;
                page.progress = None;
                page.active_install = None;
                let confirm_drop_in = matches!(&retry_candidate, Operation::ConfirmDropIn { .. });
                match result {
                    Ok(Ok((state, warning))) => {
                        page.state = Some(state);
                        page.state_error = None;
                        page.status_is_warning = warning.is_some();
                        page.status = warning.map(|error| error.public_message().to_owned());
                        if page.status.is_none() {
                            page.feedback_entry = None;
                        }
                        // 任何写入都会改变磁盘记录；重读观察才能更新指纹、损坏与待确认状态，
                        // 否则确认入口会拿着旧观察必然冲突，修好的文件也要离开分类才可见。
                        page.reload_drop_in_state(cx);
                    }
                    Ok(Err((error, committed, warning))) => {
                        // 取消安装属于用户主动结束，不写错误、不提供重试，也不把页面标成失败。
                        if error.public_code() != PublicErrorCode::Cancelled {
                            let stale_drop_in = confirm_drop_in
                                && !committed
                                && error.public_code() == PublicErrorCode::Conflict;
                            if stale_drop_in {
                                // 指纹或修订已过期；旧值重试只会继续冲突，必须按最新状态刷新。
                                page.retry_operation = None;
                                page.status_is_warning = true;
                                page.status = Some(
                                    crate::locale::text(
                                        "状态已更改，请刷新",
                                        "The state has changed; refresh to continue.",
                                    )
                                    .to_string(),
                                );
                                page.reload_drop_in_state(cx);
                            } else if committed {
                                // 写已提交但回读失败：权威状态未知，所有写入口必须停用。
                                // 保留已提交事实，先做一次安全回读；成功后才解除门禁，
                                // 不盲目重放原操作。
                                page.retry_operation = None;
                                page.state_error = Some(error.public_message().to_owned());
                                page.status_is_warning = false;
                                page.status = Some(format!(
                                    "{}{}",
                                    crate::locale::text(
                                        "操作已提交，但读取状态失败；请刷新状态。",
                                        "The operation was committed, but its state could not be read. Refresh the state."
                                    ),
                                    warning
                                        .map(|value| format!(" {}", value.public_message()))
                                        .unwrap_or_default()
                                ));
                                page.reload_drop_in_state(cx);
                            } else if error.public_code() == PublicErrorCode::OutcomeUnknown {
                                // 未知提交不能当成确定失败重试；先回读状态，读回成功
                                // 才能重新开放写入口。
                                page.retry_operation = None;
                                page.state_error = Some(error.public_message().to_owned());
                                page.status_is_warning = false;
                                page.status = Some(error.public_message().to_owned());
                                page.reload_drop_in_state(cx);
                            } else if error.public_code() == PublicErrorCode::Conflict {
                                // 冻结的目录身份或 CAS 修订已经变化；用同一旧值重试
                                // 只会再次冲突。安装冲突刷新目录后重新确认，其余冲突
                                // 重新读取权威状态。
                                page.retry_operation = None;
                                page.status_is_warning = true;
                                page.status = Some(
                                    crate::locale::text(
                                        "状态已更改，请刷新后重新确认",
                                        "The state has changed; refresh and confirm again.",
                                    )
                                    .to_string(),
                                );
                                if matches!(retry_candidate, Operation::Install(_)) {
                                    page.refresh_catalog(false, cx);
                                } else {
                                    page.reload_drop_in_state(cx);
                                }
                            } else {
                                if !matches!(retry_candidate, Operation::Delete(_) | Operation::File(_, _) | Operation::Change(ExtensionChange::CancelFailedChange { .. }) | Operation::ConfirmDropIn { .. }) {
                                    page.retry_operation = Some(RetryOperation {
                                        operation: retry_candidate,
                                        feedback: retry_feedback,
                                    });
                                }
                                page.status_is_warning = false;
                                page.status = Some(error.public_message().to_owned());
                            }
                        }
                    }
                    Err(error) => {
                        if error.public_code() != PublicErrorCode::Cancelled {
                            // 执行器外层失败意味着写任务没有返回可判定的回执，提交是否
                            // 落地未知；与未知提交一样先停写并回读，不能凭错误文案重试。
                            page.retry_operation = None;
                            page.status = Some(error.public_message().to_owned());
                            page.status_is_warning = false;
                            page.state_error = Some(error.public_message().to_owned());
                            page.reload_drop_in_state(cx);
                        }
                    }
                }
                // 页面实体只重绘自身；父面板依赖写入状态控制提示显隐，必须显式通知。
                cx.emit(ExtensionsEvent::WriteFinished);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 单项与批量共用同一可更新判定，避免行内按钮和批量范围各说一套。
    ///
    /// 只有权威状态与目录快照都存在时才可能判断版本；可信的过期缓存仍是一份
    /// 已验签目录，不能因为刷新失败就机械地当成“未知”。pending、未确认投放、
    /// 不支持（含身份冲突）、非市场来源与目录中已无更高版本的记录都不进入候选。
    ///
    /// 目录安装固定以 `Official` 提交，安装器对其它来源返回冲突；这里提前排除，
    /// 行内、详情与批量都不会留下必然失败的“更新”入口。
    ///
    /// @author kongweiguang
    fn update_selection(&self, id: &PluginId) -> Option<CatalogSelection> {
        if self.state_error.is_some() {
            return None;
        }
        let state = self.state.as_ref()?;
        let installed = state.plugins.get(id)?;
        if state.pending.iter().any(|change| change.plugin_id() == id) {
            return None;
        }
        if matches!(installed.status, PluginInstallStatus::Unsupported { .. }) {
            return None;
        }
        if installed.source != PluginSource::Official {
            return None;
        }
        let snapshot = self.catalog.as_ref()?;
        let plugin = snapshot
            .catalog
            .plugins
            .iter()
            .find(|plugin| &plugin.id == id)?;
        let available = selection(plugin, &self.host_api, Some(&snapshot.identity))?;
        (available.version.as_semver() > installed.active.as_semver()).then_some(available)
    }

    /// 批量范围来自全部已安装记录，不受搜索、分类或内置组折叠影响。
    ///
    /// 候选按稳定身份排序，确认弹层与执行顺序可复现；名称与完全访问标记在
    /// 确认前冻结，执行阶段不再读取可能已变化的页面状态。
    ///
    /// @author kongweiguang
    fn update_candidates(&self) -> Vec<UpdateCandidate> {
        let Some(state) = self.state.as_ref() else {
            return Vec::new();
        };
        let Some(snapshot) = self.catalog.as_ref() else {
            return Vec::new();
        };
        state
            .plugins
            .iter()
            .filter_map(|(id, installed)| {
                let available = self.update_selection(id)?;
                let plugin = snapshot
                    .catalog
                    .plugins
                    .iter()
                    .find(|plugin| &plugin.id == id)?;
                let full_access = plugin
                    .versions
                    .iter()
                    .find(|version| version.version == available.version)
                    .is_some_and(|version| version.requires_full_access());
                Some(UpdateCandidate {
                    id: id.clone(),
                    name: label(&plugin.name, id.as_str()).to_owned(),
                    from: installed.active.to_string(),
                    selection: available,
                    full_access,
                })
            })
            .collect()
    }

    /// 打开批量确认：整份计划一次冻结，不逐项弹窗，也不在确认后重新挑选版本。
    ///
    /// 目录身份与逐项目标版本都来自同一份已验签快照；执行时安装器会按该身份
    /// 重读缓存，目录已变化就冲突并要求重新确认。离线与工具栏按钮保持同一门禁，
    /// 不能从状态弹层的重试入口绕过。
    ///
    /// @author kongweiguang
    fn open_batch_update(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.offline || self.state_error.is_some() || self.state.is_none() {
            return;
        }
        let items = self.update_candidates();
        if items.is_empty() {
            // 候选已被行内更新或目录刷新消化；重试入口必须随事实消失，
            // 不能留下点了没有反馈的死按钮。
            self.retry_batch = false;
            self.status_is_warning = true;
            self.status =
                Some(crate::locale::text("没有可更新的扩展", "No updates available").to_string());
            // 这条提示描述的是整页事实而不是上一个失败对象；不清归属会让旧对象
            // 的详情把“没有可更新的扩展”当成自己的操作结果。retry_operation
            // 仍指向可恢复的单项失败，保留它，避免顺手吞掉有效恢复入口。
            self.feedback_entry = None;
            cx.notify();
            return;
        }
        self.open_confirmation(Confirmation::BatchUpdate(BatchUpdatePlan { items }), cx);
    }

    /// 批量更新串行复用真实安装用例：每项完成即提交 pending，失败或未知就停在安全边界。
    ///
    /// 不新建运行时或通用批处理框架：取消令牌、进度通道、安装器身份复核与权威
    /// 回读都沿用单项路径。已提交项保留 pending；未开始项不提交；失败后只允许
    /// 重新计算剩余计划并重新确认，不能重放旧身份或重复已排队变更。
    ///
    /// @author kongweiguang
    fn run_batch_update(&mut self, plan: BatchUpdatePlan, cx: &mut Context<Self>) {
        if plan.items.is_empty() {
            return;
        }
        self.supersede_drop_in_inspection();
        self.busy = true;
        self.inspecting = false;
        self.writing = true;
        if !self.status_is_warning {
            self.status = None;
        }
        self.retry_operation = None;
        self.retry_batch = false;
        self.dialog_open = false;
        self.confirmation = None;
        self.feedback_entry = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let total = plan.items.len();
        self.progress = Some(InstallProgress::Preparing);
        self.batch = Some(BatchProgress {
            index: 0,
            total,
            id: plan.items[0].id.clone(),
        });
        self.active_install = Some(plan.items[0].id.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        let (progress_tx, mut progress_rx) =
            kerminal_application::extensions::install_progress_channel();
        cx.spawn(async move |this, cx| {
            while progress_rx.changed().await.is_ok() {
                let progress = progress_rx.borrow_and_update().clone();
                if this
                    .update(cx, |page, cx| {
                        if page.generation == generation && page.busy {
                            page.progress = Some(progress);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let mut committed: Vec<PluginId> = Vec::new();
            // 清理残留是磁盘事实，后续失败或取消都不能把它从终态反馈里抹掉。
            let mut warnings: Vec<String> = Vec::new();
            let mut failure: Option<BatchFailure> = None;
            let mut cancelled = false;
            for (index, item) in plan.items.iter().enumerate() {
                // 取消发生在两项之间时不再启动下一项；已提交项仍保留 pending。
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                // 页面已离开、被新代次取代或取消后不再启动新下载；弱引用失效时
                // 直接停止，不能继续用一个已经没有展示目标的页面推进安装。
                let current = this
                    .update(cx, |page, cx| {
                        if page.generation != generation || !page.busy || page.cancel.is_none() {
                            return false;
                        }
                        page.batch = Some(BatchProgress {
                            index,
                            total,
                            id: item.id.clone(),
                        });
                        page.active_install = Some(item.id.clone());
                        page.progress = Some(InstallProgress::Preparing);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
                let selection = item.selection.clone();
                let task_service = service.clone();
                let task_progress = progress_tx.clone();
                let outcome = executor
                    .run_cancellable(cancel.clone(), move |token| async move {
                        task_service
                            .install(selection, task_progress, token)
                            .await
                    })
                    .await;
                match outcome {
                    Ok(receipt) => {
                        if let Some(warning) = receipt.cleanup_warning {
                            warnings.push(format!("{}: {}", item.name, warning.public_message()));
                        }
                        committed.push(item.id.clone());
                        // 每项提交后立即回读权威状态；读失败就停写，不能带着
                        // 未知状态启动下一项，也不能把失败项当成未提交。
                        let read_service = service.clone();
                        let state_result = executor
                            .run(move |_| async move { read_service.state().await })
                            .await;
                        match state_result {
                            Ok(state) => {
                                let applied = this
                                    .update(cx, |page, cx| {
                                        if page.generation != generation || !page.busy {
                                            return false;
                                        }
                                        page.state = Some(state);
                                        page.state_error = None;
                                        cx.notify();
                                        true
                                    })
                                    .unwrap_or(false);
                                if !applied {
                                    break;
                                }
                            }
                            Err(error) => {
                                if error.public_code() == PublicErrorCode::Cancelled {
                                    cancelled = true;
                                } else {
                                    failure = Some(BatchFailure {
                                        id: item.id.clone(),
                                        name: item.name.clone(),
                                        reason: error.public_message().to_owned(),
                                        unknown: true,
                                        committed: true,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::Cancelled {
                            cancelled = true;
                        } else {
                            // 执行器外层失败没有可判定回执，提交是否落地未知；
                            // 与单项路径一样先回读状态，不能凭错误文案重试。
                            failure = Some(BatchFailure {
                                id: item.id.clone(),
                                name: item.name.clone(),
                                reason: error.public_message().to_owned(),
                                unknown: error.public_code() == PublicErrorCode::OutcomeUnknown,
                                committed: false,
                            });
                        }
                        break;
                    }
                }
            }
            // 收尾再回读一次：失败、取消与逐项回读失败都可能留下未落地的状态；
            // 成功路径的逐项回读已证明权威状态，这里统一收尾并尝试恢复。
            let state_service = service.clone();
            let state_result = executor
                .run(move |_| async move { state_service.state().await })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.busy = false;
                page.writing = false;
                page.cancel = None;
                page.progress = None;
                page.batch = None;
                page.active_install = None;
                let state_ok = match state_result {
                    Ok(state) => {
                        page.state = Some(state);
                        page.state_error = None;
                        true
                    }
                    Err(error) => {
                        if error.public_code() != PublicErrorCode::Cancelled {
                            page.state_error = Some(error.public_message().to_owned());
                        }
                        false
                    }
                };
                let committed_count = committed.len();
                let status = if let Some(failure) = failure {
                    // 失败项本身不算“未处理”；已提交但回读失败的项已经计入
                    // committed，剩余未开始项单独计数，反馈不夸大进度。
                    let unprocessed = if failure.committed {
                        total.saturating_sub(committed_count)
                    } else {
                        total.saturating_sub(committed_count + 1)
                    };
                    // 状态未知时只允许刷新；回读成功且仍有候选才重新开放重新规划，
                    // 避免重试入口指向一份已经全部完成的空计划。
                    page.retry_batch = state_ok && !page.update_candidates().is_empty();
                    page.status_is_warning = false;
                    if failure.committed {
                        let recovery = if state_ok {
                            crate::locale::text("已重新读取状态。", "The state was re-read.")
                        } else {
                            crate::locale::text("请刷新状态。", "Refresh the state.")
                        };
                        crate::locale::text(
                            format!(
                                "更新「{}」（{}）已提交，但读取状态失败：{}。{recovery} 已更新 {committed_count} 个，{unprocessed} 个未处理。",
                                failure.name,
                                failure.id.as_str(),
                                failure.reason
                            ),
                            format!(
                                "“{}” ({}) was committed, but its state could not be read: {}. {recovery} {committed_count} updated, {unprocessed} unprocessed.",
                                failure.name,
                                failure.id.as_str(),
                                failure.reason
                            ),
                        )
                        .to_string()
                    } else if failure.unknown {
                        if state_ok {
                            crate::locale::text(
                                format!(
                                    "更新「{}」（{}）结果未知，已停止并重新读取状态。已更新 {committed_count} 个，{unprocessed} 个未处理。",
                                    failure.name,
                                    failure.id.as_str()
                                ),
                                format!(
                                    "The outcome of “{}” ({}) is unknown; stopped and re-read the state. {committed_count} updated, {unprocessed} unprocessed.",
                                    failure.name,
                                    failure.id.as_str()
                                ),
                            )
                            .to_string()
                        } else {
                            crate::locale::text(
                                format!(
                                    "更新「{}」（{}）结果未知，且状态回读失败；请刷新状态。已更新 {committed_count} 个，{unprocessed} 个未处理。",
                                    failure.name,
                                    failure.id.as_str()
                                ),
                                format!(
                                    "The outcome of “{}” ({}) is unknown and the state could not be re-read. Refresh the state. {committed_count} updated, {unprocessed} unprocessed.",
                                    failure.name,
                                    failure.id.as_str()
                                ),
                            )
                            .to_string()
                        }
                    } else {
                        crate::locale::text(
                            format!(
                                "批量更新已停止：「{}」（{}）失败：{}。已更新 {committed_count} 个，{unprocessed} 个未处理。",
                                failure.name,
                                failure.id.as_str(),
                                failure.reason
                            ),
                            format!(
                                "The batch update stopped: “{}” ({}) failed: {}. {committed_count} updated, {unprocessed} unprocessed.",
                                failure.name,
                                failure.id.as_str(),
                                failure.reason
                            ),
                        )
                        .to_string()
                    }
                } else if cancelled {
                    let unprocessed = total.saturating_sub(committed_count);
                    // 取消后仍以权威状态重新计算候选：若未开始项已被外部提交，
                    // 重试入口不能指向空计划。
                    page.retry_batch = state_ok && !page.update_candidates().is_empty();
                    page.status_is_warning = true;
                    crate::locale::text(
                        format!(
                            "已取消批量更新：已更新 {committed_count} 个，{unprocessed} 个未处理。已提交的变更重启后生效。"
                        ),
                        format!(
                            "Batch update cancelled: {committed_count} updated, {unprocessed} unprocessed. Committed changes take effect after restart."
                        ),
                    )
                    .to_string()
                } else if state_ok {
                    page.retry_batch = false;
                    page.status_is_warning = true;
                    crate::locale::text(
                        format!("已更新 {committed_count} 个扩展 · 重启后生效"),
                        format!("Updated {committed_count} extensions · Restart required"),
                    )
                    .to_string()
                } else {
                    // 全部提交但回读失败：主文案必须与单项路径一致，不能只报成功。
                    page.retry_batch = false;
                    page.status_is_warning = false;
                    crate::locale::text(
                        format!("已更新 {committed_count} 个扩展，但状态回读失败；请刷新状态。"),
                        format!(
                            "Updated {committed_count} extensions, but the state could not be re-read. Refresh the state."
                        ),
                    )
                    .to_string()
                };
                page.status = Some(append_cleanup_warnings(status, &warnings));
                // 写入改变磁盘记录；重读观察才能更新指纹与待确认状态。
                page.reload_drop_in_state(cx);
                cx.emit(ExtensionsEvent::WriteFinished);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 确认冲突或确认成功后重新读取权威状态与 drop-in 观察。
    ///
    /// 状态与观察分别执行并分别记录失败：状态仓储暂时不可读时仍可更新磁盘观察，
    /// 观察失败也不能丢弃刚回读的权威状态。只刷新数据，不覆盖调用方刚写入的提示。
    ///
    /// @author kongweiguang
    fn reload_drop_in_state(&mut self, cx: &mut Context<Self>) {
        // 页面已离开时观察没有展示目标；下次 activate 会重新读取，不能把 busy 留在无人页面。
        if self.busy || !self.active {
            return;
        }
        self.busy = true;
        self.reloading_drop_ins = true;
        self.inspecting = false;
        self.supersede_drop_in_inspection();
        self.state_generation = self.state_generation.wrapping_add(1);
        let state_generation = self.state_generation;
        let drop_in_generation = self.drop_in_generation;
        let token = CancellationToken::new();
        self.drop_in_cancel = Some(token.clone());
        let state_service = self.service.clone();
        let state_executor = self.executor.clone();
        let inspect_service = self.service.clone();
        let inspect_executor = self.executor.clone();
        // 状态与观察各自独立任务，先完成的一侧立即落地；计数只负责在两者都结束后
        // 释放读盘忙态，避免慢的一侧把已到达的结果压在同一帧里。
        let pending = Rc::new(Cell::new(2u8));
        let state_pending = pending.clone();
        cx.spawn(async move |this, cx| {
            let state_result = state_executor
                .run(move |_| async move { state_service.state().await })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.state_generation != state_generation {
                    return;
                }
                match state_result {
                    Ok(state) => {
                        page.state = Some(state);
                        page.state_error = None;
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.state_error = Some(error.public_message().to_owned());
                    }
                }
                page.prune_selection(cx);
                finish_drop_in_reload(page, &state_pending, cx);
                cx.notify();
            });
        })
        .detach();
        let inspect_pending = pending.clone();
        cx.spawn(async move |this, cx| {
            let inspections_result = inspect_executor
                .run_cancellable(token, move |cancel| async move {
                    inspect_service.inspect_drop_ins(cancel).await
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.drop_in_generation != drop_in_generation {
                    return;
                }
                page.drop_in_cancel = None;
                match inspections_result {
                    Ok(inspections) => {
                        page.drop_ins = inspections;
                        page.drop_in_error = None;
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
                        page.drop_in_error = Some(error.public_message().to_owned());
                    }
                }
                page.prune_selection(cx);
                finish_drop_in_reload(page, &inspect_pending, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 验收读取真实页面和服务回执投影，不能用构造数据替代安装结果。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn acceptance_snapshot(&self) -> ExtensionsAcceptanceSnapshot {
        // 组顺序与计数直接取自渲染同源的列表条目，验收不需要另写一套分组规则。
        let plugins = self
            .catalog
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.catalog.plugins.as_slice());
        let groups = self
            .list_entries(plugins, "")
            .into_iter()
            .filter_map(|entry| match entry {
                render::ListEntry::Header { group, count } => Some(ExtensionGroupSnapshot {
                    kind: group.acceptance_kind(),
                    count,
                    collapsed: group == render::RowGroup::Builtin && !self.builtin_expanded,
                }),
                render::ListEntry::Row(_) => None,
            })
            .collect();
        let batch_items = match self.confirmation.as_ref() {
            Some(Confirmation::BatchUpdate(plan)) => plan
                .items
                .iter()
                .map(|item| {
                    format!(
                        "{} {}→{}",
                        item.id.as_str(),
                        item.from,
                        item.selection.version
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        ExtensionsAcceptanceSnapshot {
            catalog_entries: self
                .catalog
                .as_ref()
                .map_or(0, |value| value.catalog.plugins.len()),
            installed: installed_total(self.state.as_ref(), &self.builtin_units),
            pending: self.state.as_ref().map_or(0, |value| value.pending.len()),
            offline: self.offline,
            busy: self.busy,
            selected: self
                .selected
                .as_ref()
                .map(|entry| entry.id().as_str().to_owned()),
            error: self
                .status
                .clone()
                .or_else(|| self.catalog_error.clone())
                .or_else(|| self.state_error.clone())
                .or_else(|| self.drop_in_error.clone())
                .or_else(|| self.readme_error.clone()),
            catalog_error: self.catalog_error.clone(),
            state_error: self.state_error.clone(),
            readme_error: self.readme_error.clone(),
            confirmation_visible: self.confirmation.is_some(),
            confirmation_kind: self
                .confirmation
                .as_ref()
                .map(|confirmation| match confirmation {
                    Confirmation::File(_, _) => "file",
                    Confirmation::Install(_) => "install",
                    Confirmation::BatchUpdate(_) => "batch_update",
                    Confirmation::DeleteData(_) => "delete_data",
                    Confirmation::DropIn(_) => "drop_in",
                }),
            confirmation_id: self.confirmation.as_ref().map(|confirmation| {
                match confirmation {
                    Confirmation::File(_, inspection) => inspection.id.as_str(),
                    Confirmation::Install(selection) => selection.id.as_str(),
                    Confirmation::BatchUpdate(plan) => {
                        plan.items.first().map_or("", |item| item.id.as_str())
                    }
                    Confirmation::DeleteData(id) => id.as_str(),
                    Confirmation::DropIn(inspection) => inspection.id.as_str(),
                }
                .to_owned()
            }),
            confirmation_permissions: self.confirmation.as_ref().map(|confirmation| {
                match confirmation {
                    Confirmation::File(_, inspection) => inspection.permissions.clone(),
                    Confirmation::Install(selection) => self
                        .catalog
                        .as_ref()
                        .and_then(|catalog| {
                            catalog
                                .catalog
                                .plugins
                                .iter()
                                .find(|plugin| plugin.id == selection.id)
                        })
                        .map_or_else(Vec::new, |plugin| plugin.permissions.clone()),
                    Confirmation::BatchUpdate(_) => Vec::new(),
                    Confirmation::DeleteData(_) => Vec::new(),
                    Confirmation::DropIn(inspection) => inspection.permissions.clone(),
                }
            }),
            pending_restart: self
                .state
                .as_ref()
                .is_some_and(|state| !state.pending.is_empty()),
            content_width: self.content_width.get().map(|width| width.as_f32()),
            back_to_list: self.back_to_list,
            groups,
            update_candidates: self.update_candidates().len(),
            batch_items,
            batch_progress: self
                .batch
                .as_ref()
                .map(|batch| format!("{}/{} {}", batch.index + 1, batch.total, batch.id.as_str())),
            // 来源投影只读真实页面与运行态：验收需要证明点击保存后确实经过回读
            // 并切换了 service 来源，不能用注入值或伪成功替代。
            source_url: self.service.source_url(),
            source_dialog_open: self.source_dialog_open,
            source_saving: self.source_saving,
            source_error: self.source_error.clone(),
        }
    }

    /// 只有写操作会改变插件目录和待重启集合；只读加载不应阻止退出。
    pub fn is_writing(&self) -> bool {
        self.writing
    }

    /// 用户取消安装时只取消可取消的后台任务，已提交结果仍由应用层回读与收尾。
    pub fn cancel(&mut self) {
        if let Some(token) = &self.cancel {
            token.cancel();
        }
    }

    /// 目录刷新有独立取消令牌，不能借用安装取消而让两个操作互相干扰。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_refresh(&mut self) {
        if let Some(token) = &self.catalog_cancel {
            token.cancel();
        }
    }

    /// 初始缓存读取不接受取消：`cached_catalog` 没有取消参数，显示按钮只会造成
    /// “点了没反应”的假入口。只有进入网络刷新阶段才暴露取消。
    fn can_cancel_catalog_refresh(&self) -> bool {
        self.refreshing && !self.initial_catalog_loading && self.catalog_cancel.is_some()
    }

    /// 页头“商店来源…”入口：先按当前生效来源回填输入，再延迟打开窗口级弹层。
    ///
    /// 来源编辑会触发配置写入，必须与安装、批量更新和目录读取互斥；弹层打开期间
    /// 页面仍可重绘，但不能让后台回执把用户正在编辑的输入替换掉。
    ///
    /// @author kongweiguang
    fn open_source_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy
            || self.writing
            || self.source_saving
            || self.inspecting
            || self.batch.is_some()
            || self.dialog_open
            || self.confirmation.is_some()
            || self.source_dialog_open
        {
            return;
        }
        let current = self.service.source_url();
        // 官方地址回显为空：界面用“留空 = 官方”表达默认，避免用户误以为必须填长 URL。
        let value = if current == DEFAULT_PLUGIN_CATALOG_URL {
            String::new()
        } else {
            current
        };
        self.source_input.update(cx, |input, cx| {
            input.set_value(value, window, cx);
        });
        // 来源保存在离开分类后仍可能回读失败，原因只能挂在弹层里；重开时保留
        // 这份失败原因，用户在输入框实际修改或重新提交时才清除。这里只丢弃
        // 旧的 pending 提交标记，失败回执本身不会重放，也不会复用旧地址。
        self.source_pending = None;
        self.source_dialog_open = true;
        self.source_dialog_generation = self.source_dialog_generation.wrapping_add(1);
        let generation = self.source_dialog_generation;
        let owner = cx.entity().downgrade();
        let window_handle = self.window_handle;
        // 延迟到下一个应用循环再打开，保证设置窗口与页面实体都已处于稳定状态。
        cx.defer(move |app| {
            let _ = window_handle.update(app, |_, window, app| {
                if let Some(page) = owner.upgrade() {
                    page.update(app, |page, cx| {
                        if page.source_dialog_open && page.source_dialog_generation == generation {
                            page.present_source_dialog(window, cx);
                        }
                    });
                }
            });
        });
        cx.notify();
    }

    /// 使用窗口级 Kit Dialog 编辑目录地址；焦点、Escape 与关闭后的焦点恢复由 Kit 负责。
    ///
    /// 弹层内容每次重建都从页面读取保存态与错误，保存回执到达后无需重新打开；
    /// 保存中禁用取消和 Escape，避免用户看不到最终结果。
    ///
    /// @author kongweiguang
    fn present_source_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active || !self.source_dialog_open {
            self.source_dialog_open = false;
            return;
        }
        let owner = cx.entity().downgrade();
        let input = self.source_input.clone();
        let cancel_owner = owner.clone();
        let save_owner = owner.clone();
        let restore_owner = owner.clone();
        let esc_owner = owner.clone();
        window.open_dialog(cx, move |dialog, window, app| {
            let theme = ThemeTokens::current(window, app).for_settings();
            // 每次重建都读取页面当前状态：保存中、失败原因与当前来源都随回执更新。
            let (saving, error, current) = owner
                .upgrade()
                .map(|page| {
                    let page = page.read(app);
                    (
                        page.source_saving,
                        page.source_error.clone(),
                        page.service.source_url(),
                    )
                })
                .unwrap_or((false, None, String::new()));
            let cancel_owner = cancel_owner.clone();
            let save_owner = save_owner.clone();
            let restore_owner = restore_owner.clone();
            let esc_owner = esc_owner.clone();
            let input = input.clone();
            dialog
                .centered()
                .width(px(520.))
                .bg(theme.panel)
                .close_button(false)
                .overlay_closable(false)
                .title(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(crate::locale::text("商店来源", "Extension store source")),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "留空使用官方来源。支持 HTTP(S) 地址，可指向局域网或本机服务。",
                                    "Leave empty to use the official source. HTTP(S) URLs, including LAN or local services, are supported.",
                                )),
                        )
                        .child(
                            Input::new(&input)
                                .cleanable(true)
                                // 保存中冻结输入：回执只会应用提交时的地址，继续编辑
                                // 会在弹层关闭时被丢弃，用户会误以为新输入已生效。
                                .disabled(saving)
                                .bg(theme.field_surface())
                                .border_color(theme.strong_border())
                                .rounded(theme.control_radius)
                                .w_full(),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    format!("当前来源 · {current}"),
                                    format!("Current source · {current}"),
                                )),
                        )
                        .when_some(error, |body, error| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.error)
                                    .child(error),
                            )
                        }),
                )
                .footer(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            Button::new("extensions-source-restore")
                                .ghost()
                                .label(crate::locale::text(
                                    "恢复官方默认",
                                    "Restore official default",
                                ))
                                .disabled(saving)
                                .debug_selector(|| "extensions-source-restore".into())
                                .on_click(move |_, window, app| {
                                    if let Some(page) = restore_owner.upgrade() {
                                        page.update(app, |page, cx| {
                                            page.source_input.update(cx, |input, cx| {
                                                input.set_value("", window, cx);
                                            });
                                            page.request_source_save(cx);
                                        });
                                    }
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Button::new("extensions-source-cancel")
                                        .label(crate::locale::text("取消", "Cancel"))
                                        .disabled(saving)
                                        .debug_selector(|| "extensions-source-cancel".into())
                                        .on_click(move |_, window, app| {
                                            // 与确认弹层一致：按钮先同步关闭窗口级弹层，
                                            // 再只清理页面状态，避免延迟关闭误伤后续弹层。
                                            window.close_dialog(app);
                                            if let Some(page) = cancel_owner.upgrade() {
                                                page.update(app, |page, cx| {
                                                    page.clear_source_dialog(cx)
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("extensions-source-save")
                                        .primary()
                                        .label(if saving {
                                            crate::locale::text("保存中…", "Saving…")
                                        } else {
                                            crate::locale::text("保存", "Save")
                                        })
                                        .disabled(saving)
                                        .debug_selector(|| "extensions-source-save".into())
                                        .on_click(move |_, _, app| {
                                            if let Some(page) = save_owner.upgrade() {
                                                page.update(app, |page, cx| {
                                                    page.request_source_save(cx)
                                                });
                                            }
                                        }),
                                ),
                        ),
                )
                .on_cancel(move |_, _, app| {
                    // 保存中按 Escape 不能关闭弹层，否则回执到达时用户看不到结果；
                    // 空闲时由 Kit 关闭弹层，这里只清理页面状态，焦点由 Kit 恢复。
                    let saving = esc_owner
                        .upgrade()
                        .is_some_and(|page| page.read(app).source_saving);
                    if saving {
                        return false;
                    }
                    if let Some(page) = esc_owner.upgrade() {
                        page.update(app, |page, cx| page.clear_source_dialog(cx));
                    }
                    true
                })
        });
        // Kit 打开时只聚焦弹层容器；把焦点交给 URL 输入，用户可直接粘贴地址。
        let focus = self.source_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// 提交来源保存：页面只做忙碌门禁和去空白，URL 校验与落盘交给设置面板。
    ///
    /// 事件延迟到当前更新结束再发，订阅方会替换设置草稿并启动保存，不能在页面
    /// 自身事件分发中重入更新。保存期间保留输入与当前生效来源，失败由面板回执。
    ///
    /// @author kongweiguang
    fn request_source_save(&mut self, cx: &mut Context<Self>) {
        if self.source_saving
            || self.busy
            || self.writing
            || self.inspecting
            || self.batch.is_some()
        {
            return;
        }
        let url = self.source_input.read(cx).value().trim().to_owned();
        self.source_saving = true;
        self.source_error = None;
        self.source_pending = Some(url.clone());
        let owner = cx.entity().downgrade();
        cx.defer(move |app| {
            if let Some(page) = owner.upgrade() {
                page.update(app, |page, cx| {
                    // 用户可能在延迟窗口内关闭或重新打开弹层；只发送仍属当前提交的请求。
                    if page.source_pending.as_deref() == Some(url.as_str()) {
                        cx.emit(ExtensionsEvent::CatalogSourceSaveRequested { url: url.clone() });
                    }
                });
            }
        });
        cx.notify();
    }

    /// 设置保存成功且快照确实包含本次 URL 后才切换运行态来源。
    ///
    /// 切换前先作废旧目录代次与在途读取，清空旧选择、README、确认和重试，
    /// 防止旧来源的迟到回执写进新来源界面；随后强制刷新一次新来源。
    ///
    /// @author kongweiguang
    pub(crate) fn apply_catalog_source(&mut self, url: &str, cx: &mut Context<Self>) {
        // 只接受当前提交的回执；旧保存结果不能把用户刚切换的来源改回去。
        if self.source_pending.as_deref() != Some(url) {
            return;
        }
        match self.service.set_source_url(url) {
            Ok(()) => {
                self.source_saving = false;
                self.source_pending = None;
                self.source_error = None;
                self.close_source_dialog(cx);
                self.invalidate_for_source_change(cx);
                // 离开分类时只切换运行态来源，不启动不可见的网络刷新；
                // 下次 activate 会按新来源重新读取缓存并刷新。
                if self.active {
                    self.refresh_catalog(false, cx);
                }
                cx.notify();
            }
            Err(error) => {
                // 面板已按同一规则校验；这里仍保留失败分支，避免端口拒绝时静默伪成功。
                self.source_saving = false;
                self.source_pending = None;
                self.source_error = Some(error.public_message().to_owned());
                cx.notify();
            }
        }
    }

    /// 设置保存失败时保留弹层与输入，只显示原因；当前生效来源保持不变。
    ///
    /// @author kongweiguang
    pub(crate) fn fail_catalog_source_save(&mut self, message: String, cx: &mut Context<Self>) {
        self.source_saving = false;
        self.source_pending = None;
        self.source_error = Some(message);
        cx.notify();
    }

    /// 来源切换后旧目录及其派生状态全部属于旧来源，必须整体失效。
    ///
    /// 权威安装状态与 drop-in 观察来自磁盘，与目录地址无关，保留它们避免无关重读。
    /// 目录刷新由调用方在页面可见时重新发起。
    ///
    /// @author kongweiguang
    fn invalidate_for_source_change(&mut self, cx: &mut Context<Self>) {
        if let Some(token) = self.catalog_cancel.take() {
            token.cancel();
        }
        self.catalog_generation = self.catalog_generation.wrapping_add(1);
        self.catalog = None;
        self.offline = false;
        self.catalog_error = None;
        self.refreshing = false;
        self.initial_catalog_loading = false;
        if let Some(token) = self.readme_cancel.take() {
            token.cancel();
        }
        self.readme_generation = self.readme_generation.wrapping_add(1);
        self.readme = None;
        self.readme_error = None;
        self.readme_attempted = false;
        self.selected = None;
        self.filter.category = None;
        self.back_to_list = false;
        self.retry_operation = None;
        self.retry_batch = false;
        self.feedback_entry = None;
        self.status = None;
        self.status_is_warning = false;
        self.confirmation = None;
        self.dialog_open = false;
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        // 来源切换入口在 busy/writing 时禁用；这里仍释放只读忙态，避免旧读取因
        // 代次失配提前返回后把页面永久留在忙碌状态。写入忙态不能在这里释放。
        if !self.writing {
            self.busy = false;
        }
        cx.notify();
    }

    /// 关闭来源弹层：先作废未执行的打开回调，再延迟关闭窗口级 Dialog。
    ///
    /// 与确认弹层一样延迟到当前更新结束，避免在页面更新期间重入窗口状态。
    ///
    /// @author kongweiguang
    fn close_source_dialog(&mut self, cx: &mut Context<Self>) {
        let was_open = self.source_dialog_open;
        self.source_dialog_open = false;
        self.source_dialog_generation = self.source_dialog_generation.wrapping_add(1);
        if was_open {
            let window_handle = self.window_handle;
            cx.defer(move |app| {
                let _ = window_handle.update(app, |_, window, cx| window.close_dialog(cx));
            });
        }
        cx.notify();
    }

    /// 按钮或 Escape 已由 Kit 关闭弹层，这里只清理页面状态并作废延迟打开。
    ///
    /// 不再延迟 `close_dialog`：重复关闭会误伤随后打开的另一个窗口级弹层。
    ///
    /// @author kongweiguang
    fn clear_source_dialog(&mut self, cx: &mut Context<Self>) {
        self.source_dialog_open = false;
        self.source_dialog_generation = self.source_dialog_generation.wrapping_add(1);
        cx.notify();
    }

    /// 打开确认的唯一入口：先记录待执行意图，再交给窗口级 Kit Dialog，页面本身不绘制遮罩。
    fn open_confirmation(&mut self, confirmation: Confirmation, cx: &mut Context<Self>) {
        // 页面已离开时，迟到的预检结果只能回到后台收尾，不能弹出窗口级确认。
        if !self.active {
            self.close_open_confirmation(cx);
            return;
        }
        if self.dialog_open {
            // 旧弹层与新弹层必须在同一个延迟窗口内先关后开，避免两个 defer 的执行顺序决定结果。
            self.confirmation = Some(confirmation);
            self.dialog_open = true;
            self.dialog_generation = self.dialog_generation.wrapping_add(1);
            let generation = self.dialog_generation;
            let owner = cx.entity().downgrade();
            let window_handle = self.window_handle;
            cx.defer(move |app| {
                let _ = window_handle.update(app, |_, window, app| {
                    window.close_dialog(app);
                    if let Some(page) = owner.upgrade() {
                        page.update(app, |page, cx| {
                            if page.dialog_open && page.dialog_generation == generation {
                                page.present_confirmation(window, cx);
                            }
                        });
                    }
                });
            });
            cx.notify();
            return;
        }
        self.confirmation = Some(confirmation);
        self.dialog_open = true;
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        let generation = self.dialog_generation;
        let owner = cx.entity().downgrade();
        let window_handle = self.window_handle;
        // 延迟到下一个应用循环再打开，保证设置窗口与页面实体都已处于稳定状态。
        cx.defer(move |app| {
            let _ = window_handle.update(app, |_, window, app| {
                if let Some(page) = owner.upgrade() {
                    page.update(app, |page, cx| {
                        if page.dialog_open && page.dialog_generation == generation {
                            page.present_confirmation(window, cx);
                        }
                    });
                }
            });
        });
        cx.notify();
    }

    /// 关闭确认只清理页面状态，业务操作由调用方另行决定；焦点恢复由 Kit Dialog 负责。
    fn clear_confirmation(&mut self, cx: &mut Context<Self>) {
        self.confirmation = None;
        self.dialog_open = false;
        cx.notify();
    }

    /// 按钮确认先关闭窗口级弹层再执行同一操作，避免页面状态与弹层生命周期分叉。
    fn finish_confirmation(&mut self, operation: Option<Operation>, cx: &mut Context<Self>) {
        self.confirmation = None;
        self.dialog_open = false;
        if let Some(operation) = operation {
            self.run_operation(operation, cx);
        } else {
            cx.notify();
        }
    }

    /// 页面重新激活时关闭可能遗留的确认；先用代次作废未执行的打开回调，再延迟关闭，避免窗口重入。
    fn close_open_confirmation(&mut self, cx: &mut Context<Self>) {
        let was_open = self.dialog_open;
        self.confirmation = None;
        self.dialog_open = false;
        self.dialog_generation = self.dialog_generation.wrapping_add(1);
        if was_open {
            let window_handle = self.window_handle;
            cx.defer(move |app| {
                let _ = window_handle.update(app, |_, window, cx| window.close_dialog(cx));
            });
        }
        cx.notify();
    }

    /// 目录替换后留下的选择会展示另一份目录的旧详情，必须按当前目录与安装集合重新核对。
    fn prune_selection(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.as_ref() else {
            return;
        };
        let id = entry.id();
        let in_catalog = self.catalog.as_ref().is_some_and(|catalog| {
            catalog
                .catalog
                .plugins
                .iter()
                .any(|plugin| &plugin.id == id)
        });
        let in_store = self.state.as_ref().is_some_and(|state| {
            state.plugins.contains_key(id)
                || state.pending.iter().any(|change| change.plugin_id() == id)
        });
        // 内置行与磁盘记录可能共用身份，剪枝必须按选择来源判断，不能把仅存于另一来源的选择清掉。
        let in_builtin = self.builtin_units.iter().any(|unit| &unit.id == id);
        let valid = match entry {
            SelectedEntry::Builtin(_) => in_builtin,
            SelectedEntry::Plugin(_) => in_catalog || in_store,
        };
        if valid {
            return;
        }
        if let Some(token) = self.readme_cancel.take() {
            token.cancel();
        }
        self.selected = None;
        self.readme = None;
        self.readme_error = None;
        self.readme_generation = self.readme_generation.wrapping_add(1);
        cx.notify();
    }

    /// 目录替换后分类筛选必须收敛到新目录；选择仍由 `prune_selection` 单独核对。
    fn reconcile_catalog_change(&mut self, cx: &mut Context<Self>) {
        let Some(catalog) = self.catalog.as_ref() else {
            return;
        };
        // 目录换代后旧的 icon_url 结果不再可信：清空市场图标缓存、推进代次并取消
        // 在途队列。已安装图标按版本键控，不会被这次换代误丢。
        self.catalog_icons.clear();
        self.icon_generation = self.icon_generation.wrapping_add(1);
        if let Some(token) = self.icon_cancel.take() {
            token.cancel();
        }
        let category_removed = self.filter.category.as_ref().is_some_and(|category| {
            !catalog
                .catalog
                .plugins
                .iter()
                .any(|plugin| plugin.categories.contains(category))
        });
        if category_removed {
            self.filter.category = None;
            cx.notify();
        }
    }

    /// 列表和详情共用同一受信目录记录，避免使用未校验的界面 URL 作为安装目标。
    fn selected_plugin(&self) -> Option<&CatalogPlugin> {
        // 内置详情和未确认投放记录都有自己的详情分支；只有普通 Plugin 选择才允许目录条目优先。
        let SelectedEntry::Plugin(id) = self.selected.as_ref()? else {
            return None;
        };
        // 未确认的手动投放即使与目录同 id，也必须先展示确认入口；目录行同步隐藏，避免两个入口竞争。
        if unconfirmed_drop_in(self.state.as_ref(), id).is_some() {
            return None;
        }
        self.catalog.as_ref().and_then(|catalog| {
            catalog
                .catalog
                .plugins
                .iter()
                .find(|plugin| &plugin.id == id)
        })
    }

    /// 内置详情只在选择来自内置行时成立；同身份记录行不能借用它显示启停按钮。
    fn selected_builtin_unit(&self) -> Option<&BuiltinUnit> {
        let SelectedEntry::Builtin(id) = self.selected.as_ref()? else {
            return None;
        };
        self.builtin_units.iter().find(|unit| &unit.id == id)
    }

    /// 冲突记录详情只在选择来自记录行时成立，避免遮蔽同名内置行的启用入口。
    fn selected_reserved_record(&self) -> Option<(&PluginId, &InstalledPlugin)> {
        let SelectedEntry::Plugin(id) = self.selected.as_ref()? else {
            return None;
        };
        let installed = self.state.as_ref()?.plugins.get(id)?;
        is_reserved_identity(installed).then_some((id, installed))
    }

    /// 行高亮按完整选择来源比较；同 id 的内置行与记录行不能同时选中。
    fn row_selected(&self, entry: &SelectedEntry) -> bool {
        self.selected.as_ref() == Some(entry)
    }

    /// 当前可见条目与渲染同源，组标题和行的序号一起参与键盘滚动定位。
    ///
    /// 统一列表里每个非空组都有标题；只按行序号滚动会按标题数量偏移，
    /// 因此键盘路径直接消费 `list_entries`，不再维护另一套标题计数。
    ///
    /// @author kongweiguang
    fn keyboard_entries(&self, cx: &App) -> Vec<render::ListEntry> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let plugins = self
            .catalog
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.catalog.plugins.as_slice());
        self.list_entries(plugins, &query)
    }

    /// 方向键在可见行之间移动；没有当前选择时从列表头或尾进入。
    ///
    /// 滚动目标使用条目在滚动容器中的真实子元素序号，组标题与折叠后的内置组
    /// 都自然计入。窄窗里只移动高亮，不打开详情，保证后续方向键仍由列表接收。
    /// `detail_visible` 由调用方按真实窗口宽度给出，决定是否值得为高亮行读取 README。
    fn move_selection(&mut self, delta: isize, detail_visible: bool, cx: &mut Context<Self>) {
        let entries = self.keyboard_entries(cx);
        let rows = row_entry_indices(&entries);
        if rows.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|selected| {
            rows.iter().position(|&index| {
                matches!(&entries[index], render::ListEntry::Row(row) if &row.selection == selected)
            })
        });
        let next = match (current, delta.is_negative()) {
            (Some(index), false) => (index + 1).min(rows.len() - 1),
            (Some(index), true) => index.saturating_sub(1),
            (None, false) => 0,
            (None, true) => rows.len() - 1,
        };
        let entry_index = rows[next];
        let render::ListEntry::Row(row) = &entries[entry_index] else {
            return;
        };
        let selection = row.selection.clone();
        self.select_entry(selection, false, detail_visible, cx);
        // 最小滚动：目标已可见时不动列表，避免每次按键都把列表顶到目标行。
        self.list_scroll.scroll_to_item(entry_index);
    }

    /// Home/End 跳到当前可见列表的首尾，语义与方向键一致，并确保目标进入视口。
    fn select_edge(&mut self, last: bool, detail_visible: bool, cx: &mut Context<Self>) {
        let entries = self.keyboard_entries(cx);
        let rows = row_entry_indices(&entries);
        let position = if last {
            rows.len().checked_sub(1)
        } else {
            (!rows.is_empty()).then_some(0)
        };
        let Some(position) = position else {
            return;
        };
        let entry_index = rows[position];
        let render::ListEntry::Row(row) = &entries[entry_index] else {
            return;
        };
        let selection = row.selection.clone();
        self.select_entry(selection, false, detail_visible, cx);
        self.list_scroll.scroll_to_item(entry_index);
    }

    /// 列表键盘路径：仅当列表自身持焦时处理方向键/Home/End 与 Enter。
    ///
    /// 行本身不再使用会抢焦点的 Button，焦点留在列表容器上，因此键盘序列不会
    /// 因为鼠标点击过的行而中断。行内按钮冒泡的按键不在这里消费，按钮自己的
    /// Enter/Space 仍能执行安装、更新或取消。列表已可见时 Escape 交给 Root
    /// 关闭设置页，页面不再把它吞成无效操作。
    ///
    /// @author kongweiguang
    fn handle_list_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        // 行内按钮与详情动作会把 Enter/Space/方向键冒泡到列表容器；只有列表自身
        // 持焦时才消费列表导航，否则会抢走按钮的键盘激活并让 keyup 落空。
        if !self.list_focus.is_focused(window) {
            return;
        }
        // 宽窗详情始终可见，方向键高亮可以直接更新详情；窄窗列表可见时详情被
        // 遮住，不应为不可见内容读取 README。
        let detail_visible = !self.is_narrow(window);
        match event.keystroke.key.as_str() {
            "up" => {
                window.prevent_default();
                cx.stop_propagation();
                self.move_selection(-1, detail_visible, cx);
            }
            "down" => {
                window.prevent_default();
                cx.stop_propagation();
                self.move_selection(1, detail_visible, cx);
            }
            "home" => {
                window.prevent_default();
                cx.stop_propagation();
                self.select_edge(false, detail_visible, cx);
            }
            "end" => {
                window.prevent_default();
                cx.stop_propagation();
                self.select_edge(true, detail_visible, cx);
            }
            "enter" => {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(entry) = self.selected.clone() {
                    // 搜索或分类筛选可能把旧高亮移出可见行；Enter 不能打开
                    // 已被筛掉的项，否则详情会显示列表里根本看不到的内容。
                    if !self.keyboard_entries(cx).iter().any(|item| {
                        matches!(item, render::ListEntry::Row(row) if row.selection == entry)
                    }) {
                        return;
                    }
                    self.select(entry, cx);
                    // 窄窗打开详情后列表会被卸载；焦点必须交给实际挂载的详情，
                    // 否则后续 Escape 落在失效的列表句柄上，事件冒泡会关闭设置页。
                    if self.is_narrow(window) {
                        window.focus(&self.detail_focus, cx);
                    }
                }
            }
            // 列表已可见时 Escape 不再由页面吞掉；让它冒泡到 Root 关闭设置页。
            _ => {}
        }
    }

    /// 窄窗详情焦点链上的 Escape 返回列表并恢复列表焦点。
    ///
    /// 详情显示时列表已卸载，列表上的 `handle_list_key` 收不到按键；若这里不处理，
    /// Escape 会冒泡到 Root 关闭整个设置页。窗口级确认或 Kit 弹层打开时焦点属于
    /// 顶层，不能抢它们的 Escape，因此只在没有确认/弹层时拦截。
    ///
    /// @author kongweiguang
    fn handle_detail_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.modified()
            || event.keystroke.key != "escape"
            || !self.is_narrow(window)
            || self.back_to_list
            || self.confirmation.is_some()
            || self.dialog_open
        {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
        self.back_to_list = true;
        window.focus(&self.list_focus, cx);
        cx.notify();
    }

    /// 分栏阈值按真实内容区与当前 rem 计算；列表 18rem 加详情 26rem 及间隔。
    fn is_narrow(&self, window: &Window) -> bool {
        self.content_width
            .get()
            .is_some_and(|width| width < dual_min_width(window))
    }
}

/// 列表与详情并排所需的最小内容宽度；低于它时按同一规则降级为单栏。
fn dual_min_width(window: &Window) -> Pixels {
    window.rem_size() * 46.0
}

/// 可见条目里行的下标；组标题与折叠组不参与方向键移动，但仍占据滚动序号。
///
/// 键盘导航和滚动定位共用这一份映射，避免渲染插入标题后两处索引再次漂移。
///
/// @author kongweiguang
fn row_entry_indices(entries: &[render::ListEntry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| matches!(entry, render::ListEntry::Row(_)).then_some(index))
        .collect()
}

/// 清理残留描述的是已提交但未回收的磁盘材料；任何终态文案都必须把它带上，
/// 否则用户只看到成功或失败，却不知道暂存目录还留着需要处理的证据。
///
/// @author kongweiguang
fn append_cleanup_warnings(status: String, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return status;
    }
    format!(
        "{} {}",
        status,
        crate::locale::text(
            format!(
                "另有 {} 项已提交但残留未清理：{}。",
                warnings.len(),
                warnings.join("；")
            ),
            format!(
                "{} committed item(s) left unremoved staging material: {}.",
                warnings.len(),
                warnings.join("; ")
            ),
        )
    )
}

/// 初始读取共享计数归零时才释放页面 busy。
///
/// 目录与状态各自独立落地，计数只负责“两者都结束”这一件事，不能用它把先到的
/// 结果压住。调用方在代次失配时提前返回，不会走到这里，因此不会与 deactivate
/// 已释放的 busy 互相覆盖。写入一旦开始，busy 属于写回执，初始读取收尾不能
/// 提前解除它，否则旧帧可以发起第二个并发写入。
///
/// @author kongweiguang
fn finish_initial_load(
    page: &mut ExtensionsPage,
    pending: &Cell<u8>,
    cx: &mut Context<ExtensionsPage>,
) {
    let remaining = pending.get().saturating_sub(1);
    pending.set(remaining);
    if remaining == 0 {
        if !page.writing {
            page.busy = false;
        }
        // 两个初始读取都结束后再按缓存有效期决定网络刷新，避免由哪一侧先完成
        // 决定市场是否自动刷新；目录读取失败时保留重试入口，不自动重试。
        if page.catalog_error.is_none() {
            page.refresh_if_due(cx);
        }
        cx.notify();
    }
}

/// 写入后重读的共享计数归零时释放读盘忙态；状态与观察各自落地，互不等待。
///
/// `reloading_drop_ins` 标记本次读盘由写入后刷新发起，离开分类时据此释放 busy；
/// 两个任务都结束才清除，避免先完成的一侧提前解除对退出/再进入的忙态保护。
/// 与初始读取一样，在途写入的 busy 不能被读盘收尾覆盖。
///
/// @author kongweiguang
fn finish_drop_in_reload(
    page: &mut ExtensionsPage,
    pending: &Cell<u8>,
    cx: &mut Context<ExtensionsPage>,
) {
    let remaining = pending.get().saturating_sub(1);
    pending.set(remaining);
    if remaining == 0 {
        if !page.writing {
            page.busy = false;
        }
        page.reloading_drop_ins = false;
        cx.notify();
    }
}

#[derive(Clone)]
enum Operation {
    Install(CatalogSelection),
    /// 整份计划一次确认后串行执行；重试必须重新计算，不重放这里的旧计划。
    BatchUpdate(BatchUpdatePlan),
    File(PathBuf, LocalPackageInspection),
    Change(ExtensionChange),
    /// 确认使用观察时冻结的指纹与修订；任一变化都视为冲突而不是重试。
    ConfirmDropIn {
        id: PluginId,
        fingerprint: String,
        revision: u64,
    },
    Delete(PluginId),
}

/// 重试意图同时保留操作与反馈来源；同 ID 的内置/外部记录不能互相接管失败提示。
#[derive(Clone)]
struct RetryOperation {
    operation: Operation,
    feedback: Option<SelectedEntry>,
}

/// 反馈归属默认按插件记录来源；内置启停由调用方显式传入 `SelectedEntry::Builtin`，
/// 因为同一个 `ExtensionChange` 类型无法从身份反推用户点的是哪一行。
fn operation_entry(operation: &Operation) -> Option<SelectedEntry> {
    match operation {
        Operation::Install(selection) => Some(SelectedEntry::Plugin(selection.id.clone())),
        // 批量没有单一反馈对象；部分完成与失败原因统一由页头状态承载。
        Operation::BatchUpdate(_) => None,
        Operation::File(_, inspection) => Some(SelectedEntry::Plugin(inspection.id.clone())),
        Operation::Change(change) => match change {
            ExtensionChange::Enable(id)
            | ExtensionChange::Disable(id)
            | ExtensionChange::Uninstall(id)
            | ExtensionChange::RemoveMissingRecord(id)
            | ExtensionChange::Rollback(id) => Some(SelectedEntry::Plugin(id.clone())),
            ExtensionChange::CancelFailedChange { pending, .. } => {
                Some(SelectedEntry::Plugin(pending.plugin_id().clone()))
            }
        },
        Operation::ConfirmDropIn { id, .. } => Some(SelectedEntry::Plugin(id.clone())),
        Operation::Delete(id) => Some(SelectedEntry::Plugin(id.clone())),
    }
}

/// pending-only 条目仍属于可撤销失败分支；已安装条目由 installed 详情接管。
/// 列表状态与详情撤销入口共用此判定，避免失败提示和撤销按钮各走一套条件。
fn pending_only_failure<'a>(state: &'a PluginStoreState, id: &PluginId) -> Option<&'a str> {
    if state.plugins.contains_key(id) {
        return None;
    }
    if !state.pending.iter().any(|change| change.plugin_id() == id) {
        return None;
    }
    state.failures.get(id).map(String::as_str)
}

/// 撤销入口只在启动应用 pending 失败留下的绑定上成立：包读取失败没有绑定，
/// 即使后来排了新操作也不能借用它，避免页面把旧失败错绑到用户刚发起的变更。
fn cancelable_failed_change<'a>(
    state: &'a PluginStoreState,
    id: &PluginId,
) -> Option<&'a PendingPluginChange> {
    if !state.pending_failures.contains(id) {
        return None;
    }
    state.pending.iter().find(|change| change.plugin_id() == id)
}

/// 已安装版本和目录版本只按语义版本比较，不使用字符串排序。
///
/// 选择同时冻结当前验签目录身份；没有真实目录（预览或目录失败）时返回 None，
/// 界面因此不会构造一个无法绑定到用户所见快照的安装目标。
///
/// @author kongweiguang
fn selection(
    plugin: &CatalogPlugin,
    host_api: &str,
    identity: Option<&CatalogIdentity>,
) -> Option<CatalogSelection> {
    let identity = identity?.clone();
    let platform = Platform::current()?;
    let version = plugin
        .versions
        .iter()
        .filter(|version| {
            version.compatible_with(host_api).unwrap_or(false)
                && version
                    .assets
                    .iter()
                    .any(|asset| asset.platform == platform || asset.platform == Platform::Any)
        })
        .max_by(|a, b| a.version.as_semver().cmp(b.version.as_semver()))?;
    let asset = version
        .assets
        .iter()
        .find(|asset| asset.platform == platform)
        .or_else(|| {
            version
                .assets
                .iter()
                .find(|asset| asset.platform == Platform::Any)
        })?;
    Some(CatalogSelection {
        id: plugin.id.clone(),
        version: version.version.clone(),
        platform: asset.platform,
        identity,
    })
}

/// 本地化信息缺失时稳定回退英文或插件 ID，不能假设所有市场条目都有中文。
fn label<'a>(map: &'a std::collections::BTreeMap<String, String>, fallback: &'a str) -> &'a str {
    let (preferred, fallback_language) = match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => ("zh-CN", "en"),
        crate::locale::UiLanguage::EnUs => ("en", "zh-CN"),
    };
    map.get(preferred)
        .or_else(|| {
            if preferred == "en" {
                map.get("en-US")
            } else {
                map.get("zh")
            }
        })
        .or_else(|| map.get(fallback_language))
        .or_else(|| {
            if fallback_language == "en" {
                map.get("en-US")
            } else {
                map.get("zh")
            }
        })
        .map_or(fallback, String::as_str)
}

/// 应用层 DTO 只带注册表英文名；中文界面按稳定 id 本地化，未知 id 才回落到注册表名称。
fn builtin_name_pair(id: &PluginId, registry_name: &str) -> (String, String) {
    let pair = match id.as_str() {
        "kerminal.core" => ("核心", "Core"),
        "kerminal.ssh" => ("SSH", "SSH"),
        "kerminal.sftp" => ("SFTP", "SFTP"),
        "kerminal.telnet" => ("Telnet", "Telnet"),
        "kerminal.serial" => ("串口", "Serial"),
        "kerminal.rdp" => ("远程桌面", "RDP"),
        "kerminal.agents" => ("Agent", "Agents"),
        "kerminal.commands" => ("命令", "Commands"),
        "kerminal.tmux" => ("tmux", "tmux"),
        "kerminal.containers" => ("容器", "Containers"),
        "kerminal.ports" => ("端口转发", "Port forwarding"),
        "kerminal.system" => ("系统", "System"),
        "kerminal.logs" => ("日志", "Logs"),
        _ => return (registry_name.to_owned(), registry_name.to_owned()),
    };
    (pair.0.to_owned(), pair.1.to_owned())
}

/// 行与详情共用同一个展示名，避免列表中文、详情英文的割裂。
fn builtin_display_name(unit: &BuiltinUnit) -> String {
    let (zh_cn, en_us) = builtin_name_pair(&unit.id, &unit.name);
    crate::locale::text(zh_cn, en_us).to_string()
}

/// 内置启停状态只认权威 `builtins` 表；缺省表示启用，界面不能自行推断。
fn builtin_enabled(state: Option<&PluginStoreState>, id: &PluginId) -> bool {
    state
        .and_then(|state| state.builtins.get(id))
        .copied()
        .unwrap_or(true)
}

/// 任一 pending 变更都意味着运行态尚未切换，行与详情都要提示重启。
fn builtin_pending(state: Option<&PluginStoreState>, id: &PluginId) -> bool {
    state.is_some_and(|state| state.pending.iter().any(|change| change.plugin_id() == id))
}

/// 内置行只投影身份、名称与启停状态；抽出后过滤、搜索和状态可直接单测。
#[derive(Clone, Debug, Eq, PartialEq)]
struct BuiltinRow {
    id: PluginId,
    name: String,
    can_disable: bool,
    enabled: bool,
    pending: bool,
}

/// 内置单元属于已安装能力，在统一列表里单独成组；搜索同时匹配稳定身份与
/// 当前语言展示名，界面看到什么就能搜到什么。
///
/// 分类筛选只收窄目录组，不能把内置组一起藏掉，因此这里不再接收筛选条件。
///
/// @author kongweiguang
fn builtin_rows(
    units: &[BuiltinUnit],
    state: Option<&PluginStoreState>,
    query: &str,
) -> Vec<BuiltinRow> {
    let query = query.trim().to_lowercase();
    units
        .iter()
        .filter_map(|unit| {
            let name = builtin_display_name(unit);
            let matches = query.is_empty()
                || unit.id.as_str().to_lowercase().contains(&query)
                || name.to_lowercase().contains(&query);
            matches.then(|| BuiltinRow {
                id: unit.id.clone(),
                name,
                can_disable: unit.can_disable,
                enabled: builtin_enabled(state, &unit.id),
                pending: builtin_pending(state, &unit.id),
            })
        })
        .collect()
}

/// 已安装计数取安装记录与内置身份的并集；使用同一身份的外部冲突记录不能重复计数。
///
/// 统一列表的页头不再展示总计，生产渲染没有读者；只保留验收快照与测试使用，
/// 避免默认构建把辅助函数报成 dead_code。
#[cfg(any(test, feature = "right-rail-acceptance"))]
fn installed_total(state: Option<&PluginStoreState>, units: &[BuiltinUnit]) -> usize {
    let mut ids = BTreeSet::new();
    if let Some(state) = state {
        ids.extend(state.plugins.keys().map(PluginId::as_str));
    }
    ids.extend(units.iter().map(|unit| unit.id.as_str()));
    ids.len()
}

/// 占用内置身份的记录是磁盘上的冲突材料，不是可加载的扩展；状态优先于普通失败标记。
fn is_reserved_identity(installed: &InstalledPlugin) -> bool {
    matches!(
        installed.status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ReservedIdentity
        }
    )
}

/// drop-in 行与详情共用的语义状态；与本地化文案分离，便于测试和复用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DropInState {
    PendingConfirmation,
    Ready,
    Missing,
    Corrupt,
    Unsupported,
    ReservedIdentity,
}

/// 未确认的手动投放即使同 id 出现在市场目录，也必须由记录行提供确认入口。
fn unconfirmed_drop_in<'a>(
    state: Option<&'a PluginStoreState>,
    id: &PluginId,
) -> Option<&'a InstalledPlugin> {
    state?
        .plugins
        .get(id)
        .filter(|installed| installed.source == PluginSource::DropIn && !installed.confirmed)
}

/// 记录状态来自启动复验，观察结果补充当前磁盘证据；两者都不可信时才回退到记录。
fn drop_in_state(
    installed: &InstalledPlugin,
    inspection: Option<&DropInInspection>,
) -> DropInState {
    if is_reserved_identity(installed) {
        return DropInState::ReservedIdentity;
    }
    let status = inspection.map_or(installed.status, |value| value.status);
    match status {
        PluginInstallStatus::Ready
            if installed.source == PluginSource::DropIn && !installed.confirmed =>
        {
            DropInState::PendingConfirmation
        }
        PluginInstallStatus::Ready => DropInState::Ready,
        PluginInstallStatus::Missing => DropInState::Missing,
        PluginInstallStatus::Corrupt => DropInState::Corrupt,
        PluginInstallStatus::Unsupported { .. } => DropInState::Unsupported,
    }
}

/// 只有就绪、具备指纹且尚未确认的观察结果能进入确认流程；损坏内容不能伪装成可确认。
fn drop_in_confirmable(inspection: &DropInInspection) -> bool {
    !inspection.confirmed
        && inspection.status == PluginInstallStatus::Ready
        && inspection.fingerprint.is_some()
}

/// 详情动作策略集中判定：冲突记录无动作，未确认 drop-in 只暴露确认，其余沿用现有管理入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredActionsPolicy {
    None,
    ConfirmDropIn,
    Unavailable,
    Manage,
}

fn stored_actions_policy(
    installed: &InstalledPlugin,
    inspection: Option<&DropInInspection>,
) -> StoredActionsPolicy {
    if is_reserved_identity(installed) {
        return StoredActionsPolicy::None;
    }
    if installed.source != PluginSource::DropIn {
        return StoredActionsPolicy::Manage;
    }
    let state = drop_in_state(installed, inspection);
    if !installed.confirmed {
        // 未确认的普通启用会绕过确认门禁；这里只允许绑定当前观察的确认动作。
        return if state == DropInState::PendingConfirmation
            && inspection.is_some_and(drop_in_confirmable)
        {
            StoredActionsPolicy::ConfirmDropIn
        } else {
            StoredActionsPolicy::Unavailable
        };
    }
    if state == DropInState::Ready {
        StoredActionsPolicy::Manage
    } else {
        // 已确认但目录缺失/损坏时仍保留卸载入口，但不能再次启用。
        StoredActionsPolicy::Unavailable
    }
}

/// 确认操作冻结观察时的指纹与修订；两者缺一都不能构造可执行意图。
fn drop_in_confirmation_operation(inspection: &DropInInspection) -> Option<Operation> {
    if !drop_in_confirmable(inspection) {
        return None;
    }
    inspection
        .fingerprint
        .clone()
        .map(|fingerprint| Operation::ConfirmDropIn {
            id: inspection.id.clone(),
            fingerprint,
            revision: inspection.revision,
        })
}

/// 空态必须区分“状态未知”“仍在读取”“搜索无匹配”“分类无匹配”“目录失败”和
/// 真实空列表；把失败或过滤结果说成“暂无扩展”会误导用户。
#[derive(Clone, Debug, Eq, PartialEq)]
enum EmptyStateKind {
    SearchNoMatch,
    StateUnknown,
    StateReading,
    CatalogUnavailable,
    CatalogReading,
    CategoryEmpty(String),
    Empty,
}

/// 状态失败和读取中优先于搜索/分类文案；目录失败只在确实没有可展示内容时出现，
/// 已安装与内置组仍可见时不能把整页说成市场不可用。
///
/// @author kongweiguang
fn empty_state_kind(
    query: &str,
    category: Option<&str>,
    catalog_present: bool,
    catalog_error: bool,
    state_present: bool,
    state_error: bool,
) -> EmptyStateKind {
    if state_error {
        EmptyStateKind::StateUnknown
    } else if !state_present {
        EmptyStateKind::StateReading
    } else if !query.trim().is_empty() {
        EmptyStateKind::SearchNoMatch
    } else if catalog_error {
        EmptyStateKind::CatalogUnavailable
    } else if !catalog_present {
        EmptyStateKind::CatalogReading
    } else if let Some(category) = category {
        EmptyStateKind::CategoryEmpty(category.to_owned())
    } else {
        EmptyStateKind::Empty
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/extensions_page.rs"]
mod tests;

/// 隔离窗口验收只暴露页面中已有的状态，不触发网络或修改插件。
#[cfg(feature = "right-rail-acceptance")]
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExtensionsAcceptanceSnapshot {
    pub catalog_entries: usize,
    pub installed: usize,
    pub pending: usize,
    pub offline: bool,
    pub busy: bool,
    pub selected: Option<String>,
    pub error: Option<String>,
    /// 目录与状态失败分开暴露，验收可证明任一失败没有阻断另一侧。
    pub catalog_error: Option<String>,
    pub state_error: Option<String>,
    pub readme_error: Option<String>,
    pub confirmation_visible: bool,
    pub confirmation_kind: Option<&'static str>,
    pub confirmation_id: Option<String>,
    pub confirmation_permissions: Option<Vec<String>>,
    pub pending_restart: bool,
    /// 正文实测宽度：验收先等它降到单栏区间再点击行，避免宽窗布局下点击后
    /// 焦点留在随后被卸载的列表上。
    pub content_width: Option<f32>,
    /// 窄窗详情是否处于“返回列表”态；与返回按钮选择器一起证明 Escape 回到列表。
    pub back_to_list: bool,
    /// 统一列表的组顺序与计数；验收据此证明已安装置顶、内置可折叠、待重启独立。
    pub groups: Vec<ExtensionGroupSnapshot>,
    /// 批量更新候选数，不受搜索/分类/折叠影响。
    pub update_candidates: usize,
    /// 批量确认中冻结的逐项“id 旧版本→目标版本”，用于证明一次确认覆盖真实范围。
    pub batch_items: Vec<String>,
    /// 批量执行中的当前项“序号/总数 id”，用于证明串行进度可见。
    pub batch_progress: Option<String>,
    /// 真实生效来源地址；验收用它证明保存回读后运行态确实切换。
    pub source_url: String,
    /// 来源弹层是否打开；验收据此等待真实点击后的弹层出现与关闭。
    pub source_dialog_open: bool,
    /// 来源保存/回读是否仍在进行；验收据此等待最终结果而不是提前截图。
    pub source_saving: bool,
    /// 来源保存或回读失败原因；只读真实状态，不注入伪成功。
    pub source_error: Option<String>,
}

/// 验收只读取组标题投影，不复制分组规则。
#[cfg(feature = "right-rail-acceptance")]
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExtensionGroupSnapshot {
    pub kind: &'static str,
    pub count: usize,
    pub collapsed: bool,
}
