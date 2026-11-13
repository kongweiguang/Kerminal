//! 主机提供方的异步打开边界；配置与协议资源仍归应用服务所有。
//!
//! @author kongweiguang

use super::*;
use kerminal_application::connections::host_types::PluginHostOpenIntent;
use kerminal_domain::connections::RemoteProtocol;
use tokio_util::sync::{CancellationToken, DropGuard};

/// 请求 owner 被导航替换或窗口释放时传播取消，不把后台任务简单丢弃。
pub(super) struct PendingPluginHostOpen {
    request_id: String,
    navigation_generation: u64,
    _cancellation: DropGuard,
}

impl RootView {
    /// 本地主机仍由原有 profile 用例创建终端，目录存在时先经过与远程相同的提供方校验。
    /// 无目录仅供未装配插件服务的独立 UI 宿主使用，生产组合根始终注入完整目录。
    ///
    /// @author kongweiguang
    pub(super) fn open_saved_local_host(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.services.plugin_hosts.is_none() {
            self.create_terminal_for_target(
                super::connections::local_profile_target_request(profile_id),
                window,
                cx,
            );
            return;
        }
        match HostId::parse(profile_id) {
            Ok(host_id) => self.open_plugin_host(host_id, window, cx),
            Err(_) => {
                self.status = Some(crate::locale::text(
                    "本地主机标识无效，请刷新后重试。",
                    "The local host identity is invalid. Refresh and try again.",
                ));
                cx.notify();
            }
        }
    }

