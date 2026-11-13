//! 扩展页面只保存交互状态；安装和网络由应用用例完成。
//! @author kongweiguang

use gpui_kit::{
    AnyWindowHandle, AppContext as _, Context, Entity, EventEmitter, PathPromptOptions,
    ScrollHandle, Subscription, Window,
    component::{
        WindowExt as _,
        input::{InputEvent, InputState},
    },
};
use kerminal_application::{
    ApplicationTaskExecutor, PublicErrorCode,
    extensions::{
        BuiltinUnit, CatalogPlugin, CatalogSelection, CatalogSnapshot, DropInInspection,
        ExtensionChange, ExtensionsService, InstallProgress, LocalPackageInspection,
    },
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginId, PluginInstallStatus, PluginSource,
    PluginStoreState, PluginUnsupportedReason,
};
use kerminal_plugin_protocol::Platform;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

mod markdown;
mod render;
mod sample;

/// 重启由 Root 按会话关闭契约处理，页面不能自行结束进程。
#[derive(Clone, Debug)]
pub enum ExtensionsEvent {
    RestartRequested,
    /// 写操作完成时通知设置面板重绘，让退出拒绝提示随写入结束立即消失。
    WriteFinished,
}

/// 导航仅用于本页面的过滤，不改变安装状态。
#[derive(Clone, Debug, Eq, PartialEq)]
enum Filter {
    All,
    Installed,
    Updates,
    Category(String),
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
    DeleteData(PluginId),
    /// 手动投放记录在确认时仍绑定观察到的指纹与修订，目录被替换就不能继续。
    DropIn(DropInInspection),
}

/// 实例拥有搜索焦点与异步代次；服务和 executor 由组合根注入。
pub struct ExtensionsPage {
    service: Arc<ExtensionsService>,
    executor: ApplicationTaskExecutor,
    host_api: String,
    search: Entity<InputState>,
    _search_subscription: Subscription,
    catalog: Option<CatalogSnapshot>,
    offline: bool,
    state: Option<PluginStoreState>,
    /// 内置单元名单来自静态注册表，只提供身份与可关闭性；实际启停仍看 state.builtins。
    builtin_units: Vec<BuiltinUnit>,
    /// 手动投放记录的只读观察；确认按钮必须使用这里的指纹与修订。
    drop_ins: Vec<DropInInspection>,
    /// drop-in 观察有独立代次，不能被目录刷新或普通写入的代次推进误作废。
    drop_in_generation: u64,
    drop_in_cancel: Option<CancellationToken>,
    /// 本次 drop-in 重读是否持有页面 busy；离开分类时据它释放读盘占用的忙态。
    reloading_drop_ins: bool,
    filter: Filter,
    selected: Option<SelectedEntry>,
    readme: Option<String>,
    readme_cancel: Option<CancellationToken>,
    readme_generation: u64,
    status: Option<String>,
    /// 清理残留属于可继续使用的警告，不能与失败错误共用同一种语义色。
    status_is_warning: bool,
    busy: bool,
    /// 只有目录读取显示刷新转圈；安装和文件预检不冒充目录刷新。
    refreshing: bool,
    /// 只有安装、更新、启停或卸载等会改插件目录的操作才阻止退出；只读加载不阻止。
    writing: bool,
    cancel: Option<CancellationToken>,
    generation: u64,
    progress: Option<InstallProgress>,
    active_install: Option<PluginId>,
    /// 本地包预检与目录安装共用取消门禁，页面级活动条据此显示校验状态。
    inspecting: bool,
    /// 页面是否仍属于当前设置分类；离开后迟到的预检不能升级为窗口级确认。
    active: bool,
    retry_operation: Option<Operation>,
    confirmation: Option<Confirmation>,
    /// Kit 窗口级确认只允许一个实例；标记防止重建页面或连续点击重复打开。
    dialog_open: bool,
    /// 延迟打开时核对代次，避免连续确认让旧 defer 打开新弹层。
    dialog_generation: u64,
    window_handle: AnyWindowHandle,
    list_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
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
        let window_handle = window.window_handle();
        Self {
            service,
            executor,
            host_api,
            search,
            _search_subscription: subscription,
            catalog: None,
            offline: false,
            state: None,
            builtin_units: Vec::new(),
            drop_ins: Vec::new(),
            drop_in_generation: 0,
            drop_in_cancel: None,
            reloading_drop_ins: false,
            filter: Filter::All,
            selected: None,
            readme: None,
            readme_cancel: None,
            readme_generation: 0,
            status: None,
            status_is_warning: false,
            busy: false,
            refreshing: false,
            writing: false,
            cancel: None,
            generation: 0,
            progress: None,
            active_install: None,
            inspecting: false,
            active: false,
            retry_operation: None,
            confirmation: None,
            dialog_open: false,
            dialog_generation: 0,
            window_handle,
            list_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
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
    }

