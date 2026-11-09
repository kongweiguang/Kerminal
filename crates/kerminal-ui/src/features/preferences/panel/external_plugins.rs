//! 外部设置页由偏好面板持有，导航切换不能释放草稿或绕过关闭回执。
//! @author kongweiguang

use super::{PreferencesEvent, PreferencesPanel};
use crate::plugins::{
    external::factories::ExternalViewServices,
    external_settings::{ExternalSettingsCloseState, ExternalSettingsPages},
};
use gpui_kit::{App, Context, Window};
use kerminal_application::{
    external_contributions::PreparedPlugin, plugin_settings::PluginSettingsService,
};
use kerminal_domain::plugins::PluginId;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(in crate::features::preferences) struct ExternalPreferencesState {
    pub(in crate::features::preferences) plugins: Vec<Arc<PreparedPlugin>>,
    service: Option<PluginSettingsService>,
    views: Option<ExternalViewServices>,
    pub(in crate::features::preferences) pages: ExternalSettingsPages,
    pub(in crate::features::preferences) selected: Option<PluginId>,
    closing: bool,
}

impl PreferencesPanel {
    /// 验收只复用正常设置导航，仍由真实表单接收键鼠事件。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn external_plugin_acceptance_open(
        &mut self,
        plugin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let id: PluginId = plugin
            .parse()
            .map_err(|_| kerminal_application::AppError::invalid("插件身份无效"))?;
        self.open_external_plugin(&id, window, cx);
        if self.external_plugins.selected.as_ref() == Some(&id) {
            Ok(())
        } else {
            Err(kerminal_application::AppError::conflict(
                "插件设置页面无法打开",
            ))
        }
    }

    /// 快照只暴露活动页和其现有门禁，避免验收直接改写用户设置。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn external_plugin_acceptance_snapshot(&self, cx: &App) -> serde_json::Value {
        let page = self
            .external_plugins
            .selected
            .as_ref()
            .and_then(|id| self.external_plugins.pages.page(id));
        serde_json::json!({
            "selected": self.external_plugins.selected.as_ref().map(PluginId::as_str),
            "closing": self.external_plugins.closing,
            "status": self.status,
            "page": page.map(|page| page.read(cx).external_plugin_acceptance_snapshot(cx)),
        })
    }

    /// 只注入启动期冻结的资源；实体按需建立，避免在绘制时访问磁盘或重建草稿。
    pub(crate) fn with_external_plugin_settings(
        mut self,
        plugins: Vec<Arc<PreparedPlugin>>,
        service: Option<PluginSettingsService>,
        views: Option<ExternalViewServices>,
    ) -> Self {
        self.external_plugins.plugins = plugins
            .into_iter()
            .filter(|plugin| plugin.package.manifest.contributes.settings.is_some())
            .collect();
        self.external_plugins.service = service;
        self.external_plugins.views = views;
        self
    }

    /// 以稳定插件身份缓存页面，切换时通知旧实例失焦，保留其草稿。
    pub(in crate::features::preferences) fn open_external_plugin(
        &mut self,
        id: &PluginId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.external_plugins.closing {
            return;
        }
        let Some(plugin) = self
            .external_plugins
            .plugins
            .iter()
            .find(|plugin| &plugin.id == id)
            .cloned()
        else {
            return;
        };
        let (Some(service), Some(views)) = (
            self.external_plugins.service.clone(),
            self.external_plugins.views.clone(),
        ) else {
            self.status = Some("插件设置服务不可用".into());
            cx.notify();
            return;
        };
        let plugin = match views.runtime.prepared_plugin(&plugin.id) {
            Ok(Some(latest)) => latest,
            Ok(None) => plugin,
            Err(error) => {
                self.status = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        match self
            .external_plugins
            .pages
            .get_or_open(plugin, service, views, window, cx)
        {
            Ok(page) => {
                self.hide_external_plugin(cx);
                self.cancel_shortcut_capture(cx);
                self.external_plugins.selected = Some(id.clone());
                self.selected_navigation_target = None;
                page.update(cx, |page, cx| page.visibility_changed(true, false, cx));
            }
            Err(error) => self.status = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 页面缓存与可见选择分开，离开时仅通知显隐，避免误丢未保存输入。
    pub(in crate::features::preferences) fn hide_external_plugin(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.external_plugins.selected.take() {
            if let Some(page) = self.external_plugins.pages.page(&id) {
                page.update(cx, |page, cx| page.visibility_changed(false, false, cx));
            }
            cx.notify();
        }
    }

    /// 汇总所有已打开页面；在途或待核对状态优先于普通草稿确认。
    pub(super) fn external_close_state(&self, cx: &App) -> ExternalSettingsCloseState {
        if self.external_plugins.closing {
            return ExternalSettingsCloseState::Working;
        }
        let mut dirty = false;
        for id in self.external_plugins.pages.mounted_plugins() {
            if let Some(page) = self.external_plugins.pages.page(&id) {
                match page.read(cx).close_state(cx) {
                    ExternalSettingsCloseState::Ready => {}
                    ExternalSettingsCloseState::UnsavedChanges => dirty = true,
                    state => return state,
                }
            }
        }
        if dirty {
            ExternalSettingsCloseState::UnsavedChanges
        } else {
            ExternalSettingsCloseState::Ready
        }
    }

    /// 先冻结全部页面，再以同一截止时间等关闭回执；失败保留未关闭实体并明确提示。
    pub(super) fn close_external_pages(
        &mut self,
        discard: bool,
        quit: bool,
        cx: &mut Context<Self>,
    ) {
        let ids = self.external_plugins.pages.mounted_plugins();
        if ids.is_empty() {
            if quit {
                cx.emit(PreferencesEvent::QuitConfirmed);
            } else {
                self.emit_leave_event(cx);
            }
            return;
        }
        self.hide_external_plugin(cx);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pending = Vec::new();
        for id in ids {
            match self
                .external_plugins
                .pages
                .begin_close(&id, deadline, discard, cx)
            {
                Ok(pair) => pending.push(pair),
                Err(error) => {
                    for (ticket, _) in pending {
                        let failure = Err(kerminal_application::AppError::conflict(
                            "插件页面关闭未启动",
                        ));
                        let _ = self.external_plugins.pages.finish_close(&ticket, &failure);
                    }
                    self.status = Some(error.to_string());
                    if quit {
                        cx.emit(PreferencesEvent::QuitDeferred {
                            message: error.to_string(),
                        });
                    }
                    cx.notify();
                    return;
                }
            }
        }
        self.external_plugins.closing = true;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            for (ticket, future) in pending {
                let result = executor.clone().run(move |_| future).await;
                let _ = this.update(cx, |panel, _| {
                    let _ = panel.external_plugins.pages.finish_close(&ticket, &result);
                    if let Err(error) = result {
                        panel.status = Some(error.to_string());
                    }
                });
            }
            let _ = this.update(cx, |panel, cx| {
                panel.external_plugins.closing = false;
                if panel.external_plugins.pages.mounted_plugins().is_empty() {
                    if quit {
                        cx.emit(PreferencesEvent::QuitConfirmed);
                    } else {
                        panel.emit_leave_event(cx);
                    }
                } else if quit {
                    cx.emit(PreferencesEvent::QuitDeferred {
                        message: panel
                            .status
                            .clone()
                            .unwrap_or_else(|| "插件设置关闭失败".into()),
                    });
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
