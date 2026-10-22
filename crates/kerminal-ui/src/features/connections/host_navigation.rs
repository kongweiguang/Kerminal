//! 左侧主机树；只发出带稳定 HostId 的意图，具体协议与信任流程留给完整连接面板。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, Focusable as _, IntoElement,
    Render, StatefulInteractiveElement, Subscription, Window,
    component::{
        Disableable, Icon, IconName,
        button::{Button, ButtonCustomVariant, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    ApplicationTaskExecutor, ConfigurationChangeService,
    configuration_changes::ConfigurationChangeMessage,
    connections::management::{
        HostDraft, HostGroup, HostGroupDraft, HostManagementService, ManagedHost,
    },
    preferences::{InterfaceDensity, ProfileService, VersionedProfile},
    workspace::WorkspaceTargetRef,
};
use kerminal_domain::{
    connections::RemoteProtocol,
    identity::HostId,
    preferences::{ConfigRevision, ConfigurationMode, ConfigurationRecoveryState},
};
use tokio_util::sync::CancellationToken;

use crate::features::workspace_controller::WorkspaceSplitTargetOption;
use crate::theme::ThemeTokens;

const DUPLICATE_OPEN_GUARD: Duration = Duration::from_millis(650);
const UNGROUPED_GROUP_ID: &str = "__ungrouped";

/// 导航侧栏只调整自身的可见密度，不改变共享字体令牌或主工作区的几何。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq)]
struct HostNavigationLayoutMetrics {
    row_height: f32,
    group_header_height: f32,
    content_gap: f32,
    tree_gap: f32,
    section_gap: f32,
    sidebar_padding: f32,
}

/// 密度映射保持两行文案可读，并让 compact/comfortable/spacious 都有真实布局差异。
/// @author kongweiguang
fn host_navigation_layout_metrics(density: InterfaceDensity) -> HostNavigationLayoutMetrics {
    match density {
        InterfaceDensity::Compact => HostNavigationLayoutMetrics {
            row_height: 46.,
            group_header_height: 24.,
            content_gap: 4.,
            tree_gap: 2.,
            section_gap: 2.,
            sidebar_padding: 4.,
        },
        InterfaceDensity::Comfortable => HostNavigationLayoutMetrics {
            row_height: 50.,
            group_header_height: 26.,
            content_gap: 6.,
            tree_gap: 4.,
            section_gap: 4.,
            sidebar_padding: 5.,
        },
        InterfaceDensity::Spacious => HostNavigationLayoutMetrics {
            row_height: 54.,
            group_header_height: 28.,
            content_gap: 8.,
            tree_gap: 6.,
            section_gap: 6.,
            sidebar_padding: 6.,
        },
    }
}

/// 左侧导航表达主机目录、主机选择、连接与 SFTP 意图；协议动作仍由 Root 交给现有面板。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostNavigationEvent {
    CreateLocalProfileRequested { group_id: Option<String> },
    LocalProfileSelected { profile_id: String },
    LocalProfileConnectRequested { profile_id: String },
    EditLocalProfileRequested { profile_id: String },
    CopyLocalProfileRequested { profile_id: String },
    DeleteLocalProfileRequested { profile_id: String },
    HostSelected { host_id: HostId },
    ShowHostDetailsRequested { host_id: HostId },
    EditHostRequested { host_id: HostId },
    CopyHostRequested { host_id: HostId },
    DeleteHostRequested { host_id: HostId },
    ConnectRequested { host_id: HostId },
    ActivateExistingSessionRequested { host_id: HostId },
    OpenSftpRequested { host_id: HostId },
    CreateSftpTransferTabRequested { host_id: HostId },
    OpenSftpWorkbenchRequested,
    CreateHostInGroupRequested { group_id: String },
    CreateGroupRequested,
    RenameGroupRequested { group_id: String },
    DeleteGroupRequested { group_id: String },
}

/// 保存确认后切换两个互斥的左栏选择身份，避免远端 Host 与本地 Profile 同时高亮。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
enum SavedNavigationTarget {
    Host(HostId),
    LocalProfile(String),
}

/// 将保存目标写入导航选择状态；只由明确保存/采用回执调用，列表刷新不会推断选择。
/// @author kongweiguang
fn apply_saved_navigation_target(
    selected_host: &mut Option<HostId>,
    selected_profile: &mut Option<String>,
    target: SavedNavigationTarget,
) {
    match target {
        SavedNavigationTarget::Host(host_id) => {
            *selected_host = Some(host_id);
            *selected_profile = None;
        }
        SavedNavigationTarget::LocalProfile(profile_id) => {
            *selected_host = None;
            *selected_profile = Some(profile_id);
        }
    }
}

/// 主机行只展示由工作区运行态推导出的会话阶段；未出现的主机表示没有可见会话。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSessionStatus {
    Opening,
    Ready,
    Failed,
}

/// Root 依据实际 pane/session 阶段生成的单主机状态投影，不代表网络可达性。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionProjection {
    pub host_id: HostId,
    pub status: HostSessionStatus,
}

#[derive(Clone, Copy)]
enum HostAction {
    Details,
    Edit,
    Copy,
    Delete,
}

/// 主机右键菜单根据当前 SFTP 能力裁剪动作；SFTP-only 无能力时不暴露不可执行的主动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostContextMenuActions {
    show_primary: bool,
    show_open_files: bool,
    show_transfer_tab: bool,
}

#[derive(Clone, Copy)]
enum LocalProfileAction {
    Edit,
    Copy,
    Delete,
}

#[derive(Clone, Copy)]
enum GroupAction {
    CreateHost,
    Rename,
    Delete,
}

/// 轻量导航只持有目录快照和筛选状态，不读取文件、不访问网络，也不保存连接秘密。
pub struct HostNavigation {
    hosts: HostManagementService,
    profiles: Option<ProfileService>,
    executor: ApplicationTaskExecutor,
    recovery: ConfigurationRecoveryState,
    density: InterfaceDensity,
    search: String,
    search_input: Entity<InputState>,
    _search_subscription: Subscription,
    rows: Vec<ManagedHost>,
    profile_rows: Vec<VersionedProfile>,
    groups: Vec<HostGroup>,
    group_revision: Option<ConfigRevision>,
    selected_host: Option<HostId>,
    selected_profile: Option<String>,
    session_projection: HashMap<HostId, HostSessionStatus>,
    collapsed_groups: HashSet<String>,
    pending_host_moves: HashSet<HostId>,
    pending_group_pins: HashSet<String>,
    last_open_request: Option<(HostId, Instant)>,
    last_profile_open_request: Option<(String, Instant)>,
    feedback: Option<NavigationFeedback>,
    loading: bool,
    status: Option<String>,
    reload_pending: bool,
    configuration_changes: Option<ConfigurationChangeService>,
    configuration_watch_cancellation: CancellationToken,
    configuration_watch_started: bool,
}

/// 只呈现最近一次分组移动回执，避免把普通连接列表替换成错误页。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationFeedback {
    message: String,
    is_error: bool,
}

impl EventEmitter<HostNavigationEvent> for HostNavigation {}

