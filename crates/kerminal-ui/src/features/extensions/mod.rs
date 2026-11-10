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
        CatalogPlugin, CatalogSelection, CatalogSnapshot, ExtensionChange, ExtensionsService,
        InstallProgress, LocalPackageInspection,
    },
};
use kerminal_domain::plugins::{PluginId, PluginStoreState};
use kerminal_plugin_protocol::Platform;
use std::{path::PathBuf, sync::Arc};
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

/// 未经市场审核的文件以及不可逆数据删除必须经过明确确认。
#[derive(Clone)]
enum Confirmation {
    File(PathBuf, LocalPackageInspection),
    Install(CatalogSelection),
    DeleteData(PluginId),
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
    filter: Filter,
    selected: Option<PluginId>,
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
            retry_operation: None,
            confirmation: None,
            dialog_open: false,
            dialog_generation: 0,
            window_handle,
            list_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
        }
    }

    /// 首次进入先读可信缓存与权威状态；失败保留现有展示并提供重试。
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        // 切回设置或重新进入时，遗留的窗口级确认必须先关闭，不能让它覆盖新页面状态。
        self.close_open_confirmation(cx);
        self.retry_operation = None;
        if self.busy {
            return;
        }
        self.busy = true;
        self.refreshing = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let state = service.state().await?;
                    let catalog = service.cached_catalog().await?;
                    Ok((state, catalog))
                })
                .await;
            let _ = this.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.busy = false;
                page.refreshing = false;
                match result {
                    Ok((state, catalog)) => {
                        page.state = Some(state);
                        page.offline = catalog.as_ref().is_some_and(|snapshot| snapshot.stale);
                        page.catalog = catalog;
                        page.status = None;
                        page.status_is_warning = false;
                        page.prune_selection(cx);
                        // 每次进入分类都会 activate；日常刷新按 24 小时缓存判断，不能重复打网络。
                        page.refresh_if_due(cx);
                    }
                    Err(error) => {
                        // 取消属于页面的正常空闲路径，不留下错误文案、离线标记或重试入口。
                        if error.public_code() != PublicErrorCode::Cancelled {
                            page.retry_operation = None;
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
                        page.offline = catalog.stale;
                        page.catalog = Some(catalog);
                        page.status = None;
                        page.status_is_warning = false;
                        page.prune_selection(cx);
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
    fn select(&mut self, id: PluginId, cx: &mut Context<Self>) {
        if let Some(previous) = self.readme_cancel.take() {
            previous.cancel();
        }
        self.selected = Some(id.clone());
        self.readme_generation = self.readme_generation.wrapping_add(1);
        let generation = self.readme_generation;
        self.readme = None;
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
                    && page.selected.as_ref() == Some(&id)
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
                match result {
                    Ok(Ok((state, warning))) => {
                        page.state = Some(state);
                        page.status_is_warning = warning.is_some();
                        page.status = warning.map(|error| error.public_message().to_owned());
                    }
                    Ok(Err((error, committed, warning))) => {
                        // 取消安装属于用户主动结束，不写错误、不提供重试，也不把页面标成失败。
                        if error.public_code() != PublicErrorCode::Cancelled {
                            if !committed && !matches!(error.public_code(), PublicErrorCode::OutcomeUnknown)
                                && !matches!(retry_candidate, Operation::Delete(_) | Operation::File(_, _) | Operation::Change(ExtensionChange::CancelFailedChange { .. })) {
                                page.retry_operation = Some(retry_candidate);
                            }
                            page.status_is_warning = false;
                            page.status = Some(if committed {
                                format!("{}{}", crate::locale::text("操作已提交，但读取状态失败；请刷新状态。", "The operation was committed, but its state could not be read. Refresh the state."), warning.map(|value| format!(" {}", value.public_message())).unwrap_or_default())
                            } else { error.public_message().to_owned() });
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

    /// 验收读取真实页面和服务回执投影，不能用构造数据替代安装结果。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn acceptance_snapshot(&self) -> ExtensionsAcceptanceSnapshot {
        ExtensionsAcceptanceSnapshot {
            catalog_entries: self
                .catalog
                .as_ref()
                .map_or(0, |value| value.catalog.plugins.len()),
            installed: self.state.as_ref().map_or(0, |value| value.plugins.len()),
            pending: self.state.as_ref().map_or(0, |value| value.pending.len()),
            offline: self.offline,
            busy: self.busy,
            selected: self.selected.as_ref().map(|id| id.as_str().to_owned()),
            error: self.status.clone(),
            confirmation_visible: self.confirmation.is_some(),
            confirmation_kind: self
                .confirmation
                .as_ref()
                .map(|confirmation| match confirmation {
                    Confirmation::File(_, _) => "file",
                    Confirmation::Install(_) => "install",
                    Confirmation::DeleteData(_) => "delete_data",
                }),
            confirmation_id: self.confirmation.as_ref().map(|confirmation| {
                match confirmation {
                    Confirmation::File(_, inspection) => inspection.id.as_str(),
                    Confirmation::Install(selection) => selection.id.as_str(),
                    Confirmation::DeleteData(id) => id.as_str(),
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
            self.close_open_confirmation(cx);
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

    /// 页面重新激活时关闭可能遗留的确认；Kit 负责把焦点还给原控件。
    fn close_open_confirmation(&mut self, cx: &mut Context<Self>) {
        let was_open = self.dialog_open;
        self.confirmation = None;
        self.dialog_open = false;
        if was_open {
            let window_handle = self.window_handle;
            let _ = window_handle.update(cx, |_, window, cx| window.close_dialog(cx));
        }
    }

    /// 目录替换后留下的选择会展示另一份目录的旧详情，必须按当前目录与安装集合重新核对。
    /// 无真实目录时示例就是当前展示源，示例选择仍然有效。
    fn prune_selection(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.as_ref() else {
            return;
        };
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
        if in_catalog || in_store {
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

    /// 列表和详情共用同一受信目录记录，避免使用未校验的界面 URL 作为安装目标。
    /// 仅在真实目录缺席时回退到内嵌示例，示例永远不会成为安装目标。
    fn selected_plugin(&self) -> Option<&CatalogPlugin> {
        let id = self.selected.as_ref()?;
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
            return sample::catalog()
                .plugins
                .iter()
                .find(|plugin| &plugin.id == id);
        }
        None
    }
}

#[derive(Clone)]
enum Operation {
    Install(CatalogSelection),
    File(PathBuf, LocalPackageInspection),
    Change(ExtensionChange),
    Delete(PluginId),
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
