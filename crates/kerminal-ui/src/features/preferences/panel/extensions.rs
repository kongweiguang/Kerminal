//! 扩展页由设置弹框持有，服务仍由组合根注入，导航切换不重建异步状态。
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{App, AppContext as _, Context, Entity, Subscription, Window};
#[cfg(feature = "right-rail-acceptance")]
use kerminal_application::{AppError, AppResult};
use kerminal_application::{ApplicationTaskExecutor, extensions::ExtensionsService};

use super::{PreferencesEvent, PreferencesPanel, PreferencesSection};
use crate::features::extensions::{ExtensionsEvent, ExtensionsPage};

/// 设置弹框内只保留页面实体、进入激活与退出提示。
///
/// 页面创建需要窗口输入状态，只有实际进入扩展分类的导航路径才创建；
/// 设置启动或仅浏览其它分类不会触发插件目录读取。
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
    /// 退出被写操作拒绝时保留可见原因；写入结束后由正文清除。
    notice: Option<String>,
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
            notice: None,
        }
    }

    /// 页面宿主仍由设置弹框持有，重启请求经面板转交 Root，页面不自行结束进程。
    ///
    /// 只由显式导航调用；正文渲染不创建实体，避免绘制期启动后台任务。
    /// @author kongweiguang
    fn ensure_page(
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
                    // 写入结束可能清除退出提示，父面板需要重新计算正文包装。
                    ExtensionsEvent::WriteFinished => cx.notify(),
                },
            ));
            self.page = Some(page);
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
        if let Some(page) = self.ensure_page(window, cx) {
            page.update(cx, |page, cx| page.activate(cx));
        }
    }

    /// 离开扩展分类立即失效；迟到的文件预检被取消，已打开的确认由页面延迟关闭。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn deactivate(
        &mut self,
        cx: &mut Context<PreferencesPanel>,
    ) {
        if let Some(page) = self.page.clone() {
            page.update(cx, |page, cx| page.deactivate(cx));
        }
    }

    /// 只读访问已创建页面；未进入扩展分类时保持 None。
    fn page(&self) -> Option<Entity<ExtensionsPage>> {
        self.page.clone()
    }

    /// 只有写操作会改变插件目录和待重启集合，退出只需要等待这一阶段。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn is_writing(&self, cx: &App) -> bool {
        self.page
            .as_ref()
            .is_some_and(|page| page.read(cx).is_writing())
    }

    /// 关闭弹框不销毁页面，因此普通关闭不在这里拦截；该状态只服务于退出提示。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// 正文按写入状态决定是否显示提示，保留原因文本由渲染层决定显隐。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 写操作结束后清除上一次退出拒绝的原因，避免下一次写入误显示旧提示。
    pub(in crate::features::preferences) fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// 验收直接注入隔离包路径，后续预检、确认与安装仍复用生产页面流程。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    fn acceptance_inspect_file(
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
    fn acceptance_snapshot(
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

    /// 正文只读取已创建页面；服务缺失或尚未进入时由空态兜底。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn extensions_page(
        &self,
    ) -> Option<Entity<ExtensionsPage>> {
        self.extensions.page()
    }

    /// 验收打开入口用它确认页面确实创建，服务缺失时不能空等目录快照。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn extensions_page_exists(&self) -> bool {
        self.extensions.page().is_some()
    }

    /// 退出只等待写入完成；目录/Hub 的只读加载允许由窗口关闭流程终止。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn extensions_is_writing(&self, cx: &App) -> bool {
        self.extensions.is_writing(cx)
    }

    /// 退出被写入拒绝时在设置正文显示原因，写入结束后由渲染层隐藏并清除。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn set_extensions_notice(&mut self, notice: String) {
        self.extensions.set_notice(Some(notice));
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