impl HostNavigation {
    /// 装配同一主机目录用例与共享执行器；首次加载设置前默认紧凑，避免侧栏短暂撑开。
    /// @author kongweiguang
    pub fn new(
        hosts: HostManagementService,
        executor: ApplicationTaskExecutor,
        recovery: ConfigurationRecoveryState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("搜索主机", "Search hosts"))
        });
        let search_subscription = cx.subscribe(&search_input, |navigation, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                navigation.search = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let mut navigation = Self {
            hosts,
            profiles: None,
            executor,
            recovery,
            density: InterfaceDensity::Compact,
            search: String::new(),
            search_input,
            _search_subscription: search_subscription,
            rows: Vec::new(),
            profile_rows: Vec::new(),
            groups: Vec::new(),
            group_revision: None,
            selected_host: None,
            selected_profile: None,
            session_projection: HashMap::new(),
            collapsed_groups: HashSet::new(),
            pending_host_moves: HashSet::new(),
            pending_group_pins: HashSet::new(),
            last_open_request: None,
            last_profile_open_request: None,
            feedback: None,
            loading: false,
            status: None,
            reload_pending: false,
            configuration_changes: None,
            configuration_watch_cancellation: CancellationToken::new(),
            configuration_watch_started: false,
        };
        navigation.reload(cx);
        navigation
    }

    /// 更新左栏搜索框缓存文案，同时保留原实体、搜索值和当前焦点。
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("搜索主机", "Search hosts"), window, cx);
        });
        cx.notify();
    }

    /// 接入同一 Profile 用例，让持久本地终端与远程主机共享分组导航但保留独立 schema。
    /// @author kongweiguang
    pub fn with_profiles(mut self, profiles: ProfileService, cx: &mut Context<Self>) -> Self {
        self.profiles = Some(profiles);
        self.reload(cx);
        self
    }

    /// Root 在设置快照变更后同步密度；叶子自行完成布局换算，避免把持久化值复制到主题全局。
    /// @author kongweiguang
    pub fn set_interface_density(&mut self, density: InterfaceDensity, cx: &mut Context<Self>) {
        if self.density == density {
            return;
        }
        self.density = density;
        cx.notify();
    }

    /// 注入 composition root 创建的共享配置 watcher，避免轻量导航另建文件监听服务。
    pub fn with_configuration_changes(mut self, service: ConfigurationChangeService) -> Self {
        self.configuration_changes = Some(service);
        self
    }

    /// 首次绘制时开始订阅；导航销毁时使用自身 token 停止等待而不影响其他面板。
    fn start_configuration_watch(&mut self, cx: &mut Context<Self>) {
        if self.configuration_watch_started {
            return;
        }
        let Some(service) = self.configuration_changes.clone() else {
            return;
        };
        self.configuration_watch_started = true;
        let mut subscription = service.subscribe();
        let cancellation = self.configuration_watch_cancellation.child_token();
        cx.spawn(async move |this, cx| {
            loop {
                match subscription.recv(cancellation.clone()).await {
                    ConfigurationChangeMessage::Changed(event) => {
                        let hosts_changed = event.full_rescan_required
                            || event
                                .relative_paths
                                .iter()
                                .any(|path| is_navigation_configuration_path(path));
                        let _ = this.update(cx, |navigation, cx| {
                            navigation.recovery = event.recovery_state;
                            if hosts_changed {
                                navigation.reload(cx);
                            } else {
                                cx.notify();
                            }
                        });
                    }
                    ConfigurationChangeMessage::RescanRequired => {
                        let recovery = service.recovery_state();
                        let _ = this.update(cx, |navigation, cx| {
                            navigation.recovery = recovery;
                            navigation.reload(cx);
                        });
                    }
                    ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed => {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// 读取主机与分组作为同一 UI 快照；失败保留旧数据并提供显式重试。
    /// @author kongweiguang
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.reload_pending = true;
            return;
        }
        self.loading = true;
        let hosts = self.hosts.clone();
        let profiles = self.profiles.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let rows = hosts.list_hosts().await?;
                    let groups = hosts.load_groups().await?;
                    let profiles = match profiles {
                        Some(profiles) => profiles.list_profiles().await?,
                        None => Vec::new(),
                    };
                    Ok((rows, groups, profiles))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |navigation, cx| {
                    navigation.loading = false;
                    match result {
                        Ok((rows, groups, profiles)) => {
                            navigation.rows = rows;
                            navigation.groups = groups.groups;
                            navigation.group_revision = groups.revision;
                            navigation.profile_rows = profiles;
                            if navigation.selected_host.as_ref().is_some_and(|selected| {
                                !navigation.rows.iter().any(|host| &host.id == selected)
                            }) {
                                navigation.selected_host = None;
                            }
                            if navigation
                                .selected_profile
                                .as_ref()
                                .is_some_and(|selected| {
                                    !navigation
                                        .profile_rows
                                        .iter()
                                        .any(|row| &row.profile.id == selected)
                                })
                            {
                                navigation.selected_profile = None;
                            }
                            navigation.status = None;
                        }
                        Err(error) => navigation.status = Some(load_error_message(&error)),
                    }
                    cx.notify();
                    if navigation.reload_pending {
                        navigation.reload_pending = false;
                        navigation.reload(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 只从当前已加载的主机投影返回显示名；缺失时返回 None，避免把内部 HostId 当标题。
    pub fn display_name_for(&self, host_id: &HostId) -> Option<gpui_kit::SharedString> {
        self.rows
            .iter()
            .find(|host| &host.id == host_id)
            .map(|host| host.name.clone().into())
    }

    /// 复制当前已加载主机的稳定 ID→显示名投影，供 Operations 标题与多主机转发行使用。
    ///
    /// @author kongweiguang
    pub fn display_names(&self) -> HashMap<HostId, gpui_kit::SharedString> {
        self.rows
            .iter()
            .map(|host| (host.id.clone(), host.name.clone().into()))
            .collect()
    }

    /// 仅从已加载的 Profile 投影取可读标题，缺失项由恢复用例显式报错。
    /// @author kongweiguang
    pub fn display_profile_name_for(&self, profile_id: &str) -> Option<gpui_kit::SharedString> {
        self.profile_rows
            .iter()
            .find(|row| row.profile.id == profile_id)
            .map(|row| row.profile.name.clone().into())
    }

    /// 只使用稳定目录快照中的唯一默认 Profile；刷新期间不把可能过期的标题用于新标签。
    ///
    /// @author kongweiguang
    pub fn display_default_profile_name(&self) -> Option<gpui_kit::SharedString> {
        if self.loading {
            return None;
        }
        unique_default_profile_name(&self.profile_rows)
    }

    /// 分屏选择器复用导航的过滤规则，但只投影终端目标 ID、名称和端点摘要，避免复制完整连接配置。
    ///
    /// @author kongweiguang
    pub(crate) fn split_target_options(&self, query: &str) -> Vec<WorkspaceSplitTargetOption> {
        let mut options = split_target_hosts(&self.rows, &self.groups, query)
            .into_iter()
            .filter_map(|host| {
                let (target, protocol_label) = match host.protocol {
                    RemoteProtocol::Ssh => (
                        WorkspaceTargetRef::Ssh {
                            host_id: host.id.clone(),
                        },
                        "SSH",
                    ),
                    RemoteProtocol::Telnet => (
                        WorkspaceTargetRef::Telnet {
                            host_id: host.id.clone(),
                        },
                        "Telnet",
                    ),
                    RemoteProtocol::Serial => (
                        WorkspaceTargetRef::Serial {
                            host_id: host.id.clone(),
                        },
                        "Serial",
                    ),
                    RemoteProtocol::Sftp | RemoteProtocol::Rdp => return None,
                };
                Some(WorkspaceSplitTargetOption {
                    target,
                    title: host.name,
                    detail: Some(format!(
                        "{protocol_label} · {}@{}:{}",
                        host.endpoint.username, host.endpoint.host, host.endpoint.port
                    )),
                    group_id: host.group_id,
                })
            })
            .collect::<Vec<_>>();
        options.extend(
            filtered_profiles(&self.profile_rows, &self.groups, query)
                .into_iter()
                .map(|row| WorkspaceSplitTargetOption {
                    target: WorkspaceTargetRef::Local {
                        profile_id: Some(row.profile.id.clone()),
                    },
                    title: row.profile.name,
                    detail: Some(row.profile.shell),
                    group_id: row.profile.sidebar_group_id,
                }),
        );
        options
    }

    /// 复用主机导航当前分组快照，使同一个 selector 能按已存在分组呈现目标。
    ///
    /// @author kongweiguang
    pub(crate) fn split_target_groups(&self) -> Vec<HostGroup> {
        self.groups.clone()
    }

    /// 选择器区分目录加载中的空状态与确实没有可用终端目标。
    ///
    /// @author kongweiguang
    pub(crate) fn split_target_loading(&self) -> bool {
        self.loading
    }

    /// 失败状态只传递公开文案；候选选择不会自行重新连接或回退到其它目标。
    ///
    /// @author kongweiguang
    pub(crate) fn split_target_status(&self) -> Option<String> {
        self.status.clone()
    }

    /// 把稳定本地主机 ID 交给 Root 的工作区恢复路径，不在导航中复制启动参数。
    /// @author kongweiguang
    fn request_local_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        if !self
            .profile_rows
            .iter()
            .any(|row| row.profile.id == profile_id)
        {
            self.status = Some(
                crate::locale::text(
                    "本地主机已不存在，请刷新列表。",
                    "This local host no longer exists. Refresh the list.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self
            .last_profile_open_request
            .as_ref()
            .is_some_and(|(last_id, at)| {
                last_id == &profile_id && at.elapsed() < DUPLICATE_OPEN_GUARD
            })
        {
            return;
        }
        self.last_profile_open_request = Some((profile_id.clone(), Instant::now()));
        cx.emit(HostNavigationEvent::LocalProfileConnectRequested { profile_id });
    }

    /// 本地主机单击只改变导航焦点；Root 清除旧远程运维目标，双击才创建会话。
    /// @author kongweiguang
    fn select_local_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        if !self
            .profile_rows
            .iter()
            .any(|row| row.profile.id == profile_id)
        {
            return;
        }
        self.selected_profile = Some(profile_id.clone());
        self.selected_host = None;
        self.feedback = None;
        cx.emit(HostNavigationEvent::LocalProfileSelected { profile_id });
        cx.notify();
    }

    /// 在指定分组新建本地主机；Root 将其路由到统一连接表单而非偏好页。
    /// @author kongweiguang
    fn request_create_local_profile(&mut self, group_id: Option<String>, cx: &mut Context<Self>) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置只读恢复中，暂不能新建本地主机。",
                    "Configuration is read-only during recovery; a local host cannot be created.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if group_id
            .as_ref()
            .is_some_and(|id| !self.groups.iter().any(|group| &group.id == id))
        {
            self.status = Some(
                crate::locale::text(
                    "分组已变化，请刷新后重试。",
                    "The group changed. Refresh and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        cx.emit(HostNavigationEvent::CreateLocalProfileRequested { group_id });
    }

    /// 右键菜单项只发送稳定 Profile 身份，不先切换导航焦点，避免操作对象与当前工作区脱节。
    /// Root 仍复用统一本地主机表单与确认/删除事务。
    /// @author kongweiguang
    fn request_local_profile_action(
        &mut self,
        profile_id: String,
        action: LocalProfileAction,
        cx: &mut Context<Self>,
    ) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "配置处于只读恢复模式，本地主机仅供查看。",
                    "Local hosts are view-only during configuration recovery.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        if !self
            .profile_rows
            .iter()
            .any(|row| row.profile.id == profile_id)
        {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "本地主机已不存在，请刷新列表后重试。",
                    "This local host no longer exists. Refresh the list and try again.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return;
        }
        cx.emit(local_profile_action_event(profile_id, action));
    }

    /// 使用 Root 从真实工作区 pane 汇总的状态更新行标记；相同快照不触发重绘循环。
    pub fn set_session_projection(
        &mut self,
        projection: Vec<HostSessionProjection>,
        cx: &mut Context<Self>,
    ) {
        let mut next: HashMap<HostId, HostSessionStatus> = HashMap::new();
        for item in projection {
            next.entry(item.host_id)
                .and_modify(|status| *status = merge_session_status(*status, item.status))
                .or_insert(item.status);
        }
        if self.session_projection == next {
            return;
        }
        self.session_projection = next;
        cx.notify();
    }

    /// 选择主机时同步本地焦点和运维目标，事件只包含显式主机身份。
    fn select_host(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        self.selected_host = Some(host_id.clone());
        self.selected_profile = None;
        self.feedback = None;
        cx.emit(HostNavigationEvent::HostSelected { host_id });
        cx.notify();
    }

    /// 明确保存远端主机后按 HostId 选中并重读目录，避免依赖旧列表快照或普通 reload 推断选择。
    /// @author kongweiguang
    pub(crate) fn select_saved_host(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        apply_saved_navigation_target(
            &mut self.selected_host,
            &mut self.selected_profile,
            SavedNavigationTarget::Host(host_id),
        );
        self.feedback = None;
        self.reload(cx);
        cx.notify();
    }

    /// 明确保存本地主机后按 ProfileId 选中；本地目标清除远端 Host 选择。
    /// @author kongweiguang
    pub(crate) fn select_saved_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        apply_saved_navigation_target(
            &mut self.selected_host,
            &mut self.selected_profile,
            SavedNavigationTarget::LocalProfile(profile_id),
        );
        self.feedback = None;
        self.reload(cx);
        cx.notify();
    }

    /// 双击先复用活动会话，否则才进入连接面板预检；稳定身份和节流避免误建重复资源。
    /// @author kongweiguang
    fn request_connect(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        if self.suppress_duplicate_open(&host_id) {
            return;
        }
        self.select_host(host_id.clone(), cx);
        if matches!(
            self.session_projection.get(&host_id),
            Some(HostSessionStatus::Ready | HostSessionStatus::Opening)
        ) {
            cx.emit(HostNavigationEvent::ActivateExistingSessionRequested { host_id });
            return;
        }
        match primary_host_action(
            self.rows
                .iter()
                .find(|host| host.id == host_id)
                .map(|host| host.protocol)
                .unwrap_or(RemoteProtocol::Ssh),
        ) {
            HostPrimaryAction::Connect => {
                cx.emit(HostNavigationEvent::ConnectRequested { host_id })
            }
            HostPrimaryAction::OpenSftp => {
                cx.emit(HostNavigationEvent::OpenSftpRequested { host_id })
            }
        }
    }

    /// 菜单闭包可能捕获旧目录快照；每次执行前按当前 HostId 和已启用能力复核，避免热更新后误发请求。
    ///
    /// @author kongweiguang
    fn request_open_sftp(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        if !self.ensure_host_sftp_available(&host_id, cx) {
            return;
        }
        if self.suppress_duplicate_open(&host_id) {
            return;
        }
        self.select_host(host_id.clone(), cx);
        cx.emit(HostNavigationEvent::OpenSftpRequested { host_id });
    }

    /// 独立传输 Tab 复用现有信任预检；菜单可能滞留在旧快照上，故先复核当前 SFTP 能力。
    ///
    /// @author kongweiguang
    fn request_create_sftp_transfer_tab(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        if !self.ensure_host_sftp_available(&host_id, cx) {
            return;
        }
        if self.suppress_duplicate_open(&host_id) {
            return;
        }
        self.select_host(host_id.clone(), cx);
        cx.emit(HostNavigationEvent::CreateSftpTransferTabRequested { host_id });
    }

    /// 使用最新目录快照校验菜单目标；禁用或已删除的主机不向 Root 发出 SFTP 意图。
    ///
    /// @author kongweiguang
    fn ensure_host_sftp_available(&mut self, host_id: &HostId, cx: &mut Context<Self>) -> bool {
        let Some(host) = self.rows.iter().find(|host| &host.id == host_id) else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "主机已不存在，未执行 SFTP 操作。",
                    "This host no longer exists; no SFTP action was performed.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return false;
        };
        if host_context_menu_actions(host).show_transfer_tab {
            return true;
        }

        self.feedback = Some(NavigationFeedback {
            message: if matches!(host.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
                crate::locale::text(
                    "此主机已禁用 SFTP，可在文件传输设置中启用。",
                    "SFTP is disabled for this host. Enable it in file transfer settings.",
                )
            } else {
                crate::locale::text(
                    "此协议不支持 SFTP。",
                    "This protocol does not support SFTP.",
                )
            }
            .to_string(),
            is_error: true,
        });
        cx.notify();
        false
    }

    /// 迟到的工作区状态投影到达前，短暂抑制同一主机的重复双击和重复菜单提交。
    /// @author kongweiguang
    fn suppress_duplicate_open(&mut self, host_id: &HostId) -> bool {
        if self
            .last_open_request
            .as_ref()
            .is_some_and(|(last_id, at)| last_id == host_id && at.elapsed() < DUPLICATE_OPEN_GUARD)
        {
            return true;
        }
        self.last_open_request = Some((host_id.clone(), Instant::now()));
        false
    }

    /// 主机管理菜单只发送稳定身份事件，由 Root 调用完整面板用例而不复制业务逻辑。
    fn request_host_action(&mut self, host_id: HostId, action: HostAction, cx: &mut Context<Self>) {
        self.select_host(host_id.clone(), cx);
        let event = match action {
            HostAction::Details => HostNavigationEvent::ShowHostDetailsRequested { host_id },
            HostAction::Edit => HostNavigationEvent::EditHostRequested { host_id },
            HostAction::Copy => HostNavigationEvent::CopyHostRequested { host_id },
            HostAction::Delete => HostNavigationEvent::DeleteHostRequested { host_id },
        };
        cx.emit(event);
    }

    /// 分组写操作只接受真实 groups.toml 条目，避免把收藏或未分组筛选当作可写实体。
    fn request_group_action(
        &mut self,
        group_id: String,
        action: GroupAction,
        cx: &mut Context<Self>,
    ) {
        if !self.groups.iter().any(|group| group.id == group_id) {
            return;
        }
        let event = match action {
            GroupAction::CreateHost => HostNavigationEvent::CreateHostInGroupRequested { group_id },
            GroupAction::Rename => HostNavigationEvent::RenameGroupRequested { group_id },
            GroupAction::Delete => HostNavigationEvent::DeleteGroupRequested { group_id },
        };
        cx.emit(event);
    }

    /// 置顶沿用负 sort_order 的现有兼容约定，并以 groups.toml 整份 revision 做 CAS；完成后回读确认最终状态。
    /// @author kongweiguang
    fn request_group_pin(&mut self, group_id: String, pinned: bool, cx: &mut Context<Self>) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "配置只读恢复中，不能调整分组置顶状态。",
                    "Groups cannot be pinned during read-only configuration recovery.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        if self.pending_group_pins.contains(&group_id) {
            return;
        }
        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .cloned()
        else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "分组已不存在，请刷新列表后重试。",
                    "This group no longer exists. Refresh the list and try again.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return;
        };
        let Some(sort_order) = group_sort_order_after_pin(&self.groups, &group_id, pinned) else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "分组排序值已到边界，不能调整置顶顺序。",
                    "The group sort order reached its limit and cannot be changed.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        };
        if sort_order == group.sort_order {
            return;
        }
        let Some(expected_revision) = self.group_revision.clone() else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "分组版本不可用，请刷新列表后重试。",
                    "The group revision is unavailable. Refresh the list and try again.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return;
        };

        self.pending_group_pins.insert(group_id.clone());
        self.feedback = None;
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        let draft = HostGroupDraft {
            id: Some(group.id.clone()),
            name: group.name.clone(),
            sort_order,
        };
        let group_name = group.name;
        cx.spawn(async move |this, cx| {
            let execution_result = executor
                .run(move |_| async move {
                    let saved = hosts.save_group(draft, Some(expected_revision)).await;
                    let readback = hosts.load_groups().await;
                    Ok((saved, readback))
                })
                .await;
            let _ = this.update(cx, |navigation, cx| {
                navigation.pending_group_pins.remove(&group_id);
                match execution_result {
                    Ok((save_result, readback_result)) => match readback_result {
                        Ok(snapshot) => {
                            let persisted = snapshot
                                .groups
                                .iter()
                                .find(|group| group.id == group_id)
                                .is_some_and(|group| is_group_pinned(group.sort_order) == pinned);
                            navigation.groups = snapshot.groups;
                            navigation.group_revision = snapshot.revision;
                            navigation.feedback = Some(if persisted {
                                NavigationFeedback {
                                    message: if pinned {
                                        crate::locale::text(
                                            format!("「{group_name}」已置顶。"),
                                            format!("‘{group_name}’ is pinned."),
                                        )
                                    } else {
                                        crate::locale::text(
                                            format!("「{group_name}」已取消置顶。"),
                                            format!("‘{group_name}’ is unpinned."),
                                        )
                                    }
                                    .to_string(),
                                    is_error: false,
                                }
                            } else {
                                NavigationFeedback {
                                    message: save_result.err().map_or_else(
                                        || crate::locale::text(
                                            "置顶状态未能确认，请刷新后重试。",
                                            "The pin state could not be confirmed. Refresh and try again.",
                                        ).to_string(),
                                        |error| error.public_message().to_owned(),
                                    ),
                                    is_error: true,
                                }
                            });
                        }
                        Err(readback_error) => {
                            let message = save_result
                                .err()
                                .map(|error| error.public_message().to_owned())
                                .unwrap_or_else(|| load_error_message(&readback_error));
                            navigation.feedback = Some(NavigationFeedback {
                                message,
                                is_error: true,
                            });
                            navigation.reload(cx);
                        }
                    },
                    Err(error) => {
                        let public_message = error.public_message().to_owned();
                        navigation.feedback = Some(NavigationFeedback {
                            message: crate::locale::text(
                                format!("分组置顶结果未知：{public_message}"),
                                format!("The group pin result is unknown: {public_message}"),
                            )
                            .to_string(),
                            is_error: true,
                        });
                        navigation.reload(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 按稳定 HostId 二次确认网络协议与非空地址，避免串口路径或空值覆盖剪贴板。
    /// @author kongweiguang
    fn request_copy_host_address(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        let Some(host) = self.rows.iter().find(|host| host.id == host_id) else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "主机已不存在，请刷新列表后重试。",
                    "This host no longer exists. Refresh the list and try again.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return;
        };
        if host.protocol == RemoteProtocol::Serial || host.endpoint.host.trim().is_empty() {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "此主机没有可复制的网络地址。",
                    "This host has no network address to copy.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        let address = host.endpoint.host.trim().to_owned();
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(address));
        self.feedback = Some(NavigationFeedback {
            message: crate::locale::text("已复制主机地址。", "Host address copied.").to_string(),
            is_error: false,
        });
        cx.notify();
    }

    /// 通过 application 的 HostDraft 与所见 revision 做单文件 CAS；失败后刷新核对，不盲目重放。
    /// @author kongweiguang
    fn request_move_host(
        &mut self,
        host_id: HostId,
        destination: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "配置处于只读恢复模式，不能移动主机。",
                    "Hosts cannot be moved during read-only recovery.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        if destination
            .as_ref()
            .is_some_and(|id| !self.groups.iter().any(|group| &group.id == id))
        {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "目标分组已变化，请刷新后重试。",
                    "The destination group changed. Refresh and try again.",
                )
                .to_string(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        let Some(host) = self.rows.iter().find(|host| host.id == host_id).cloned() else {
            self.feedback = Some(NavigationFeedback {
                message: crate::locale::text(
                    "主机已不存在，请刷新列表。",
                    "This host no longer exists. Refresh the list.",
                )
                .to_string(),
                is_error: true,
            });
            self.reload(cx);
            cx.notify();
            return;
        };
        if host.group_id == destination || !self.pending_host_moves.insert(host_id.clone()) {
            return;
        }
        self.select_host(host_id.clone(), cx);

        let destination_name = destination.as_ref().map_or_else(
            || crate::locale::text("未分组", "Ungrouped").to_string(),
            |id| {
                self.groups
                    .iter()
                    .find(|group| &group.id == id)
                    .map(|group| group.name.clone())
                    .unwrap_or_else(|| id.clone())
            },
        );
        let draft = host_draft_for_group(&host, destination);
        let expected_revision = host.revision;
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { hosts.save_host(draft, Some(expected_revision)).await })
                .await;
            let _ = this.update(cx, |navigation, cx| {
                navigation.pending_host_moves.remove(&host_id);
                match result {
                    Ok(_) => {
                        navigation.feedback = Some(NavigationFeedback {
                            message: crate::locale::text(
                                format!("主机已移至「{destination_name}」。"),
                                format!("Host moved to ‘{destination_name}’."),
                            )
                            .to_string(),
                            is_error: false,
                        });
                    }
                    Err(error) => {
                        navigation.feedback = Some(NavigationFeedback {
                            message: error.public_message().to_owned(),
                            is_error: true,
                        });
                    }
                }
                navigation.reload(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 分组折叠只影响当前视图，不修改主机分组或工作区持久化状态。
    fn toggle_group(&mut self, group_id: String, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(&group_id) {
            self.collapsed_groups.insert(group_id);
        }
        cx.notify();
    }

    /// 一次性展开或折叠已保存分组；折叠状态只在导航实体内生效，不写入连接配置。
    /// @author kongweiguang
    fn toggle_all_groups(&mut self, cx: &mut Context<Self>) {
        let group_ids = navigation_group_ids(&self.rows, &self.profile_rows, &self.groups);
        if group_ids.is_empty() {
            return;
        }
        if all_groups_are_collapsed(&group_ids, &self.collapsed_groups) {
            self.collapsed_groups.clear();
        } else {
            self.collapsed_groups = group_ids.into_iter().collect();
        }
        cx.notify();
    }
}

impl Render for HostNavigation {
    /// 搜索框由外层主题 frame 独占边界并关闭 Kit 内框，避免焦点时内外双重描边；容器目标仍绑定活动终端。
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_configuration_watch(cx);
        let theme = ThemeTokens::current(window, cx);
        let metrics = host_navigation_layout_metrics(self.density);
        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let mut content = div()
            .id("host-navigation")
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(metrics.content_gap))
            .p(px(metrics.sidebar_padding))
            .overflow_hidden()
            .bg(theme.materials.chrome)
            .text_color(theme.foreground)
            .child(render_navigation_header(self, theme, cx));

        let filtered = filtered_hosts(&self.rows, &self.groups, self.search.as_str());
        let filtered_profiles =
            filtered_profiles(&self.profile_rows, &self.groups, self.search.as_str());
        content = content.child(
            div()
                .id("host-navigation-search-frame")
                .w_full()
                .h(px(34.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .px_2()
                .rounded_lg()
                .border_1()
                .border_color(if search_focused {
                    theme.accent
                } else {
                    theme.border
                })
                .bg(if search_focused {
                    theme.panel
                } else {
                    theme.secondary.opacity(0.18)
                })
                .hover(|style| {
                    style
                        .border_color(theme.accent.opacity(0.55))
                        .bg(theme.panel)
                })
                .child(
                    Input::new(&self.search_input)
                        .appearance(false)
                        .w_full()
                        .min_w(px(0.))
                        .prefix(div().text_color(theme.subtle).child(IconName::Search)),
                ),
        );
        // 空分组是用户保存的导航对象；无成员时仍须保留可右键管理的入口。
        let has_visible_saved_group = self.groups.iter().any(|group| {
            should_render_saved_group_header(&group.name, self.search.as_str(), false)
        });
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            content = content.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(theme.secondary)
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(crate::locale::text(
                        "配置只读恢复中；主机目录仅供查看。",
                        "Configuration is in read-only recovery mode; hosts are view-only.",
                    )),
            );
        }

        let mut host_tree = div()
            .id("host-navigation-tree")
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap(px(metrics.tree_gap))
            .overflow_y_scroll();

        if self.loading && self.rows.is_empty() && self.profile_rows.is_empty() {
            host_tree = host_tree.child(
                div()
                    .w_full()
                    .p_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_size(theme.text_sm)
                    .text_color(theme.muted)
                    .child(IconName::LoaderCircle)
                    .child(crate::locale::text("正在读取主机目录…", "Loading hosts…")),
            );
        } else if let Some(status) = self.status.clone() {
            host_tree = host_tree.child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_color(theme.error)
                    .child(status)
                    .child(
                        Button::new("host-navigation-retry")
                            .compact()
                            .label(crate::locale::text("重试", "Retry"))
                            .on_click(cx.listener(|navigation, _: &ClickEvent, _, cx| {
                                navigation.reload(cx)
                            })),
                    ),
            );
        } else if filtered.is_empty() && filtered_profiles.is_empty() && !has_visible_saved_group {
            host_tree = host_tree.child(empty_state(
                self.rows.is_empty()
                    && self.profile_rows.is_empty()
                    && self.search.trim().is_empty(),
                self.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
                theme,
            ));
        } else {
            host_tree = render_tree_groups(
                self,
                &filtered,
                &filtered_profiles,
                theme,
                metrics,
                cx,
                host_tree,
            );
        }
        if let Some(feedback) = self.feedback.as_ref() {
            content = content.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(if feedback.is_error {
                        theme.error.opacity(0.12)
                    } else {
                        theme.secondary
                    })
                    .text_size(theme.text_xs)
                    .text_color(if feedback.is_error {
                        theme.error
                    } else {
                        theme.muted
                    })
                    .child(feedback.message.clone()),
            );
        }
        content = content.child(host_tree);

        content
    }
}

/// 搜索只裁剪当前分组树，未知分组引用归入未分组，不附加“收藏/全部”伪分组筛选。
/// @author kongweiguang
fn filtered_hosts(rows: &[ManagedHost], groups: &[HostGroup], query: &str) -> Vec<ManagedHost> {
    let query = query.trim().to_lowercase();
    let group_names = groups
        .iter()
        .map(|group| (group.id.as_str(), group.name.to_lowercase()))
        .collect::<std::collections::HashMap<_, _>>();
    rows.iter()
        .filter(|host| {
            query.is_empty()
                || host.name.to_lowercase().contains(&query)
                || host.endpoint.host.to_lowercase().contains(&query)
                || host
                    .tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&query))
                || host
                    .group_id
                    .as_deref()
                    .and_then(|group_id| group_names.get(group_id))
                    .is_some_and(|name| name.contains(&query))
        })
        .cloned()
        .collect()
}

/// 分屏仅可启动真正具有 terminal capability 的 host；SFTP-only 与 RDP 不冒充 shell 目标。
///
/// @author kongweiguang
fn split_target_hosts(rows: &[ManagedHost], groups: &[HostGroup], query: &str) -> Vec<ManagedHost> {
    filtered_hosts(rows, groups, query)
        .into_iter()
        .filter(|host| {
            matches!(
                host.protocol,
                RemoteProtocol::Ssh | RemoteProtocol::Telnet | RemoteProtocol::Serial
            )
        })
        .collect()
}

/// 只在快照中存在唯一且非空默认项时返回名称；缺省或配置歧义由调用方使用通用标题。
///
/// @author kongweiguang
fn unique_default_profile_name(rows: &[VersionedProfile]) -> Option<gpui_kit::SharedString> {
    let mut defaults = rows.iter().filter(|row| row.profile.is_default);
    let profile = defaults.next()?;
    if defaults.next().is_some() {
        return None;
    }
    let name = profile.profile.name.trim();
    (!name.is_empty()).then(|| gpui_kit::SharedString::from(name.to_owned()))
}

/// 本地 Profile 与主机共用搜索词和分组树，但不伪装成远程收藏项。
/// @author kongweiguang
fn filtered_profiles(
    rows: &[VersionedProfile],
    groups: &[HostGroup],
    query: &str,
) -> Vec<VersionedProfile> {
    let query = query.trim().to_lowercase();
    let group_names = groups
        .iter()
        .map(|group| (group.id.as_str(), group.name.to_lowercase()))
        .collect::<std::collections::HashMap<_, _>>();
    rows.iter()
        .filter(|row| {
            if query.is_empty() {
                return true;
            }
            row.profile.name.to_lowercase().contains(&query)
                || row.profile.shell.to_lowercase().contains(&query)
                || row
                    .profile
                    .sidebar_group_id
                    .as_deref()
                    .and_then(|id| group_names.get(id))
                    .is_some_and(|name| name.contains(&query))
        })
        .cloned()
        .collect()
}

/// 两种配置快照均较大且体积不等，装箱让混排目录的枚举保持紧凑，避免按较大快照扩张每行空间。
/// @author kongweiguang
#[derive(Clone)]
enum GroupNavigationItem {
    RemoteHost(Box<ManagedHost>),
    LocalHost(Box<VersionedProfile>),
}

/// 主机和本地主机按相同的 order/name 排序，快照装箱以控制枚举体积，再以稳定身份消除并列。
/// @author kongweiguang
fn group_navigation_items(
    hosts: &[ManagedHost],
    profiles: &[VersionedProfile],
) -> Vec<GroupNavigationItem> {
    let mut items = hosts
        .iter()
        .cloned()
        .map(|host| GroupNavigationItem::RemoteHost(Box::new(host)))
        .chain(
            profiles
                .iter()
                .cloned()
                .map(|profile| GroupNavigationItem::LocalHost(Box::new(profile))),
        )
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let (left_order, left_name, left_kind, left_id) = match left {
            GroupNavigationItem::RemoteHost(host) => {
                (host.sort_order, host.name.as_str(), 1_u8, host.id.as_str())
            }
            GroupNavigationItem::LocalHost(profile) => (
                profile.profile.sort_order,
                profile.profile.name.as_str(),
                0_u8,
                profile.profile.id.as_str(),
            ),
        };
        let (right_order, right_name, right_kind, right_id) = match right {
            GroupNavigationItem::RemoteHost(host) => {
                (host.sort_order, host.name.as_str(), 1_u8, host.id.as_str())
            }
            GroupNavigationItem::LocalHost(profile) => (
                profile.profile.sort_order,
                profile.profile.name.as_str(),
                0_u8,
                profile.profile.id.as_str(),
            ),
        };
        left_order
            .cmp(&right_order)
            .then_with(|| left_name.cmp(right_name))
            .then_with(|| left_kind.cmp(&right_kind))
            .then_with(|| left_id.cmp(right_id))
    });
    items
}

/// 无障碍名称使用“本地主机”而不是内部配置类型，确保语音与屏幕阅读器保持产品术语一致。
/// @author kongweiguang
fn local_host_accessibility_label(name: &str) -> gpui_kit::SharedString {
    crate::locale::text(
        format!("本地主机：{name}，双击打开"),
        format!("Local host: {name}; double-click to open"),
    )
}

/// 将本地主机行操作映射为带 Profile 身份的 Root 意图，禁止把本地 ID 混入 HostId 空间。
/// @author kongweiguang
fn local_profile_action_event(
    profile_id: String,
    action: LocalProfileAction,
) -> HostNavigationEvent {
    match action {
        LocalProfileAction::Edit => HostNavigationEvent::EditLocalProfileRequested { profile_id },
        LocalProfileAction::Copy => HostNavigationEvent::CopyLocalProfileRequested { profile_id },
        LocalProfileAction::Delete => {
            HostNavigationEvent::DeleteLocalProfileRequested { profile_id }
        }
    }
}

/// 缺失分组引用仍作为未分组项显示，确保删除组不会让既有本地 Profile 消失。
/// @author kongweiguang
fn profile_group_is_ungrouped(group_id: Option<&str>, groups: &[HostGroup]) -> bool {
    group_id.is_none_or(|id| !groups.iter().any(|group| group.id == id))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostPrimaryAction {
    Connect,
    OpenSftp,
}

/// SFTP-only 主机双击打开文件工作区；其它协议沿用连接面板的原协议分派。
/// @author kongweiguang
fn primary_host_action(protocol: RemoteProtocol) -> HostPrimaryAction {
    if protocol == RemoteProtocol::Sftp {
        HostPrimaryAction::OpenSftp
    } else {
        HostPrimaryAction::Connect
    }
}

/// 只向菜单投影可执行动作；SSH 终端主动作始终保留，传输动作要求该主机当前启用了 SFTP。
///
/// @author kongweiguang
fn host_context_menu_actions(host: &ManagedHost) -> HostContextMenuActions {
    let can_sftp = super::panel::host_sftp_capable(host.protocol, host.sftp_options.enabled);
    HostContextMenuActions {
        show_primary: host.protocol != RemoteProtocol::Sftp || can_sftp,
        show_open_files: can_sftp && host.protocol != RemoteProtocol::Sftp,
        show_transfer_tab: can_sftp,
    }
}

/// Only the second click opens a connection; later clicks in a multi-click sequence never replay the action.
/// @author kongweiguang
fn is_open_gesture(click_count: usize) -> bool {
    click_count == 2
}

/// 空组在无搜索时始终可达；搜索时仅保留名称命中或仍含筛选结果的组。
/// 该纯投影规则与列表过滤分离，避免空结果提示遮住可管理的空组。
/// @author kongweiguang
fn should_render_saved_group_header(
    group_name: &str,
    query: &str,
    has_filtered_items: bool,
) -> bool {
    if has_filtered_items {
        return true;
    }
    let query = query.trim().to_lowercase();
    query.is_empty() || group_name.to_lowercase().contains(&query)
}

/// 旧版以负 sort_order 表示置顶；新导航继续识别这份已持久化约定，不增添并行配置字段。
/// @author kongweiguang
fn is_group_pinned(sort_order: i64) -> bool {
    sort_order < 0
}

/// 按旧版每 10 位的间隔分配 pin/unpin 顺序；边界溢出时拒绝改写而不产生重复排序值。
/// @author kongweiguang
fn group_sort_order_after_pin(groups: &[HostGroup], group_id: &str, pinned: bool) -> Option<i64> {
    let group = groups.iter().find(|group| group.id == group_id)?;
    if is_group_pinned(group.sort_order) == pinned {
        return Some(group.sort_order);
    }
    if pinned {
        groups
            .iter()
            .map(|group| group.sort_order)
            .fold(0_i64, i64::min)
            .checked_sub(10)
    } else {
        groups
            .iter()
            .filter(|group| !is_group_pinned(group.sort_order))
            .map(|group| group.sort_order)
            .fold(0_i64, i64::max)
            .checked_add(10)
    }
}

/// 全量折叠按钮覆盖已保存分组与当前存在的未分组区，未知组引用仍归入同一可见分区。
/// @author kongweiguang
fn navigation_group_ids(
    hosts: &[ManagedHost],
    profiles: &[VersionedProfile],
    groups: &[HostGroup],
) -> Vec<String> {
    let mut ids = groups
        .iter()
        .map(|group| group.id.clone())
        .collect::<Vec<_>>();
    let has_ungrouped_items = hosts.iter().any(|host| {
        host.group_id
            .as_deref()
            .is_none_or(|id| !groups.iter().any(|group| group.id == id))
    }) || profiles.iter().any(|profile| {
        profile_group_is_ungrouped(profile.profile.sidebar_group_id.as_deref(), groups)
    });
    if has_ungrouped_items {
        ids.push(UNGROUPED_GROUP_ID.to_owned());
    }
    ids
}

/// 无项目时不把空集合误报为已全部折叠；混合状态采取可预测的“折叠全部”。
/// @author kongweiguang
fn all_groups_are_collapsed(group_ids: &[String], collapsed: &HashSet<String>) -> bool {
    !group_ids.is_empty() && group_ids.iter().all(|id| collapsed.contains(id))
}

/// 双击说明按协议给出简短动作词，避免把 SFTP-only 目标误称成终端连接。
/// @author kongweiguang
fn host_primary_action_label(protocol: RemoteProtocol) -> gpui_kit::SharedString {
    match protocol {
        RemoteProtocol::Sftp => crate::locale::text("打开文件", "open files"),
        RemoteProtocol::Rdp => crate::locale::text("启动远程桌面", "launch remote desktop"),
        RemoteProtocol::Telnet => {
            crate::locale::text("连接 Telnet 终端", "connect the Telnet terminal")
        }
        RemoteProtocol::Serial => {
            crate::locale::text("连接串口终端", "connect the serial terminal")
        }
        RemoteProtocol::Ssh => crate::locale::text("打开 SSH 终端", "open the SSH terminal"),
    }
}

/// 移动只替换分组并使用原 revision 做 CAS；备注与协议选项必须随草稿保留，避免导航操作清空配置。
/// @author kongweiguang
fn host_draft_for_group(host: &ManagedHost, group_id: Option<String>) -> HostDraft {
    HostDraft {
        id: Some(host.id.clone()),
        group_id,
        name: host.name.clone(),
        note: host.note.clone(),
        protocol: host.protocol,
        endpoint: host.endpoint.clone(),
        authentication: host.authentication.clone(),
        authentication_mode: host.authentication_mode,
        rdp_password: host.rdp_password.clone(),
        route: host.route.clone(),
        serial: host.serial.clone(),
        tags: host.tags.clone(),
        is_favorite: host.is_favorite,
        sort_order: host.sort_order,
        connect_timeout_ms: host.connect_timeout_ms,
        keepalive_interval_ms: host.keepalive_interval_ms,
        terminal_options: host.terminal_options.clone(),
        sftp_options: host.sftp_options.clone(),
        rdp_display: host.rdp_display.clone(),
    }
}

/// 标题右键只保留分组和本地主机创建；侧栏开关统一由顶栏入口负责，避免重复导航入口。
/// @author kongweiguang
fn render_navigation_header(
    navigation: &mut HostNavigation,
    theme: ThemeTokens,
    cx: &mut Context<HostNavigation>,
) -> impl IntoElement {
    let loading = navigation.loading;
    let readonly = navigation.recovery.mode == ConfigurationMode::ReadOnlyRecovery;
    let group_ids = navigation_group_ids(
        &navigation.rows,
        &navigation.profile_rows,
        &navigation.groups,
    );
    let all_collapsed = all_groups_are_collapsed(&group_ids, &navigation.collapsed_groups);
    let entity = cx.entity().clone();
    let title = div()
        .flex_1()
        .flex()
        .items_center()
        .gap_2()
        .pl_2()
        .child(
            div()
                .text_color(theme.accent)
                .child(Icon::new(gpui_kit::assets::IconName::Server)),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(crate::locale::text("主机", "Hosts")),
        );
    let title = if readonly {
        title.into_any_element()
    } else {
        title
            .context_menu(move |menu, _, _| {
                let profile_entity = entity.clone();
                let group_entity = entity.clone();
                menu.item(
                    PopupMenuItem::new(crate::locale::text("新建分组…", "New group…"))
                        .icon(gpui_kit::assets::IconName::FolderPlus)
                        .on_click(move |_, _, app| {
                            group_entity.update(app, |_, cx| {
                                cx.emit(HostNavigationEvent::CreateGroupRequested);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new(crate::locale::text("新建本地主机…", "New local host…"))
                        .icon(gpui_kit::assets::IconName::Terminal)
                        .on_click(move |_, _, app| {
                            profile_entity.update(app, |navigation, cx| {
                                navigation.request_create_local_profile(None, cx)
                            });
                        }),
                )
            })
            .into_any_element()
    };
    let header = div()
        .id("host-navigation-header")
        .w_full()
        .h(px(36.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .child(title);
    let header = header.child(
        Button::new("host-navigation-sftp-workbench")
            .compact()
            .ghost()
            .icon(gpui_kit::assets::IconName::ArrowLeftRight)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.sidebar.opacity(0.))
                    .foreground(theme.muted)
                    .hover(theme.hover)
                    .active(theme.selected),
            )
            .accessibility_label(crate::locale::text(
                "打开 SFTP 工作台",
                "Open SFTP workbench",
            ))
            .tooltip(crate::locale::text("SFTP 工作台", "SFTP workbench"))
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                cx.emit(HostNavigationEvent::OpenSftpWorkbenchRequested)
            })),
    );
    let header = header.child(
        Button::new("host-navigation-toggle-groups")
            .compact()
            .ghost()
            .icon(if all_collapsed {
                gpui_kit::assets::IconName::ChevronsUpDown
            } else {
                gpui_kit::assets::IconName::ChevronsDownUp
            })
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.sidebar.opacity(0.))
                    .foreground(theme.muted)
                    .hover(theme.hover)
                    .active(theme.selected),
            )
            .accessibility_label(if all_collapsed {
                crate::locale::text("展开全部分组", "Expand all groups")
            } else {
                crate::locale::text("折叠全部分组", "Collapse all groups")
            })
            .tooltip(if all_collapsed {
                crate::locale::text("展开全部分组", "Expand all groups")
            } else {
                crate::locale::text("折叠全部分组", "Collapse all groups")
            })
            .disabled(group_ids.is_empty())
            .on_click(
                cx.listener(|navigation, _: &ClickEvent, _, cx| navigation.toggle_all_groups(cx)),
            ),
    );
    let header = header.child(
        Button::new("host-navigation-reload")
            .compact()
            .ghost()
            .icon(gpui_kit::assets::IconName::RefreshCw)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.sidebar.opacity(0.))
                    .foreground(theme.muted)
                    .hover(theme.hover)
                    .active(theme.selected),
            )
            .accessibility_label(crate::locale::text("刷新主机列表", "Refresh hosts"))
            .tooltip(if loading {
                crate::locale::text("正在读取主机", "Loading hosts")
            } else {
                crate::locale::text("刷新主机列表", "Refresh hosts")
            })
            .loading(loading)
            .disabled(loading)
            .on_click(cx.listener(|navigation, _: &ClickEvent, _, cx| navigation.reload(cx))),
    );
    header.into_any_element()
}

/// 无搜索时保留完整分组树（含空组）；搜索时仅保留命中组名或仍有命中条目的组，并在每组内统一排序两类主机。
/// @author kongweiguang
fn render_tree_groups(
    navigation: &mut HostNavigation,
    filtered: &[ManagedHost],
    filtered_profiles: &[VersionedProfile],
    theme: ThemeTokens,
    metrics: HostNavigationLayoutMetrics,
    cx: &mut Context<HostNavigation>,
    mut tree: gpui_kit::Stateful<gpui_kit::Div>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    for group in navigation.groups.clone() {
        let hosts = filtered
            .iter()
            .filter(|host| host.group_id.as_deref() == Some(&group.id))
            .cloned()
            .collect::<Vec<_>>();
        let profiles = filtered_profiles
            .iter()
            .filter(|row| row.profile.sidebar_group_id.as_deref() == Some(&group.id))
            .cloned()
            .collect::<Vec<_>>();
        let items = group_navigation_items(&hosts, &profiles);
        let has_filtered_items = !items.is_empty();
        if !should_render_saved_group_header(
            &group.name,
            navigation.search.as_str(),
            has_filtered_items,
        ) {
            continue;
        }
        tree = render_host_group(
            navigation,
            HostGroupRenderItems {
                group_id: group.id.clone(),
                label: crate::locale::text(group.name.clone(), group.name.clone()),
                items,
            },
            theme,
            metrics,
            cx,
            tree,
        );
    }

    let ungrouped = filtered
        .iter()
        .filter(|host| {
            host.group_id.is_none()
                || !navigation
                    .groups
                    .iter()
                    .any(|group| Some(group.id.as_str()) == host.group_id.as_deref())
        })
        .cloned()
        .collect::<Vec<_>>();
    let ungrouped_profiles = filtered_profiles
        .iter()
        .filter(|row| {
            profile_group_is_ungrouped(row.profile.sidebar_group_id.as_deref(), &navigation.groups)
        })
        .cloned()
        .collect::<Vec<_>>();
    let ungrouped_items = group_navigation_items(&ungrouped, &ungrouped_profiles);
    if !ungrouped_items.is_empty() {
        tree = render_host_group(
            navigation,
            HostGroupRenderItems {
                group_id: UNGROUPED_GROUP_ID.to_owned(),
                label: crate::locale::text("未分组", "Ungrouped"),
                items: ungrouped_items,
            },
            theme,
            metrics,
            cx,
            tree,
        );
    }
    tree
}

/// 分组行统一由单个按钮承载点击、焦点与 hover，避免 chevron、标题和计数各自形成窄命中区。
/// @author kongweiguang
fn host_group_accessibility_label(
    label: &str,
    count: usize,
    collapsed: bool,
    pinned: bool,
) -> gpui_kit::SharedString {
    let (state_zh, action_zh, state_en, action_en) = if collapsed {
        (
            "已折叠",
            "按 Enter 或空格展开",
            "collapsed",
            "Press Enter or Space to expand",
        )
    } else {
        (
            "已展开",
            "按 Enter 或空格折叠",
            "expanded",
            "Press Enter or Space to collapse",
        )
    };
    if pinned {
        crate::locale::text(
            format!("分组：{label}，已置顶，{count} 项，{state_zh}，{action_zh}"),
            format!("Group: {label}, pinned, {count} items, {state_en}. {action_en}."),
        )
    } else {
        crate::locale::text(
            format!("分组：{label}，{count} 项，{state_zh}，{action_zh}"),
            format!("Group: {label}, {count} items, {state_en}. {action_en}."),
        )
    }
}

/// 将同一分组的显示快照合成参数，避免拆散筛选和树渲染状态。
/// @author kongweiguang
struct HostGroupRenderItems {
    group_id: String,
    label: gpui_kit::SharedString,
    items: Vec<GroupNavigationItem>,
}

/// 分组标题保持紧凑并使用单一全行按钮，让 chevron、名称和计数共享圆角 hover、点击及键盘焦点。
/// 右键菜单仍捕获稳定组 ID；行内容来自一个已按 order/name 合并的序列，不先分种类渲染。
/// 点击任意行内位置只切换折叠状态，不增加行高或分拆命中区。
/// @author kongweiguang
fn render_host_group(
    navigation: &mut HostNavigation,
    items: HostGroupRenderItems,
    theme: ThemeTokens,
    metrics: HostNavigationLayoutMetrics,
    cx: &mut Context<HostNavigation>,
    mut tree: gpui_kit::Stateful<gpui_kit::Div>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let HostGroupRenderItems {
        group_id,
        label,
        items,
    } = items;
    let count = items.len();
    let collapsed = navigation.collapsed_groups.contains(&group_id);
    let saved_group = navigation.groups.iter().find(|group| group.id == group_id);
    let pinned = saved_group.is_some_and(|group| is_group_pinned(group.sort_order));
    let pin_pending = navigation.pending_group_pins.contains(&group_id);
    let group_for_toggle = group_id.clone();
    let header = Button::new(format!("host-navigation-group-{group_id}"))
        .ghost()
        .w_full()
        .h(px(metrics.group_header_height))
        .min_w(px(0.))
        .flex_shrink_0()
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(theme.sidebar.opacity(0.))
        .bg(theme.sidebar.opacity(0.))
        .text_color(theme.muted)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(theme.sidebar.opacity(0.))
                .foreground(theme.muted)
                .hover(theme.hover)
                .active(theme.selected),
        )
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(px(12.))
            .text_color(theme.muted),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label.clone()),
        )
        .when(pinned, |header| {
            header.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px_1()
                    .rounded_full()
                    .bg(theme.selected)
                    .text_color(theme.accent)
                    .text_size(theme.text_xs)
                    .child(
                        Icon::new(gpui_kit::assets::IconName::Pin)
                            .size(px(10.))
                            .text_color(theme.accent),
                    )
                    .child(crate::locale::text("置顶", "Pinned")),
            )
        })
        .child(
            div()
                .min_w(px(22.))
                .flex_shrink_0()
                .px_1()
                .rounded_full()
                .bg(theme.secondary)
                .text_right()
                .text_color(theme.muted)
                .text_size(theme.text_xs)
                .child(count.to_string()),
        )
        .accessibility_label(host_group_accessibility_label(
            &label, count, collapsed, pinned,
        ))
        .on_click(cx.listener(move |navigation, _: &ClickEvent, _, cx| {
            navigation.toggle_group(group_for_toggle.clone(), cx)
        }));
    let header = if navigation.recovery.mode != ConfigurationMode::ReadOnlyRecovery
        && saved_group.is_some()
    {
        let entity = cx.entity().clone();
        let menu_group_id = group_id.clone();
        header
            .context_menu(move |menu, _, _| {
                group_context_menu(
                    menu,
                    entity.clone(),
                    menu_group_id.clone(),
                    pinned,
                    pin_pending,
                )
            })
            .into_any_element()
    } else {
        header.into_any_element()
    };
    let mut section = div()
        .id(format!("host-navigation-section-{group_id}"))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(metrics.section_gap))
        .child(header);
    if !collapsed {
        for item in items {
            section = section.child(match item {
                GroupNavigationItem::LocalHost(profile) => {
                    let selected =
                        navigation.selected_profile.as_deref() == Some(profile.profile.id.as_str());
                    local_profile_row(
                        *profile,
                        selected,
                        navigation.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
                        theme,
                        metrics,
                        cx,
                    )
                    .into_any_element()
                }
                GroupNavigationItem::RemoteHost(host) => {
                    host_row(navigation, *host, theme, metrics, cx).into_any_element()
                }
            });
        }
    }
    tree = tree.child(section);
    tree
}

