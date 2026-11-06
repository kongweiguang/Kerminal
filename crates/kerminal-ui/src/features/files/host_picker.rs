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

use crate::{
    features::host_picker::{HostPickerGroup, HostPickerListKeyboardState},
    locale,
};

use super::{FilesPane, FilesPaneEvent, SftpHostChoice, localized_error};

/// 两个浏览区域有独立请求与端点身份，不能用活动侧推断异步回执去向。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilePaneSide {
    Left,
    Right,
}

impl FilePaneSide {
    /// 固定索引与渲染顺序一致，不让调用方传入越界的任意整数。
    ///
    /// @author kongweiguang
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    /// 将现有双栏组件索引转换为具名侧别，未知索引显式拒绝。
    ///
    /// @author kongweiguang
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            _ => None,
        }
    }
}

/// Root 负责消费事件并走既有 host key trust/connect 流程，FilesPane 不直接建连。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SftpHostRequested {
    pub host_id: HostId,
    pub side: FilePaneSide,
    pub generation: u64,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateSftpHostRequested {
    pub side: FilePaneSide,
    pub generation: u64,
}

/// 主机目录读取独立于当前两个文件 endpoint，且列表只缓存非凭据展示投影。
#[derive(Default)]
pub(super) struct HostPickerState {
    pub(super) enabled: bool,
    pub(super) expanded_side: Option<FilePaneSide>,
    pub(super) target_unselected: [bool; 2],
    pub(super) target_pending: [bool; 2],
    pub(super) target_errors: [Option<String>; 2],
    pub(super) host_tabs: super::host_tabs::HostTabs,
    pub(super) target_generations: [u64; 2],
    requested_hosts: [Option<HostId>; 2],
    pub(super) restored_targets: bool,
    pub(super) targets_initialized: bool,
    pub(super) last_emitted_endpoints: Option<(FileEndpoint, Option<FileEndpoint>)>,
    pub(super) hosts: Vec<SftpHostChoice>,
    pub(super) groups: Vec<HostPickerGroup>,
    pub(super) loaded: bool,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    pub(super) query: [String; 2],
    pub(super) generation: u64,
    cancellation: Option<CancellationToken>,
    pub(super) search_inputs: [Option<Entity<InputState>>; 2],
    pub(super) keyboard_states: [Option<Entity<HostPickerListKeyboardState>>; 2],
    search_subscriptions: [Option<Subscription>; 2],
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
        for input in self.search_inputs.iter().flatten() {
            input.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text("搜索主机…", "Search hosts…"),
                    window,
                    cx,
                )
            });
        }
    }
}