    /// 所有中央导航共用取消入口；已经受理的会话资源不在这里关闭。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_plugin_host_open(&mut self) {
        self.pending_plugin_host_open = None;
    }

    /// 内容标签受理后才收起主机抽屉；实例恢复失败仍展示原标签和重试入口。
    /// 抽屉仍持有焦点时在卸载后再次请求内容焦点，已转移的焦点不被关闭动作抢回。
    ///
    /// @author kongweiguang
    pub(super) fn open_plugin_host_content(
        &mut self,
        provider_id: &str,
        request: &crate::plugins::actions::OpenContentTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> kerminal_application::AppResult<()> {
        let declaration = self
            .services
            .plugins
            .content_type(&request.content_id)
            .ok_or_else(|| kerminal_application::AppError::not_found("内容提供方不可用"))?;
        // 声明与被禁用的归属单元分离保存；打开内容必须同时通过启用视图。
        if !self
            .services
            .plugins
            .content_type_enabled(&request.content_id)
        {
            return Err(kerminal_application::AppError::not_found(
                "内容提供方不可用",
            ));
        }
        if declaration.provider.as_str() != provider_id {
            return Err(kerminal_application::AppError::conflict(
                "主机不能打开其它提供方的内容",
            ));
        }
        let tab_id = self.open_plugin_tab(request, window, cx)?;
        if self.compact_host_drawer_open {
            if self.tool_rail_rendered_compact.0 && self.tool_rail_rendered_compact.1.is_some() {
                self.tool_rail_state
                    .hide_compact_drawer_for_tab(Some(&tab_id));
            }
            let drawer_had_focus = self
                .compact_drawer_focus_handle
                .contains_focused(window, cx);
            self.close_compact_drawer_for_center_navigation(window, cx);
            let scope = crate::plugins::content::ContentScope::Tab(tab_id);
            if drawer_had_focus
                && !self.content_failures.contains_key(&scope)
                && let Err(error) = self.content_host.focus(&scope, window, cx)
            {
                self.status = Some(error.public_message().to_owned().into());
            }
        }
        Ok(())
    }

    /// 只把已保存主机交给冻结提供方；收到结果后再次核对请求及导航身份。
    /// 提供方返回打开意图，不获得窗口、Root 或会话注册表的所有权。
    ///
    /// @author kongweiguang
    pub(super) fn open_plugin_host(
        &mut self,
        host_id: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested || self.tool_panel_overlay_focus_is_blocked(window, cx) {
            return;
        }
        let Some(service) = self.services.plugin_hosts.clone() else {
            self.status = Some(crate::locale::text(
                "主机提供方不可用。",
                "The host provider is unavailable.",
            ));
            cx.notify();
            return;
        };
        // 导航和 Root 可能处于不同刷新代次；打开前用当前快照复核插件启停，禁用时保留配置。
        let disabled_host = self
            .host_navigation
            .as_ref()
            .and_then(|navigation| navigation.read(cx).plugin_host_snapshot(&host_id))
            .filter(|host| !service.host_enabled(host));
        if disabled_host.is_some() {
            self.status = Some(crate::locale::text(
                "插件主机已禁用，配置仍保留供查看或迁移。",
                "This plugin host is disabled; its configuration is kept for review or migration.",
            ));
            cx.notify();
            return;
        }
        let cancellation = CancellationToken::new();
        let request_id = uuid::Uuid::new_v4().to_string();
        self.pending_plugin_host_open = Some(PendingPluginHostOpen {
            request_id: request_id.clone(),
            navigation_generation: self.surface_navigation_generation,
            _cancellation: cancellation.clone().drop_guard(),
        });
        let executor = self.services.executor.clone();
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let task_host = host_id.clone();
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    service.open(&task_host, cancel).await
                })
                .await;
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    let accepts = root
                        .pending_plugin_host_open
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.request_id == request_id
                                && pending.navigation_generation
                                    == root.surface_navigation_generation
                        });
                    if !accepts || root.host_shutdown_requested {
                        return;
                    }
                    root.pending_plugin_host_open = None;
                    let opened = result.and_then(|intent| match intent {
                        PluginHostOpenIntent::Builtin(intent) => {
                            if intent.host_id != host_id {
                                return Err(kerminal_application::AppError::conflict(
                                    "主机提供方返回了不匹配的目标",
                                ));
                            }
                            if intent.configuration_type == "local" {
                                root.selected_host_id = None;
                                root.selected_plugin_host_id = None;
                                root.refresh_operations_panel_targets(cx);
                                root.create_terminal_for_target(
                                    TerminalTargetRequest::ProfileLocal {
                                        profile_id: host_id.as_str().to_owned(),
                                    },
                                    window,
                                    cx,
                                );
                                return Ok(());
                            }
                            let protocol = match intent.configuration_type.as_str() {
                                "ssh" => RemoteProtocol::Ssh,
                                "sftp" => RemoteProtocol::Sftp,
                                "telnet" => RemoteProtocol::Telnet,
                                "serial" => RemoteProtocol::Serial,
                                "rdp" => RemoteProtocol::Rdp,
                                _ => {
                                    return Err(kerminal_application::AppError::unsupported(
                                        "内置主机提供方返回了未知协议",
                                    ));
                                }
                            };
                            let panel = root.connections_panel.clone().ok_or_else(|| {
                                kerminal_application::AppError::unsupported("主机连接服务不可用")
                            })?;
                            if !panel.update(cx, |panel, cx| {
                                panel.request_connect_builtin_host(&host_id, protocol, cx)
                            }) {
                                return Err(kerminal_application::AppError::conflict(
                                    "主机配置已变化，请刷新后重试",
                                ));
                            }
                            Ok(())
                        }
                        PluginHostOpenIntent::Terminal(target) => {
                            root.create_terminal_for_target(
                                TerminalTargetRequest::ExplicitHost { host_id, target },
                                window,
                                cx,
                            );
                            Ok(())
                        }
                        PluginHostOpenIntent::Content {
                            provider_id,
                            content_type_id,
                            title,
                            state_version,
                            state,
                        } => root.open_plugin_host_content(
                            &provider_id,
                            &crate::plugins::actions::OpenContentTab {
                                content_id: content_type_id,
                                title,
                                state_version,
                                state,
                            },
                            window,
                            cx,
                        ),
                        PluginHostOpenIntent::Rdp {
                            host_id: requested_host_id,
                        } => {
                            if requested_host_id != host_id {
                                return Err(kerminal_application::AppError::conflict(
                                    "主机提供方返回了不匹配的 RDP 目标",
                                ));
                            }
                            let panel = root.connections_panel.clone().ok_or_else(|| {
                                kerminal_application::AppError::unsupported("RDP 启动服务不可用")
                            })?;
                            if !panel.update(cx, |panel, cx| {
                                panel.request_connect_builtin_host(
                                    &requested_host_id,
                                    RemoteProtocol::Rdp,
                                    cx,
                                )
                            }) {
                                return Err(kerminal_application::AppError::conflict(
                                    "RDP 主机配置已变化，请刷新后重试",
                                ));
                            }
                            Ok(())
                        }
                        _ => Err(kerminal_application::AppError::unsupported(
                            "当前版本不支持此主机打开方式",
                        )),
                    });
                    if let Err(error) = opened {
                        root.status = Some(error.public_message().to_owned().into());
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }
}