/// 导航行复用 Kit 按钮的键盘与交互样式；这里不能设置 hover/active，
/// Kit render 会在同一底层 Div 安装它们，重复安装会触发 GPUI 启动断言。
/// @author kongweiguang
fn navigation_row_button(
    id: impl Into<gpui_kit::ElementId>,
    selected: bool,
    theme: ThemeTokens,
    metrics: HostNavigationLayoutMetrics,
) -> Button {
    Button::new(id)
        .ghost()
        .w_full()
        .h(px(metrics.row_height))
        .px_2()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(if selected {
            theme.accent
        } else {
            theme.sidebar.opacity(0.)
        })
        .bg(if selected {
            theme.selected
        } else {
            theme.sidebar.opacity(0.)
        })
        .text_color(theme.foreground)
}

/// 本地主机整行可选择，双击才打开；右键只展示菜单，选择具体菜单项后才发送稳定 ID 动作。
/// @author kongweiguang
fn local_profile_row(
    row: VersionedProfile,
    selected: bool,
    readonly: bool,
    theme: ThemeTokens,
    metrics: HostNavigationLayoutMetrics,
    cx: &mut Context<HostNavigation>,
) -> impl IntoElement + 'static {
    let id = row.profile.id.clone();
    let dom_id = id.clone();
    let name = row.profile.name.clone();
    let accessible_name = name.clone();
    let shell = row.profile.shell.clone();
    let entity = cx.entity().clone();
    let click_id = id.clone();
    navigation_row_button(
        format!("host-navigation-profile-{dom_id}"),
        selected,
        theme,
        metrics,
    )
    .child(
        div()
            .size(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .bg(theme.selected)
            .text_color(theme.accent)
            .child(Icon::new(IconName::SquareTerminal).size(px(15.))),
    )
    .child(
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .items_start()
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(name),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        format!("本地主机 · {shell}"),
                        format!("Local host · {shell}"),
                    )),
            ),
    )
    .accessibility_label(local_host_accessibility_label(&accessible_name))
    .on_click(cx.listener(move |navigation, event: &ClickEvent, _, cx| {
        if is_open_gesture(event.click_count()) {
            navigation.request_local_profile(click_id.clone(), cx);
        } else if event.click_count() == 1 {
            navigation.select_local_profile(click_id.clone(), cx);
        }
    }))
    .context_menu(move |menu, _, _| {
        let open_entity = entity.clone();
        let open_id = id.clone();
        let mut menu = menu.item(
            PopupMenuItem::new(crate::locale::text("打开本地主机", "Open local host"))
                .icon(IconName::SquareTerminal)
                .on_click(move |_, _, app| {
                    open_entity.update(app, |navigation, cx| {
                        navigation.request_local_profile(open_id.clone(), cx)
                    });
                }),
        );
        if !readonly {
            let edit_entity = entity.clone();
            let edit_id = id.clone();
            menu = menu.separator().item(
                PopupMenuItem::new(crate::locale::text("编辑…", "Edit…")).on_click(
                    move |_, _, app| {
                        edit_entity.update(app, |navigation, cx| {
                            navigation.request_local_profile_action(
                                edit_id.clone(),
                                LocalProfileAction::Edit,
                                cx,
                            )
                        });
                    },
                ),
            );
            let copy_entity = entity.clone();
            let copy_id = id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "复制本地主机…",
                    "Duplicate local host…",
                ))
                .on_click(move |_, _, app| {
                    copy_entity.update(app, |navigation, cx| {
                        navigation.request_local_profile_action(
                            copy_id.clone(),
                            LocalProfileAction::Copy,
                            cx,
                        )
                    });
                }),
            );
            let delete_entity = entity.clone();
            let delete_id = id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("删除本地主机…", "Delete local host…"))
                    .on_click(move |_, _, app| {
                        delete_entity.update(app, |navigation, cx| {
                            navigation.request_local_profile_action(
                                delete_id.clone(),
                                LocalProfileAction::Delete,
                                cx,
                            )
                        });
                    }),
            );
        }
        menu
    })
}