    /// 首次进入先读可信缓存与权威状态；失败保留现有展示并提供重试。
    ///
    /// drop-in 观察在权威状态落地后单独读取：它只补充磁盘证据，失败不应阻塞
    /// 列表与详情，也不能借用目录刷新或写操作的代次把自己作废。
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        // 切回设置或重新进入时，遗留的窗口级确认必须先关闭，不能让它覆盖新页面状态。
        self.close_open_confirmation(cx);
        self.retry_operation = None;
        if self.busy {
            return;
        }
        // 内置名单是编译期静态数据；读取失败说明注册表自身无效，不能带着空名单继续。
        match self.service.builtin_units() {
            Ok(units) => self.builtin_units = units,
            Err(error) => {
                self.status = Some(error.public_message().to_owned());
                self.status_is_warning = false;
                cx.notify();
                return;
            }
        }
        self.supersede_drop_in_inspection();
        let drop_in_generation = self.drop_in_generation;
        let drop_in_token = CancellationToken::new();
        self.drop_in_cancel = Some(drop_in_token.clone());
        self.busy = true;
        self.refreshing = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let service = self.service.clone();
        let executor = self.executor.clone();
        let inspect_service = self.service.clone();
        let inspect_executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let state = service.state().await?;
                    let catalog = service.cached_catalog().await?;
                    Ok((state, catalog))
                })
                .await;
            let loaded = this
                .update(cx, |page, cx| {
                    if page.generation != generation {
                        return false;
                    }
                    page.busy = false;
                    page.refreshing = false;
                    match result {
                        Ok((state, catalog)) => {
                            let previous_preview = page.catalog.is_none();
                            page.state = Some(state);
                            page.offline = catalog.as_ref().is_some_and(|snapshot| snapshot.stale);
                            page.catalog = catalog;
                            page.status = None;
                            page.status_is_warning = false;
                            page.prune_selection(cx);
                            page.reconcile_catalog_change(previous_preview, cx);
                            // 每次进入分类都会 activate；日常刷新按 24 小时缓存判断，不能重复打网络。
                            page.refresh_if_due(cx);
                            true
                        }
                        Err(error) => {
                            // 取消属于页面的正常空闲路径，不留下错误文案、离线标记或重试入口。
                            if error.public_code() != PublicErrorCode::Cancelled {
                                page.retry_operation = None;
                                page.status = Some(error.public_message().to_owned());
                                page.status_is_warning = false;
                            }
                            false
                        }
                    }
                })
                .unwrap_or(false);
            if !loaded {
                return;
            }
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
                        page.prune_selection(cx);
                    }
                    // 页面离开或被新一次激活取代时静默收尾，不把取消写成可重试错误。
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
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
        // 离开分类后 drop-in 观察没有展示目标，继续读取只会占用工作线程。
        self.supersede_drop_in_inspection();
        // drop-in 重读的完成回调会因代次失配而提前返回，不能指望它清 busy；
        // 读盘 busy 由发起者自己释放，否则重新进入分类会被 busy 永久挡住。
        if self.reloading_drop_ins {
            self.reloading_drop_ins = false;
            self.busy = false;
        }
        if self.inspecting
            && let Some(token) = &self.cancel
        {
            token.cancel();
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
    fn refresh_catalog(&mut self, due_only: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.retry_operation = None;
        self.busy = true;
        self.refreshing = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
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
                if page.generation != generation {
                    return;
                }
                page.busy = false;
                page.refreshing = false;
                page.cancel = None;
                match result {
                    Ok(catalog) => {
                        let previous_preview = page.catalog.is_none();
                        page.offline = catalog.stale;
                        page.catalog = Some(catalog);
                        page.status = None;
                        page.status_is_warning = false;
                        page.prune_selection(cx);
                        page.reconcile_catalog_change(previous_preview, cx);
                        // 用户显式刷新后重读磁盘观察，修好或移除投放文件无需离开分类再回来；
                        // 进入分类的按需刷新已由 activate 的后继观察覆盖，避免并发读取。
                        if !due_only {
                            page.reload_drop_in_state(cx);
                        }
                    }
                    Err(error) => {
                        // 用户取消刷新时静默回到空闲；只有真实失败才把页面标成离线并保留重试文案。
                        if error.public_code() != PublicErrorCode::Cancelled {
                            page.offline = true;
                            page.status = Some(error.public_message().to_owned());
                            page.status_is_warning = false;
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
        if self.busy {
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
        if self.busy {
            return;
        }
        // 预检会推进页面代次；旧 drop-in 观察不能跨这次操作继续落地。
        self.supersede_drop_in_inspection();
        self.busy = true;
        self.inspecting = true;
        self.retry_operation = None;
        self.status = None;
        self.status_is_warning = false;
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

    /// README 只能通过签名目录对应的应用接口读取，迟到结果不得覆盖新选中的插件。
    fn select(&mut self, entry: SelectedEntry, cx: &mut Context<Self>) {
        // 详情内容随选择整体替换，沿用上一个插件的滚动位置会让人误以为没切换成功。
        self.detail_scroll.set_offset(Default::default());
        if let Some(previous) = self.readme_cancel.take() {
            previous.cancel();
        }
        let id = entry.id().clone();
        self.selected = Some(entry);
        self.readme_generation = self.readme_generation.wrapping_add(1);
        let generation = self.readme_generation;
        self.readme = None;
        // 详情优先级已由 selected_plugin 统一判定：内置、未确认投放和目录缺席都没有目录条目，
        // 选它们只切换详情；已确认的 drop-in 若同时出现在市场目录，详情由目录条目接管，
        // 仍需加载签名 README，不能仅凭安装来源提前跳过。
        if self.selected_plugin().is_none() {
            cx.notify();
            return;
        }
        // 示例条目只带内嵌元数据，没有可验签的 README 地址，不能为预览发起网络请求。
        if self.catalog.is_none() && sample::contains(&id) {
            cx.notify();
            return;
        }
        let token = CancellationToken::new();
        self.readme_cancel = Some(token.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(token, move |cancel| async move {
                    service.readme(&id, cancel).await.map(|text| (id, text))
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if let Ok((id, text)) = result
                    && page.readme_generation == generation
                    && matches!(
                        page.selected.as_ref(),
                        Some(SelectedEntry::Plugin(selected)) if selected == &id
                    )
                {
                    page.readme = text;
                    page.readme_cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 所有写操作完成后回读权威状态；失败保留明确错误，禁止自动重放。
    fn run_operation(&mut self, operation: Operation, cx: &mut Context<Self>) {
        // 预览示例没有可信安装来源；即使调用方绕过列表，也必须在状态机入口拒绝。
        if let Operation::Install(selection) = &operation
            && sample::is_preview_entry(self.catalog.is_some(), &selection.id)
        {
            self.confirmation = None;
            self.dialog_open = false;
            self.retry_operation = None;
            self.status = Some(
                crate::locale::text("示例条目不可安装。", "Sample entries are not installable.")
                    .to_string(),
            );
            self.status_is_warning = false;
            cx.notify();
            return;
        }
        if self.busy {
            return;
        }
        // 写操作基于当前观察；在途 drop-in 检查不能把旧指纹重新写回页面。
        self.supersede_drop_in_inspection();
        self.busy = true;
        self.inspecting = false;
        // 写操作尚未收到权威回读时退出会丢失提交结果，只有这一阶段需要阻止退出。
        self.writing = true;
        self.status = None;
        self.status_is_warning = false;
        self.retry_operation = None;
        self.dialog_open = false;
        self.active_install = match &operation {
            Operation::Install(selection) => Some(selection.id.clone()),
            Operation::File(_, inspection) => Some(inspection.id.clone()),
            _ => None,
        };
        self.confirmation = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = CancellationToken::new();
        self.cancel = matches!(&operation, Operation::Install(_) | Operation::File(_, _))
            .then(|| cancel.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        let retry_candidate = operation.clone();
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
                        page.status_is_warning = warning.is_some();
                        page.status = warning.map(|error| error.public_message().to_owned());
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
                            } else {
                                if !committed && !matches!(error.public_code(), PublicErrorCode::OutcomeUnknown)
                                    && !matches!(retry_candidate, Operation::Delete(_) | Operation::File(_, _) | Operation::Change(ExtensionChange::CancelFailedChange { .. }) | Operation::ConfirmDropIn { .. }) {
                                    page.retry_operation = Some(retry_candidate);
                                }
                                page.status_is_warning = false;
                                page.status = Some(if committed {
                                    format!("{}{}", crate::locale::text("操作已提交，但读取状态失败；请刷新状态。", "The operation was committed, but its state could not be read. Refresh the state."), warning.map(|value| format!(" {}", value.public_message())).unwrap_or_default())
                                } else { error.public_message().to_owned() });
                            }
                        }
                    }
                    Err(error) => {
                        if error.public_code() != PublicErrorCode::Cancelled {
                            page.retry_operation = None;
                            page.status = Some(error.public_message().to_owned());
                            page.status_is_warning = false;
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

    /// 确认冲突或确认成功后重新读取权威状态与 drop-in 观察。
    ///
    /// 只刷新数据，不覆盖调用方刚写入的提示：冲突提示需要留在页面上解释
    /// 为什么列表已经变化；成功路径本来就没有提示，刷新后详情会撤下确认入口。
    fn reload_drop_in_state(&mut self, cx: &mut Context<Self>) {
        // 页面已离开时观察没有展示目标；下次 activate 会重新读取，不能把 busy 留在无人页面。
        if self.busy || !self.active {
            return;
        }
        self.busy = true;
        self.reloading_drop_ins = true;
        self.inspecting = false;
        self.supersede_drop_in_inspection();
        let drop_in_generation = self.drop_in_generation;
        let token = CancellationToken::new();
        self.drop_in_cancel = Some(token.clone());
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(token, move |cancel| async move {
                    let state = service.state().await?;
                    let inspections = service.inspect_drop_ins(cancel).await?;
                    Ok((state, inspections))
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                // 代次失配说明 deactivate 已释放本次读盘忙态；这里只负责丢弃旧结果，
                // 不能覆盖新的激活或写入已经占用的状态。
                if page.drop_in_generation != drop_in_generation {
                    return;
                }
                page.busy = false;
                page.reloading_drop_ins = false;
                page.drop_in_cancel = None;
                match result {
                    Ok((state, inspections)) => {
                        page.state = Some(state);
                        page.drop_ins = inspections;
                        page.prune_selection(cx);
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {}
                    Err(error) => {
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

    /// 验收读取真实页面和服务回执投影，不能用构造数据替代安装结果。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn acceptance_snapshot(&self) -> ExtensionsAcceptanceSnapshot {
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
            error: self.status.clone(),
            confirmation_visible: self.confirmation.is_some(),
            confirmation_kind: self
                .confirmation
                .as_ref()
                .map(|confirmation| match confirmation {
                    Confirmation::File(_, _) => "file",
                    Confirmation::Install(_) => "install",
                    Confirmation::DeleteData(_) => "delete_data",
                    Confirmation::DropIn(_) => "drop_in",
                }),
            confirmation_id: self.confirmation.as_ref().map(|confirmation| {
                match confirmation {
                    Confirmation::File(_, inspection) => inspection.id.as_str(),
                    Confirmation::Install(selection) => selection.id.as_str(),
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
                    Confirmation::DeleteData(_) => Vec::new(),
                    Confirmation::DropIn(inspection) => inspection.permissions.clone(),
                }
            }),
            pending_restart: self
                .state
                .as_ref()
                .is_some_and(|state| !state.pending.is_empty()),
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

    /// 打开确认的唯一入口：先记录待执行意图，再交给窗口级 Kit Dialog，页面本身不绘制遮罩。
    fn open_confirmation(&mut self, confirmation: Confirmation, cx: &mut Context<Self>) {
        // 页面已离开时，迟到的预检结果只能回到后台收尾，不能弹出窗口级确认。
        if !self.active {
            self.close_open_confirmation(cx);
            return;
        }
        // 示例只有展示元数据；在打开确认前拒绝，避免弹层承诺一个不可执行的安装。
        if let Confirmation::Install(selection) = &confirmation
            && sample::is_preview_entry(self.catalog.is_some(), &selection.id)
        {
            self.close_open_confirmation(cx);
            self.retry_operation = None;
            self.status = Some(
                crate::locale::text("示例条目不可安装。", "Sample entries are not installable.")
                    .to_string(),
            );
            self.status_is_warning = false;
            cx.notify();
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
    /// 无真实目录时示例就是当前展示源，示例选择仍然有效。
    fn prune_selection(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.as_ref() else {
            return;
        };
        let id = entry.id();
        let in_catalog = match &self.catalog {
            Some(catalog) => catalog
                .catalog
                .plugins
                .iter()
                .any(|plugin| &plugin.id == id),
            None => sample::contains(id),
        };
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
        self.readme_generation = self.readme_generation.wrapping_add(1);
        cx.notify();
    }

    /// 目录替换后分类筛选必须收敛到新目录；从示例预览切到真实目录时补一次 README。
    /// 只在预览→真实目录的那一次替换触发重读，普通刷新不重复请求正文。
    fn reconcile_catalog_change(&mut self, previous_preview: bool, cx: &mut Context<Self>) {
        let Some(catalog) = self.catalog.as_ref() else {
            return;
        };
        let category_removed = matches!(
            &self.filter,
            Filter::Category(category)
                if !catalog
                    .catalog
                    .plugins
                    .iter()
                    .any(|plugin| plugin.categories.contains(category))
        );
        let reselect = previous_preview
            && matches!(
                &self.selected,
                Some(SelectedEntry::Plugin(id))
                    if catalog
                        .catalog
                        .plugins
                        .iter()
                        .any(|plugin| &plugin.id == id)
            );
        if category_removed {
            self.filter = Filter::All;
        }
        if reselect && let Some(entry) = self.selected.clone() {
            self.select(entry, cx);
        }
    }

    /// 列表和详情共用同一受信目录记录，避免使用未校验的界面 URL 作为安装目标。
    /// 仅在真实目录缺席时回退到内嵌示例，示例永远不会成为安装目标。
    fn selected_plugin(&self) -> Option<&CatalogPlugin> {
        // 内置详情和未确认投放记录都有自己的详情分支；只有普通 Plugin 选择才允许目录条目优先。
        let SelectedEntry::Plugin(id) = self.selected.as_ref()? else {
            return None;
        };
        // 未确认的手动投放即使与目录同 id，也必须先展示确认入口；目录行同步隐藏，避免两个入口竞争。
        if unconfirmed_drop_in(self.state.as_ref(), id).is_some() {
            return None;
        }
        if let Some(plugin) = self.catalog.as_ref().and_then(|catalog| {
            catalog
                .catalog
                .plugins
                .iter()
                .find(|plugin| &plugin.id == id)
        }) {
            return Some(plugin);
        }
        if self.catalog.is_none() {
            // 示例 id 若已被本机安装或排队，交给真实存储详情，不能被预览遮蔽管理入口。
            let in_store = self.state.as_ref().is_some_and(|state| {
                state.plugins.contains_key(id)
                    || state.pending.iter().any(|change| change.plugin_id() == id)
            });
            if in_store {
                return None;
            }
            return sample::catalog()
                .plugins
                .iter()
                .find(|plugin| &plugin.id == id);
        }
        None
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
}

#[derive(Clone)]
enum Operation {
    Install(CatalogSelection),
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
fn selection(plugin: &CatalogPlugin, host_api: &str) -> Option<CatalogSelection> {
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

/// 内置单元属于已安装能力，只在“全部”和“已安装”出现；分类筛选没有它们的归属。
/// 搜索同时匹配稳定身份与当前语言展示名，界面看到什么就能搜到什么。
fn builtin_rows(
    units: &[BuiltinUnit],
    state: Option<&PluginStoreState>,
    query: &str,
    filter: &Filter,
) -> Vec<BuiltinRow> {
    if !matches!(filter, Filter::All | Filter::Installed) {
        return Vec::new();
    }
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
fn installed_total(state: Option<&PluginStoreState>, units: &[BuiltinUnit]) -> usize {
    let mut ids = BTreeSet::new();
    if let Some(state) = state {
        ids.extend(state.plugins.keys().map(PluginId::as_str));
    }
    ids.extend(units.iter().map(|unit| unit.id.as_str()));
    ids.len()
}

/// 页头总数同样按稳定身份去重，目录与内置名单出现同名身份时只算一个。
fn extension_total(plugins: &[CatalogPlugin], units: &[BuiltinUnit]) -> usize {
    let mut ids: BTreeSet<&str> = plugins.iter().map(|plugin| plugin.id.as_str()).collect();
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
    pub confirmation_visible: bool,
    pub confirmation_kind: Option<&'static str>,
    pub confirmation_id: Option<String>,
    pub confirmation_permissions: Option<Vec<String>>,
    pub pending_restart: bool,
}
