//! 共享 SFTP 工作区的主机选择状态与信任流程事件。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Entity, Subscription, Window,
    component::input::{InputEvent, InputState},
};
use kerminal_application::ApplicationTaskExecutor;
use kerminal_domain::{files::FileEndpoint, identity::HostId};
use tokio_util::sync::CancellationToken;

use crate::features::host_picker::HostPickerGroup;

use super::{FilesPane, SftpHostChoice, localized_error};

/// Root 负责消费事件并走既有 host key trust/connect 流程，FilesPane 不直接建连。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SftpHostRequested {
    pub host_id: HostId,
}

/// 紧凑文件栏请求 Root 按已验证的主机密钥流程信任该主机；不在 FilesPane 内建立连接。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SftpHostKeyTrustRequested {
    pub request_id: u64,
    pub host_id: HostId,
}

/// 请求 Root 打开现有连接管理创建流程，不在文件工作区直接编辑主机配置。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CreateSftpHostRequested;

/// 主机目录读取独立于当前两个文件 endpoint，且列表只缓存非凭据展示投影。
#[derive(Default)]
pub(super) struct HostPickerState {
    pub(super) enabled: bool,
    pub(super) expanded: bool,
    pub(super) right_target_unselected: bool,
    pub(super) hosts: Vec<SftpHostChoice>,
    pub(super) groups: Vec<HostPickerGroup>,
    pub(super) loaded: bool,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    pub(super) query: String,
    pub(super) generation: u64,
    cancellation: Option<CancellationToken>,
    pub(super) search_input: Option<Entity<InputState>>,
    search_subscription: Option<Subscription>,
}

impl Drop for HostPickerState {
    /// Pane 关闭或 picker 禁用时取消仍在途的配置目录读取。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.as_ref() {
            cancellation.cancel();
        }
    }
}

impl HostPickerState {
    /// 语言切换只更新占位，不重建输入实体或覆盖当前搜索和焦点。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_locale(&self, window: &mut Window, cx: &mut Context<FilesPane>) {
        if let Some(input) = self.search_input.as_ref() {
            input.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text(
                        "搜索主机、地址、协议、分组或用户名",
                        "Search hosts, addresses, protocols, groups, or usernames",
                    ),
                    window,
                    cx,
                )
            });
        }
    }
}

impl FilesPane {
    /// 仅共享中央文件工作区启用主机选择；紧凑文件栏和冻结目标传输 Tab 保持原目标绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn set_host_picker_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.host_picker.enabled == enabled {
            return;
        }
        self.host_picker.enabled = enabled;
        if enabled {
            self.host_picker.right_target_unselected =
                matches!(&self.panels[1].endpoint, FileEndpoint::Local { .. });
            self.host_picker.expanded = self.host_picker.right_target_unselected;
            if self.host_picker.right_target_unselected && self.initialized {
                self.invalidate_unselected_right_target();
            }
            self.reload_sftp_hosts(cx);
        } else {
            self.host_picker.expanded = false;
            self.host_picker.right_target_unselected = false;
            self.host_picker.query.clear();
            self.host_picker.groups.clear();
            self.host_picker.search_input = None;
            self.host_picker.search_subscription = None;
            if let Some(cancellation) = self.host_picker.cancellation.take() {
                cancellation.cancel();
            }
            self.host_picker.generation = self.host_picker.generation.wrapping_add(1);
            self.host_picker.loading = false;
        }
        cx.notify();
    }

    /// 懒创建搜索输入，保留 GPUI 输入法与焦点管理并用变更订阅更新本地筛选词。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_host_search_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<InputState>> {
        if !self.host_picker.enabled {
            return None;
        }
        if let Some(input) = self.host_picker.search_input.as_ref() {
            return Some(input.clone());
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索主机、地址、协议、分组或用户名",
                "Search hosts, addresses, protocols, groups, or usernames",
            ))
        });
        let subscription = cx.subscribe(&input, |pane, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                pane.host_picker.query = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        self.host_picker.search_input = Some(input.clone());
        self.host_picker.search_subscription = Some(subscription);
        Some(input)
    }

    /// 主机列表重试使用代次和协作取消拒绝迟到回执，不把配置读取放在 render 阶段。
    ///
    /// @author kongweiguang
    pub(super) fn reload_sftp_hosts(&mut self, cx: &mut Context<Self>) {
        if !self.host_picker.enabled {
            return;
        }
        if let Some(cancellation) = self.host_picker.cancellation.take() {
            cancellation.cancel();
        }
        self.host_picker.generation = self.host_picker.generation.wrapping_add(1).max(1);
        let generation = self.host_picker.generation;
        let cancellation = CancellationToken::new();
        self.host_picker.cancellation = Some(cancellation.clone());
        self.host_picker.loading = true;
        self.host_picker.error = None;
        let lookup = self.target_name_lookup.clone();
        let executor: ApplicationTaskExecutor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    lookup.list_sftp_hosts(operation_cancellation).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if !pane.host_picker.enabled || pane.host_picker.generation != generation {
                        return;
                    }
                    pane.host_picker.cancellation = None;
                    pane.host_picker.loading = false;
                    match result {
                        Ok(directory) => {
                            pane.host_picker.hosts = directory.hosts;
                            pane.host_picker.groups = directory
                                .groups
                                .into_iter()
                                .map(|group| HostPickerGroup {
                                    id: group.id,
                                    title: group.name.into(),
                                })
                                .collect();
                            pane.host_picker.loaded = true;
                            pane.host_picker.error = None;
                        }
                        Err(error) => pane.host_picker.error = Some(localized_error(&error)),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 受控 popover 开合沿用当前搜索草稿；重新打开时刷新主机目录并把焦点交回搜索输入。
    ///
    /// @author kongweiguang
    pub(super) fn set_host_picker_open(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.host_picker.enabled {
            return;
        }
        if self.host_picker.expanded == open {
            return;
        }
        self.host_picker.expanded = open;
        if open {
            // 保存主机由 Root 表单完成；每次重新展开刷新，使刚创建或外部变更的目标可见。
            self.reload_sftp_hosts(cx);
            if let Some(input) = self.host_picker.search_input.as_ref().cloned() {
                input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
        cx.notify();
    }

    /// HostId 只交回 Root；连接、known_hosts 提示和实际 endpoint 绑定统一由原流程完成。
    ///
    /// @author kongweiguang
    pub(super) fn request_sftp_host(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        self.host_picker.expanded = false;
        cx.emit(SftpHostRequested { host_id });
        cx.notify();
    }

    /// 创建主机入口回到 Root 的连接管理弹窗，避免在文件 workspace 复制一份配置写入状态机。
    ///
    /// @author kongweiguang
    pub(super) fn request_create_sftp_host(&mut self, cx: &mut Context<Self>) {
        self.host_picker.expanded = false;
        cx.emit(CreateSftpHostRequested);
        cx.notify();
    }

    /// 共享工作区初始右侧本机占位不是远端目标，因此不加载或保留重复的本机目录清单。
    ///
    /// @author kongweiguang
    fn invalidate_unselected_right_target(&mut self) {
        let panel = &mut self.panels[1];
        panel.generation = panel.generation.wrapping_add(1);
        panel.loading = false;
        panel.requested_endpoint = None;
        panel.listing = None;
        panel.selected = None;
        panel.selection.clear();
        panel.pending_selection = None;
        panel.pending_file_preview = false;
        panel.invalidate_preview_request();
        panel.tree.cancel_pending_requests();
        if self.active_side == 1 {
            self.preview = None;
        }
    }
}