/// 分组右键只保留分组 CRUD 动作；目录浏览由左侧导航承载，重命名和删除按稳定 group ID 路由。
/// @author kongweiguang
fn group_context_menu(
    menu: PopupMenu,
    entity: gpui_kit::Entity<HostNavigation>,
    group_id: String,
    pinned: bool,
    pin_pending: bool,
) -> PopupMenu {
    let create_entity = entity.clone();
    let create_id = group_id.clone();
    let menu = menu.item(
        PopupMenuItem::new(crate::locale::text(
            "添加连接到此分组…",
            "Add connection to this group…",
        ))
        .icon(gpui_kit::assets::IconName::FolderPlus)
        .on_click(move |_, _, app| {
            create_entity.update(app, |navigation, cx| {
                navigation.request_group_action(create_id.clone(), GroupAction::CreateHost, cx)
            });
        }),
    );
    let profile_entity = entity.clone();
    let profile_group_id = group_id.clone();
    let menu = menu.item(
        PopupMenuItem::new(crate::locale::text(
            "在此分组中新建本地主机",
            "New local host in group",
        ))
        .icon(gpui_kit::assets::IconName::Terminal)
        .on_click(move |_, _, app| {
            profile_entity.update(app, |navigation, cx| {
                navigation.request_create_local_profile(Some(profile_group_id.clone()), cx)
            });
        }),
    );
    let rename_entity = entity.clone();
    let rename_id = group_id.clone();
    let menu = menu.separator().item(
        PopupMenuItem::new(crate::locale::text("重命名分组", "Rename group"))
            .icon(gpui_kit::assets::IconName::Pencil)
            .on_click(move |_, _, app| {
                rename_entity.update(app, |navigation, cx| {
                    navigation.request_group_action(rename_id.clone(), GroupAction::Rename, cx)
                });
            }),
    );
    let pin_entity = entity.clone();
    let pin_id = group_id.clone();
    let pin_item = PopupMenuItem::new(if pinned {
        crate::locale::text("取消置顶", "Unpin group")
    } else {
        crate::locale::text("置顶分组", "Pin group")
    })
    .icon(if pinned {
        gpui_kit::assets::IconName::PinOff
    } else {
        gpui_kit::assets::IconName::Pin
    })
    .disabled(pin_pending)
    .on_click(move |_, _, app| {
        pin_entity.update(app, |navigation, cx| {
            navigation.request_group_pin(pin_id.clone(), !pinned, cx)
        });
    });
    let menu = menu.item(pin_item);
    let delete_entity = entity.clone();
    let delete_id = group_id;
    menu.item(
        PopupMenuItem::new(crate::locale::text("删除分组…", "Delete group…"))
            .icon(gpui_kit::assets::IconName::Trash)
            .on_click(move |_, _, app| {
                delete_entity.update(app, |navigation, cx| {
                    navigation.request_group_action(delete_id.clone(), GroupAction::Delete, cx)
                });
            }),
    )
    .separator()
    .item({
        let new_group_entity = entity.clone();
        PopupMenuItem::new(crate::locale::text("新建分组…", "New group…"))
            .icon(gpui_kit::assets::IconName::FolderPlus)
            .on_click(move |_, _, app| {
                new_group_entity.update(app, |_, cx| {
                    // 新分组没有所属组 ID，Root 只路由到共享分组编辑器。
                    cx.emit(HostNavigationEvent::CreateGroupRequested);
                });
            })
    })
}