impl FilesPane {
    /// 仅中央工作区启用主机选择；恢复标签的本机端点不能误判成共享工作台的未选占位。
    ///
    /// @author kongweiguang
    pub(crate) fn set_host_picker_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.host_picker.enabled == enabled {
            return;
        }
        self.host_picker.enabled = enabled;
        if enabled {
            if !self.host_picker.targets_initialized {
                self.host_picker.target_unselected = [
                    false,
                    !self.host_picker.restored_targets
                        && matches!(&self.panels[1].endpoint, FileEndpoint::Local { .. }),
                ];
                self.host_picker.targets_initialized = true;
            }
            self.initialize_host_tabs();
            self.host_picker.expanded_side = None;
            if self.host_picker.target_unselected[1] && self.initialized {
                self.invalidate_unselected_right_target();
            }
            self.reload_sftp_hosts(cx);
        } else {
            self.host_picker.expanded_side = None;
            for side in [FilePaneSide::Left, FilePaneSide::Right] {
                self.cancel_pending_host_selection(side, cx);
            }
            self.host_picker.query = [String::new(), String::new()];
            self.host_picker.groups.clear();
            self.host_picker.search_inputs = [None, None];
            self.host_picker.keyboard_states = [None, None];
            self.host_picker.search_subscriptions = [None, None];
            if let Some(cancellation) = self.host_picker.cancellation.take() {
                cancellation.cancel();
            }
            self.host_picker.generation = self.host_picker.generation.wrapping_add(1);
            self.host_picker.loading = false;
        }
        cx.notify();
    }

    /// 懒创建搜索输入，保留输入法与焦点管理；点击已展开输入时不重复重启目录读取。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_host_search_input(
        &mut self,
        side: FilePaneSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<InputState>> {
        if !self.host_picker.enabled {
            return None;
        }
        let index = side.index();
        if let Some(input) = self.host_picker.search_inputs[index].as_ref() {
            return Some(input.clone());
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("搜索主机…", "Search hosts…"))
        });
        let subscription = cx.subscribe(&input, move |pane, input, event, cx| match event {
            InputEvent::Change => {
                pane.host_picker.query[index] = input.read(cx).value().to_string();
                if pane.host_picker.enabled {
                    pane.host_picker.expanded_side = Some(side);
                }
                cx.notify();
            }
            InputEvent::Focus
                if pane.host_picker.enabled && pane.host_picker.expanded_side != Some(side) =>
            {
                pane.host_picker.expanded_side = Some(side);
                pane.reload_sftp_hosts(cx);
                cx.notify();
            }
            _ => {}
        });
        self.host_picker.search_inputs[index] = Some(input.clone());
        self.host_picker.search_subscriptions[index] = Some(subscription);
        Some(input)
    }

    /// 独立键盘状态同时服务搜索框和结果列表，左右不会争用高亮或滚动。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_host_keyboard_state(
        &mut self,
        side: FilePaneSide,
        cx: &mut Context<Self>,
    ) -> Entity<HostPickerListKeyboardState> {
        let index = side.index();
        if let Some(state) = self.host_picker.keyboard_states[index].as_ref() {
            return state.clone();
        }
        let state = cx.new(|_| HostPickerListKeyboardState::default());
        self.host_picker.keyboard_states[index] = Some(state.clone());
        state
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
                            pane.reconcile_host_tabs(cx);
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

    /// 双侧共享主机目录但各自保留搜索；旧关闭事件不能关闭另一侧弹层。
    ///
    /// @author kongweiguang
    pub(super) fn set_host_picker_open(
        &mut self,
        side: FilePaneSide,
        open: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.host_picker.enabled {
            return;
        }
        let next = if open {
            Some(side)
        } else if self.host_picker.expanded_side == Some(side) {
            None
        } else {
            return;
        };
        if self.host_picker.expanded_side == next {
            return;
        }
        self.host_picker.expanded_side = next;
        if open {
            // 保存主机由 Root 表单完成；重新展开刷新而不覆盖输入中的搜索草稿。
            self.reload_sftp_hosts(cx);
        }
        cx.notify();
    }

    /// 选择结果后清空搜索；Kit 的 set_value 不发送 Change，不会重新展开弹层。
    ///
    /// @author kongweiguang
    pub(super) fn clear_host_search(
        &mut self,
        side: FilePaneSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = side.index();
        self.host_picker.query[index].clear();
        if let Some(input) = self.host_picker.search_inputs[index].as_ref().cloned() {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    /// 主机预检前撤销相关侧尚未提交的传输；原浏览目标保留到连接成功。
    ///
    /// @author kongweiguang
    pub(super) fn request_sftp_host(
        &mut self,
        side: FilePaneSide,
        host_id: HostId,
        cx: &mut Context<Self>,
    ) {
        self.host_picker.expanded_side = None;
        let index = side.index();
        self.cancel_transfer_preparation_for_side(index, cx);
        self.cancel_pending_host_selection(side, cx);
        let generation = self.begin_host_selection(side);
        self.host_picker.requested_hosts[index] = Some(host_id.clone());
        cx.emit(SftpHostRequested {
            host_id,
            side,
            generation,
        });
        cx.notify();
    }

    /// 创建流程先撤销相关侧未提交的传输；主机配置仍由 Root 连接管理弹窗负责。
    ///
    /// @author kongweiguang
    pub(super) fn request_create_sftp_host(&mut self, side: FilePaneSide, cx: &mut Context<Self>) {
        self.host_picker.expanded_side = None;
        self.cancel_transfer_preparation_for_side(side.index(), cx);
        self.cancel_pending_host_selection(side, cx);
        let generation = self.begin_host_selection(side);
        cx.emit(CreateSftpHostRequested { side, generation });
        cx.notify();
    }

    /// 同侧的新选择取代旧请求；另一侧代次不受影响。
    ///
    /// @author kongweiguang
    fn begin_host_selection(&mut self, side: FilePaneSide) -> u64 {
        let index = side.index();
        let generation = self.host_picker.target_generations[index]
            .wrapping_add(1)
            .max(1);
        self.host_picker.target_generations[index] = generation;
        self.host_picker.target_pending[index] = true;
        self.host_picker.requested_hosts[index] = None;
        self.host_picker.target_errors[index] = None;
        generation
    }

    /// 清除该侧待完成的选择并推进代次，外部连接回执不能再覆盖后续导航。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate_host_selection(&mut self, side: FilePaneSide) {
        let index = side.index();
        self.host_picker.target_generations[index] = self.host_picker.target_generations[index]
            .wrapping_add(1)
            .max(1);
        self.host_picker.target_pending[index] = false;
        self.host_picker.requested_hosts[index] = None;
    }

    /// 用户改变该侧意图时通知 Root 取消对应预检，避免旧指纹模态阻塞新目标。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_pending_host_selection(
        &mut self,
        side: FilePaneSide,
        cx: &mut Context<Self>,
    ) {
        let index = side.index();
        let pending = self.host_picker.target_pending[index]
            .then_some(self.host_picker.target_generations[index]);
        self.invalidate_host_selection(side);
        if let Some(generation) = pending {
            cx.emit(FilesPaneEvent::CancelHostSelection { side, generation });
        }
    }

    /// Root 在连接预检或新建主机回执前查询同侧身份，避免关闭标签后的回投。
    ///
    /// @author kongweiguang
    pub(crate) fn host_selection_is_current(&self, side: FilePaneSide, generation: u64) -> bool {
        let index = side.index();
        self.host_picker.enabled
            && self.host_picker.target_pending[index]
            && self.host_picker.target_generations[index] == generation
    }

    /// 预检只允许绑定原侧原代次；第一次右侧选主机可沿用既有本机默认目录约定。
    /// 默认本机起点变化仍是导航重绑，必须拒绝先前目录写入的迟到展示。
    ///
    /// @author kongweiguang
    pub(crate) fn complete_host_selection(
        &mut self,
        side: FilePaneSide,
        generation: u64,
        endpoint: FileEndpoint,
        initial_local_directory: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.host_selection_is_current(side, generation) {
            return false;
        }
        let index = side.index();
        let FileEndpoint::Remote { host_id, .. } = &endpoint else {
            self.fail_host_selection(
                side,
                generation,
                locale::text(
                    "主机返回了无效的文件目标。",
                    "The host returned an invalid file target.",
                )
                .to_string(),
                cx,
            );
            return false;
        };
        if self.host_picker.requested_hosts[index]
            .as_ref()
            .is_some_and(|expected| expected != host_id)
        {
            self.fail_host_selection(
                side,
                generation,
                locale::text(
                    "连接结果与所选主机不匹配。",
                    "The connection did not match the selected host.",
                )
                .to_string(),
                cx,
            );
            return false;
        }
        let apply_local_start = side == FilePaneSide::Right
            && super::should_apply_shared_host_local_start(
                self.host_picker.enabled,
                self.host_picker.target_unselected[1],
                &self.panels[0].endpoint,
                self.panels[0].navigation_history.is_initial(),
                self.locked_left_host_id.is_some(),
            );
        if apply_local_start && let Some(path) = initial_local_directory {
            let local = FileEndpoint::Local { path: path.clone() };
            self.local_directories[0] = path;
            if self.panels[0].endpoint != local {
                self.invalidate_file_action_presentation(0);
                self.panels[0].endpoint = local.clone();
                self.panels[0].navigation_history.reset(local.clone());
                self.panels[0].listing = None;
                self.panels[0].selected = None;
                self.panels[0].selection.clear();
                self.panels[0].tree.cancel_pending_requests();
                self.load_panel_at(0, local, cx);
            }
        }
        self.host_picker.target_pending[index] = false;
        self.host_picker.requested_hosts[index] = None;
        self.bind_selected_endpoint(side, endpoint, cx);
        true
    }

    /// 失败只结束同代预检，原目录与另一侧选择保持可用。
    ///
    /// @author kongweiguang
    pub(crate) fn fail_host_selection(
        &mut self,
        side: FilePaneSide,
        generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if !self.host_selection_is_current(side, generation) {
            return;
        }
        let index = side.index();
        self.host_picker.target_pending[index] = false;
        self.host_picker.requested_hosts[index] = None;
        self.host_picker.target_errors[index] = Some(message.clone());
        self.status = Some(message);
        cx.notify();
    }

    /// 用户取消预检或关闭创建表单时只撤销对应侧的未提交选择。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_host_selection(
        &mut self,
        side: FilePaneSide,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self.host_selection_is_current(side, generation) {
            self.invalidate_host_selection(side);
            cx.notify();
        }
    }

    /// 本机选择不需要连接预检，但仍推进该侧代次以撤销之前的远端请求。
    ///
    /// @author kongweiguang
    pub(super) fn request_local_host(&mut self, side: FilePaneSide, cx: &mut Context<Self>) {
        self.host_picker.expanded_side = None;
        self.cancel_pending_host_selection(side, cx);
        self.host_picker.target_errors[side.index()] = None;
        let endpoint = FileEndpoint::Local {
            path: self.local_directories[side.index()].clone(),
        };
        self.bind_selected_endpoint(side, endpoint, cx);
    }

    /// 每次添加远端都建立独立标签；本机复用本侧已有标签，避免重复创建本机入口。
    ///
    /// @author kongweiguang
    pub(super) fn bind_selected_endpoint(
        &mut self,
        side: FilePaneSide,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        self.add_host_tab(side, endpoint, cx);
    }

    /// 发布去重后的活动目标与明确的右侧空态，持久化由 Root 负责。
    ///
    /// @author kongweiguang
    pub(super) fn emit_transfer_endpoints_changed(&mut self, cx: &mut Context<Self>) {
        if !self.host_picker.enabled || self.compact_layout || self.host_picker.target_unselected[0]
        {
            return;
        }
        let endpoints = (
            self.panels[0].endpoint.clone(),
            (!self.host_picker.target_unselected[1]).then(|| self.panels[1].endpoint.clone()),
        );
        if self.host_picker.last_emitted_endpoints.as_ref() == Some(&endpoints) {
            return;
        }
        self.host_picker.last_emitted_endpoints = Some(endpoints.clone());
        cx.emit(FilesPaneEvent::TransferEndpointsChanged {
            left: endpoints.0,
            right: endpoints.1,
        });
    }

    /// 共享工作区初始右侧本机占位不是远端目标，因此不加载或保留重复的本机目录清单。
    /// 占位失效也撤销写入回执展示资格，但后台写入仍持有自己的 busy 到完成。
    ///
    /// @author kongweiguang
    fn invalidate_unselected_right_target(&mut self) {
        self.invalidate_file_action_presentation(1);
        let panel = &mut self.panels[1];
        panel.generation = panel.generation.wrapping_add(1);
        panel.loading = false;
        panel.requested_endpoint = None;
        panel.directory_error = None;
        panel.failed_directory = None;
        panel.listing = None;
        panel.selected = None;
        panel.selection.clear();
        panel.pending_selection = None;
        panel.tree.cancel_pending_requests();
    }
}
