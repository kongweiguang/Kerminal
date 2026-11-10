//! 扩展页由设置弹框持有，服务仍由组合根注入，导航切换不重建异步状态。
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{App, AppContext as _, Context, Entity, Subscription, Window};
#[cfg(feature = "right-rail-acceptance")]
use kerminal_application::{AppError, AppResult};
use kerminal_application::{ApplicationTaskExecutor, extensions::ExtensionsService};

use super::{PreferencesEvent, PreferencesPanel, PreferencesSection};
use crate::features::extensions::{ExtensionsEvent, ExtensionsPage};

/// 设置弹框内只保留页面实体与一次进入激活意图。
///
/// 页面创建需要窗口输入状态，所以导航点击先记录激活意图，正文首次绘制时再创建；
/// 这样设置启动、仅浏览其它分类都不会触发插件目录读取。
///
/// @author kongweiguang
#[derive(Default)]
pub(in crate::features::preferences) struct ExtensionsPreferencesState {
    service: Option<Arc<ExtensionsService>>,
    executor: Option<ApplicationTaskExecutor>,
    host_api: String,
    page: Option<Entity<ExtensionsPage>>,
    // 订阅只用于保持事件监听存活，不参与状态读取。
    _subscription: Option<Subscription>,
    /// 进入分类时置位，创建或下一次绘制时消费，避免每帧重复加载目录。
    activation_pending: bool,
}

impl ExtensionsPreferencesState {
    /// 组合根在启动时冻结服务与兼容 API；页面仍只在用户进入扩展分类时创建。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn new(
        service: Option<Arc<ExtensionsService>>,
        executor: ApplicationTaskExecutor,
        host_api: String,
    ) -> Self {
        Self {
            service,
            executor: Some(executor),
            host_api,
            page: None,
            _subscription: None,
            activation_pending: false,
        }
    }

    /// 页面宿主仍由设置弹框持有，重启请求经面板转交 Root，页面不自行结束进程。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn ensure_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) -> Option<Entity<ExtensionsPage>> {
        if self.page.is_none() {
            let (Some(service), Some(executor)) = (self.service.clone(), self.executor.clone())
            else {
                return None;
            };
            let host_api = self.host_api.clone();
            let page = cx.new(|cx| ExtensionsPage::new(service, executor, host_api, window, cx));
            // 订阅挂在设置面板上而不是页面内，关闭分类后事件仍由 Root 统一处理。
            self._subscription = Some(cx.subscribe(
                &page,
                |_panel, _page, event: &ExtensionsEvent, cx| match event {
                    ExtensionsEvent::RestartRequested => {
                        cx.emit(PreferencesEvent::ExtensionsRestartRequested)
                    }
                },
            ));
            self.page = Some(page);
        }
        if self.activation_pending {
            self.activation_pending = false;
            if let Some(page) = self.page.as_ref() {
                page.update(cx, |page, cx| page.activate(cx));
            }
        }
        self.page.clone()
    }

    /// 每次进入分类都重新读取权威状态；页面自己的 busy 门禁会拒绝并发重复请求。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn activate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<PreferencesPanel>,
    ) {
        // ensure_page 会消费本次激活意图，避免同一进入动作重复启动目录读取。
        self.activation_pending = true;
        self.ensure_page(window, cx);
    }

    /// 无窗口的导航入口只记录意图，首次正文绘制时再创建页面。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn request_activation(&mut self) {
        self.activation_pending = true;
    }

    /// 扩展操作会写插件目录和待重启集合，设置关闭与退出都必须等它结束。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn is_busy(&self, cx: &App) -> bool {
        self.page
            .as_ref()
            .is_some_and(|page| page.read(cx).is_busy())
    }

    /// 验收直接注入隔离包路径，后续预检、确认与安装仍复用生产页面流程。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(in crate::features::preferences) fn acceptance_inspect_file(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut Context<PreferencesPanel>,
    ) -> AppResult<()> {
        let page = self
            .page
            .clone()
            .ok_or_else(|| AppError::not_found("扩展页面不可用"))?;
        page.update(cx, |page, cx| page.acceptance_inspect_file(path, cx))
    }

    /// 未创建页面时返回 None，避免把尚未进入的页面表述成空目录。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(in crate::features::preferences) fn acceptance_snapshot(
        &self,
        cx: &App,
    ) -> Option<crate::features::extensions::ExtensionsAcceptanceSnapshot> {
        self.page
            .as_ref()
            .map(|page| page.read(cx).acceptance_snapshot())
    }
}

impl PreferencesPanel {
    /// 注入共享扩展服务；必需字段保持为空，测试构造不必补齐新依赖。
    ///
    /// @author kongweiguang
    pub(crate) fn with_extensions(
        mut self,
        service: Option<Arc<ExtensionsService>>,
        host_api: String,
    ) -> Self {
        self.extensions = ExtensionsPreferencesState::new(service, self.executor.clone(), host_api);
        self
    }

    /// 命令面板和验收复用同一入口：打开设置、隐藏外部插件页并定位扩展分类。
    ///
    /// @author kongweiguang
    pub(crate) fn open_extensions_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_external_plugin(cx);
        if self.section != PreferencesSection::Extensions {
            self.cancel_shortcut_capture(cx);
            self.reset_page_disclosures();
        }
        self.section = PreferencesSection::Extensions;
        self.selected_navigation_target = None;
        self.extensions.activate(window, cx);
        cx.notify();
    }

    /// 正文只在页面存在时渲染已创建实体；服务缺失时给出稳定说明而非空白区域。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn extensions_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ExtensionsPage>> {
        self.extensions.ensure_page(window, cx)
    }

    /// 离开分类不清空筛选和草稿；再次进入时由导航显式触发重新加载。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn extensions_is_busy(&self, cx: &App) -> bool {
        self.extensions.is_busy(cx)
    }

    /// 验收打开设置弹框后定位同一生产页面，不另建中央内容宿主。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn extensions_acceptance_inspect_file(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.extensions.acceptance_inspect_file(path, cx)
    }

    /// 验收快照只读取已创建页面，未进入扩展分类时保持 None 语义。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn extensions_acceptance_snapshot(
        &self,
        cx: &App,
    ) -> Option<crate::features::extensions::ExtensionsAcceptanceSnapshot> {
        self.extensions.acceptance_snapshot(cx)
    }
}