/// 整行一次性构造连接行；右键只保留主机与传输动作，容器目标始终由活动终端确定。
/// 外层按钮占满可用宽度，选中与悬停色沿用主题令牌，协议动作仍绑定稳定 HostId。
/// @author kongweiguang
fn host_row(
    navigation: &mut HostNavigation,
    host: ManagedHost,
    theme: ThemeTokens,
    metrics: HostNavigationLayoutMetrics,
    cx: &mut Context<HostNavigation>,
) -> impl IntoElement + 'static {
    let host_id = host.id.clone();
    let dom_id = host_id.as_str().to_owned();
    let protocol = host.protocol;
    let menu_actions = host_context_menu_actions(&host);
    let can_copy_network_address =
        protocol != RemoteProtocol::Serial && !host.endpoint.host.trim().is_empty();
    let endpoint = host_endpoint_label(&host);
    let name = host.name.clone();
    let selected = navigation.selected_host.as_ref() == Some(&host_id);
    let pending = navigation.pending_host_moves.contains(&host_id);
    let session_status = navigation.session_projection.get(&host_id).copied();
    let readonly = navigation.recovery.mode == ConfigurationMode::ReadOnlyRecovery;
    let groups = navigation.groups.clone();
    let current_group = host.group_id.clone();
    let click_id = host_id.clone();
    let menu_entity = cx.entity().clone();
    let menu_id = host_id.clone();
    let open_label = if menu_actions.show_primary {
        host_primary_action_label(protocol)
    } else {
        crate::locale::text("SFTP 已禁用", "SFTP is disabled")
    };
    let menu_open_label = open_label.clone();
    let primary_action = primary_host_action(protocol);
    let menu_group_id = current_group.clone();

    let row = navigation_row_button(
        format!("host-navigation-host-{dom_id}"),
        selected,
        theme,
        metrics,
    )
    .min_w(px(0.))
    .custom(
        ButtonCustomVariant::new(cx)
            .color(if selected {
                theme.selected
            } else {
                theme.sidebar.opacity(0.)
            })
            .foreground(theme.foreground)
            .hover(if selected {
                theme.selected
            } else {
                theme.hover
            })
            .active(theme.selected),
    )
    .child(
        div()
            .size(px(28.))
            .flex_shrink_0()
            .rounded_md()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.selected)
            .text_color(theme.accent)
            .child(protocol_icon(protocol)),
    )
    .child(
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .items_start()
            .gap(px(1.))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child(name.clone()),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme.text_xs)
                    .text_color(theme.subtle)
                    .child(endpoint),
            ),
    )
    .when_some(session_status, |row, status| {
        row.child(session_status_dot(status, theme))
    })
    .when(pending, |row| {
        row.child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text("移动中", "Moving")),
        )
    })
    .accessibility_label(if menu_actions.show_primary {
        crate::locale::text(
            format!("{name}，单击选择，双击{open_label}"),
            format!("{name}; click to select, double-click to {open_label}"),
        )
    } else {
        crate::locale::text(
            format!("{name}，SFTP 已禁用"),
            format!("{name}; SFTP is disabled"),
        )
    })
    .when_some(session_status, |row, status| {
        row.tooltip(session_status_label(status))
    })
    .on_click(cx.listener(move |navigation, event: &ClickEvent, _, cx| {
        if is_open_gesture(event.click_count()) {
            navigation.request_connect(click_id.clone(), cx);
        } else if event.click_count() == 1 {
            navigation.select_host(click_id.clone(), cx);
        }
    }))
    .context_menu(move |menu, window, cx| {
        let mut menu = menu;
        if menu_actions.show_primary {
            let action_entity = menu_entity.clone();
            let action_id = menu_id.clone();
            let action_label = menu_open_label.clone();
            menu = menu.item(
                PopupMenuItem::new(action_label)
                    .icon(match primary_action {
                        HostPrimaryAction::Connect => gpui_kit::assets::IconName::SquareTerminal,
                        HostPrimaryAction::OpenSftp => gpui_kit::assets::IconName::FolderOpen,
                    })
                    .on_click(move |_, _, app| {
                        action_entity.update(app, |navigation, cx| match primary_action {
                            HostPrimaryAction::Connect => {
                                navigation.request_connect(action_id.clone(), cx)
                            }
                            HostPrimaryAction::OpenSftp => {
                                navigation.request_open_sftp(action_id.clone(), cx)
                            }
                        });
                    }),
            );
        }
        if menu_actions.show_open_files {
            let files_entity = menu_entity.clone();
            let files_id = menu_id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("打开文件", "Open files"))
                    .icon(gpui_kit::assets::IconName::FolderOpen)
                    .on_click(move |_, _, app| {
                        files_entity.update(app, |navigation, cx| {
                            navigation.request_open_sftp(files_id.clone(), cx)
                        });
                    }),
            );
        }
        if menu_actions.show_transfer_tab {
            menu = menu.separator();
            let transfer_entity = menu_entity.clone();
            let transfer_id = menu_id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("新建传输标签", "New transfer tab"))
                    .icon(gpui_kit::assets::IconName::ArrowLeftRight)
                    .on_click(move |_, _, app| {
                        transfer_entity.update(app, |navigation, cx| {
                            navigation.request_create_sftp_transfer_tab(transfer_id.clone(), cx)
                        });
                    }),
            );
        }
        if !readonly && let Some(group_id) = menu_group_id.clone() {
            let group_entity = menu_entity.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "添加同组连接…",
                    "Add connection to this group…",
                ))
                .icon(gpui_kit::assets::IconName::FolderPlus)
                .on_click(move |_, _, app| {
                    group_entity.update(app, |navigation, cx| {
                        navigation.request_group_action(
                            group_id.clone(),
                            GroupAction::CreateHost,
                            cx,
                        )
                    });
                }),
            );
        }
        if can_copy_network_address {
            let address_entity = menu_entity.clone();
            let address_id = menu_id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "复制主机 IP / 主机名",
                    "Copy host IP / hostname",
                ))
                .icon(gpui_kit::assets::IconName::Copy)
                .on_click(move |_, _, app| {
                    address_entity.update(app, |navigation, cx| {
                        navigation.request_copy_host_address(address_id.clone(), cx)
                    });
                }),
            );
        }

        let details_entity = menu_entity.clone();
        let details_id = menu_id.clone();
        menu = menu.separator().item(
            PopupMenuItem::new(crate::locale::text("查看连接详情", "Connection details"))
                .icon(gpui_kit::assets::IconName::Info)
                .on_click(move |_, _, app| {
                    details_entity.update(app, |navigation, cx| {
                        navigation.request_host_action(details_id.clone(), HostAction::Details, cx)
                    });
                }),
        );

        if !readonly {
            menu = menu.separator();
            let edit_entity = menu_entity.clone();
            let edit_id = menu_id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("编辑…", "Edit…"))
                    .icon(gpui_kit::assets::IconName::Pencil)
                    .on_click(move |_, _, app| {
                        edit_entity.update(app, |navigation, cx| {
                            navigation.request_host_action(edit_id.clone(), HostAction::Edit, cx)
                        });
                    }),
            );
            let copy_entity = menu_entity.clone();
            let copy_id = menu_id.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "复制主机配置…",
                    "Copy host configuration…",
                ))
                .icon(gpui_kit::assets::IconName::Copy)
                .on_click(move |_, _, app| {
                    copy_entity.update(app, |navigation, cx| {
                        navigation.request_host_action(copy_id.clone(), HostAction::Copy, cx)
                    });
                }),
            );
            if !pending {
                let move_entity = menu_entity.clone();
                let move_id = menu_id.clone();
                let destination_groups = groups.clone();
                let source_group = current_group.clone();
                menu = menu.separator().submenu(
                    crate::locale::text("移动到分组", "Move to group"),
                    window,
                    cx,
                    move |mut submenu, _, _| {
                        if source_group.is_some() {
                            let entity = move_entity.clone();
                            let id = move_id.clone();
                            submenu = submenu.item(
                                PopupMenuItem::new(crate::locale::text("未分组", "Ungrouped"))
                                    .on_click(move |_, _, app| {
                                        entity.update(app, |navigation, cx| {
                                            navigation.request_move_host(id.clone(), None, cx)
                                        });
                                    }),
                            );
                        }
                        for group in &destination_groups {
                            if source_group.as_deref() == Some(group.id.as_str()) {
                                continue;
                            }
                            let entity = move_entity.clone();
                            let id = move_id.clone();
                            let destination = group.id.clone();
                            let label = group.name.clone();
                            submenu = submenu.item(PopupMenuItem::new(label).on_click(
                                move |_, _, app| {
                                    entity.update(app, |navigation, cx| {
                                        navigation.request_move_host(
                                            id.clone(),
                                            Some(destination.clone()),
                                            cx,
                                        )
                                    });
                                },
                            ));
                        }
                        submenu
                    },
                );
            }
            let delete_entity = menu_entity.clone();
            let delete_id = menu_id.clone();
            menu = menu.separator().item(
                PopupMenuItem::new(crate::locale::text("删除连接…", "Delete connection…"))
                    .icon(gpui_kit::assets::IconName::Trash)
                    .on_click(move |_, _, app| {
                        delete_entity.update(app, |navigation, cx| {
                            navigation.request_host_action(
                                delete_id.clone(),
                                HostAction::Delete,
                                cx,
                            )
                        });
                    }),
            );
        }
        menu
    });
    row.into_any_element()
}

/// 统一连接与本地主机的空目录提示，并与搜索无匹配状态区分，避免误报为加载失败。
/// @author kongweiguang
fn empty_state(is_first_empty: bool, read_only: bool, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .text_color(theme.muted)
        .child(
            div()
                .p_2()
                .rounded_md()
                .bg(theme.secondary)
                .text_color(theme.accent)
                .child(if is_first_empty {
                    IconName::Network
                } else {
                    IconName::Search
                }),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(if is_first_empty {
                    crate::locale::text("还没有主机", "No hosts yet")
                } else {
                    crate::locale::text("没有匹配的主机", "No matching hosts")
                }),
        )
        .when(is_first_empty, |empty| {
            empty.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.subtle)
                    .child(crate::locale::text(
                        "连接信息会安全保存在此设备。",
                        "Saved connections stay on this device.",
                    )),
            )
        })
        .when(is_first_empty, |empty| {
            empty.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.subtle)
                    .child(if read_only {
                        crate::locale::text(
                            "配置处于只读恢复模式。",
                            "Configuration is read-only during recovery.",
                        )
                    } else {
                        crate::locale::text(
                            "使用底部 + 添加连接或本地主机。",
                            "Use + below to add a connection or local host.",
                        )
                    }),
            )
        })
}

/// SSH 使用 Kit 完整图标集中的服务器图标，Telnet 则保留终端语义。
fn protocol_icon(protocol: RemoteProtocol) -> Icon {
    match protocol {
        RemoteProtocol::Ssh => Icon::new(gpui_kit::assets::IconName::Server),
        RemoteProtocol::Telnet => Icon::new(IconName::SquareTerminal),
        RemoteProtocol::Sftp => Icon::new(IconName::FolderOpen),
        RemoteProtocol::Serial => Icon::new(IconName::Network),
        RemoteProtocol::Rdp => Icon::new(IconName::WindowMaximize),
    }
}

/// 用状态色显示当前可见会话阶段；没有运行态快照时不绘制状态点。
fn session_status_dot(status: HostSessionStatus, theme: ThemeTokens) -> impl IntoElement {
    let color = match status {
        HostSessionStatus::Opening => theme.warning,
        HostSessionStatus::Ready => theme.success,
        HostSessionStatus::Failed => theme.error,
    };
    div().size(px(7.)).flex_shrink_0().rounded_full().bg(color)
}

/// 给行按钮提供与纯颜色点一致的可访问说明，状态只描述 Kerminal 当前 pane。
fn session_status_label(status: HostSessionStatus) -> gpui_kit::SharedString {
    match status {
        HostSessionStatus::Opening => {
            crate::locale::text("当前工作区正在连接此主机", "A workspace session is opening")
        }
        HostSessionStatus::Ready => {
            crate::locale::text("此主机当前有就绪会话", "This host has a ready session")
        }
        HostSessionStatus::Failed => crate::locale::text(
            "此主机的会话打开失败",
            "A session for this host failed to open",
        ),
    }
}

/// 主机地址行优先显示登录名，同时在所有协议下保留明确的端口信息。
fn host_endpoint_label(host: &ManagedHost) -> gpui_kit::SharedString {
    let endpoint = &host.endpoint;
    if endpoint.username.trim().is_empty() {
        format!("{}:{}", endpoint.host, endpoint.port).into()
    } else {
        format!("{}@{}:{}", endpoint.username, endpoint.host, endpoint.port).into()
    }
}

/// 只匹配主机配置文档；其它设置变更更新恢复门禁但不触发无关目录 I/O。
fn is_navigation_configuration_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path == "hosts/groups.toml"
        || (path.starts_with("hosts/") && path.ends_with(".toml"))
        || (path.starts_with("profiles/") && path.ends_with(".toml"))
        || path.starts_with("profile-transactions/")
}

/// 错误文案只使用应用层公开信息并提供英文语义，避免在导航中泄漏路径或 source。
fn load_error_message(error: &kerminal_application::AppError) -> String {
    let message = error.public_message();
    let english = match error.public_code() {
        kerminal_application::PublicErrorCode::ConfigurationCorrupt => {
            "Host configuration needs recovery"
        }
        kerminal_application::PublicErrorCode::Cancelled => "Loading the host list was cancelled",
        _ => "Could not load the host list",
    };
    crate::locale::text(message, english).to_string()
}

impl Drop for HostNavigation {
    /// 关闭左栏时停止仅属于此面板的配置事件等待任务。
    fn drop(&mut self) {
        self.configuration_watch_cancellation.cancel();
    }
}

/// 多个 pane 指向同一主机时优先显示 Ready，其次 Opening，避免失败 pane 遮住可用会话。
fn merge_session_status(left: HostSessionStatus, right: HostSessionStatus) -> HostSessionStatus {
    match (left, right) {
        (HostSessionStatus::Ready, _) | (_, HostSessionStatus::Ready) => HostSessionStatus::Ready,
        (HostSessionStatus::Opening, _) | (_, HostSessionStatus::Opening) => {
            HostSessionStatus::Opening
        }
        _ => HostSessionStatus::Failed,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/host_navigation.rs"]
mod tests;
