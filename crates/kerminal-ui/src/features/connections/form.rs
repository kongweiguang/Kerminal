//! 主机编辑器的输入状态与边界转换；GPUI 输入只产生 application 用例草稿。
//!
//! @author kongweiguang

use std::path::PathBuf;

use gpui_kit::{
    AppContext as _, Context, Entity, ScrollAnchor, ScrollHandle, SharedString, Subscription,
    Window,
    component::{
        input::{InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
    },
};
use kerminal_application::{
    AppError,
    connections::management::{
        HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft, HostGroup,
        HostRouteSettings, HostSftpOptions, HostTerminalOptions, ManagedHost, ManagedJumpHost,
        RdpDisplayOptions,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostEndpoint, ProxyProfile, ProxyProtocol, RemoteProtocol, SshAuthentication,
    },
    sessions::{
        SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
    },
};

use super::panel::ConnectionsPanel;

/// 集中保存主机、认证、串口与代理表单输入；秘密在 host 草稿之外单独转换为 vault 用例。
pub(super) struct HostEditor {
    pub(super) original: Option<ManagedHost>,
    pub(super) protocol: RemoteProtocol,
    pub(super) section: HostEditorSection,
    pub(super) validation_issue: Option<HostEditorValidationIssue>,
    pub(super) scroll_handle: ScrollHandle,
    pub(super) validation_anchor: ScrollAnchor,
    pub(super) authentication_mode: HostAuthenticationMode,
    pub(super) selected_group: Option<String>,
    pub(super) favorite: bool,
    pub(super) proxy_enabled: bool,
    pub(super) proxy_protocol: ProxyProtocol,
    pub(super) sftp_enabled: bool,
    pub(super) preserve_mtime: bool,
    pub(super) follow_source_symlinks: bool,
    pub(super) rdp_fullscreen: bool,
    pub(super) serial_data_bits: SerialDataBits,
    pub(super) serial_stop_bits: SerialStopBits,
    pub(super) serial_parity: SerialParity,
    pub(super) serial_flow_control: SerialFlowControl,
    pub(super) inputs: HostEditorInputs,
    pub(super) jumps: Vec<JumpInputs>,
    pub(super) clear_target_credential: bool,
    pub(super) clear_private_key: bool,
    /// 用户本次明确选择文件来源；仅在路径非空时替换旧的 Vault 私钥引用。
    pub(super) private_key_file_source: bool,
    pub(super) clear_passphrase: bool,
    pub(super) clear_rdp_password: bool,
    pub(super) clear_proxy_password: bool,
    pub(super) group_select: Entity<SelectState<Vec<HostGroupOption>>>,
    pub(super) serial_data_bits_select:
        Entity<SelectState<Vec<SerialSelectOption<SerialDataBits>>>>,
    pub(super) serial_stop_bits_select:
        Entity<SelectState<Vec<SerialSelectOption<SerialStopBits>>>>,
    pub(super) serial_parity_select: Entity<SelectState<Vec<SerialSelectOption<SerialParity>>>>,
    pub(super) serial_flow_select: Entity<SelectState<Vec<SerialSelectOption<SerialFlowControl>>>>,
    baseline: HostEditorBaseline,
    protocol_drafts: Vec<(RemoteProtocol, HostProtocolDraft)>,
    _probe_subscriptions: Vec<Subscription>,
    _group_select_subscription: Subscription,
    _serial_select_subscriptions: Vec<Subscription>,
}

/// 让 Kit 搜索式下拉保存稳定分组 ID；显示名称变化不会污染主机草稿值。
#[derive(Clone)]
pub(super) struct HostGroupOption {
    id: Option<String>,
    name: SharedString,
}

impl SearchableListItem for HostGroupOption {
    type Value = Option<String>;

    /// 搜索和辅助技术读取可独立理解的分组名称，未分组项使用本地化标签。
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        self.name.clone()
    }

    /// 用可空 ID 表示未分组，不引入易冲突的哨兵字符串。
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// 固定将“未分组”放在第一项，后续选项直接映射当前已验证的配置分组。
/// @author kongweiguang
fn group_select_options(groups: &[HostGroup]) -> Vec<HostGroupOption> {
    let mut options = Vec::with_capacity(groups.len() + 1);
    options.push(HostGroupOption {
        id: None,
        name: crate::locale::text("未分组", "Ungrouped"),
    });
    options.extend(groups.iter().map(|group| HostGroupOption {
        id: Some(group.id.clone()),
        name: SharedString::from(group.name.clone()),
    }));
    options
}

/// 编辑器导航只描述当前可见分区，不进入主机草稿或脏状态基线。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostEditorSection {
    Properties,
    Authentication,
    Proxy,
    JumpHosts,
    Terminal,
    Network,
    Transfer,
    Display,
}

impl HostEditorSection {
    /// 返回侧栏实际可达的表单分区；认证归入属性，网络与传输保留独立入口避免隐藏能力。
    /// @author kongweiguang
    pub(super) fn for_protocol(protocol: RemoteProtocol) -> &'static [Self] {
        match protocol {
            RemoteProtocol::Ssh => &[
                Self::Properties,
                Self::Proxy,
                Self::JumpHosts,
                Self::Network,
                Self::Terminal,
                Self::Transfer,
            ],
            RemoteProtocol::Sftp => &[
                Self::Properties,
                Self::Proxy,
                Self::JumpHosts,
                Self::Network,
                Self::Transfer,
            ],
            RemoteProtocol::Rdp => &[Self::Properties, Self::Display],
            RemoteProtocol::Serial => &[Self::Properties, Self::Terminal],
            RemoteProtocol::Telnet => &[Self::Properties],
        }
    }

    /// 协议切换后只保留当前协议支持的分区，否则回到主机基础信息。
    /// @author kongweiguang
    pub(super) fn normalize(protocol: RemoteProtocol, section: Self) -> Self {
        if Self::for_protocol(protocol).contains(&section) {
            section
        } else {
            Self::Properties
        }
    }
}

/// 字段错误只绑定当前编辑器输入，不修改 HostDraft 的领域校验或持久化契约。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum HostEditorField {
    Name,
    Group,
    Host,
    Port,
    Username,
    Authentication,
    PrivateKey,
    Note,
    SerialBaud,
    Network,
    Terminal,
    Transfer,
    Display,
    Proxy,
    JumpHosts,
    Tags,
}

/// 应用边界仍是错误权威；UI 只把稳定公开错误就近投影到对应输入或高级分区。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostEditorValidationIssue {
    pub(super) field: HostEditorField,
    pub(super) section: HostEditorSection,
    pub(super) message: String,
}

impl HostEditorValidationIssue {
    /// 未知错误保留在表单状态栏，不猜测字段归属或隐藏更宽范围的失败。
    /// @author kongweiguang
    pub(super) fn from_error(error: &AppError, message: String) -> Option<Self> {
        let (field, section) = match error.public_message() {
            "主机名称必须为 1 到 128 个可见字符" => {
                (HostEditorField::Name, HostEditorSection::Properties)
            }
            "所选主机分组已不存在，请刷新后重新选择" => {
                (HostEditorField::Group, HostEditorSection::Properties)
            }
            "主机地址不能为空，网络协议需要非零端口" => {
                (HostEditorField::Host, HostEditorSection::Properties)
            }
            "串口 endpoint 必须由设备路径投影，且不包含网络端口或用户名"
            | "串口设备路径必须能编码为 UTF-8" => {
                (HostEditorField::Host, HostEditorSection::Terminal)
            }
            "连接端口必须为 1 到 65535" => {
                (HostEditorField::Port, HostEditorSection::Properties)
            }
            "此协议需要有效的用户名" | "RDP 用户名不能包含控制字符" => {
                (HostEditorField::Username, HostEditorSection::Properties)
            }
            "主机标签不能包含控制字符" | "串口参数必须填写在串口设置中，不能作为主机标签" => {
                (HostEditorField::Tags, HostEditorSection::Properties)
            }
            "波特率必须为 300 到 4000000"
            | "串口连接必须提供完整串口参数"
            | "串口波特率必须大于零" => {
                (HostEditorField::SerialBaud, HostEditorSection::Terminal)
            }
            "SSH 超时必须为 1 到 600 秒"
            | "SSH 心跳必须为 0 到 600 秒"
            | "SSH 超时和心跳间隔必须在 1 到 600 秒之间" => {
                (HostEditorField::Network, HostEditorSection::Network)
            }
            "SSH 终端类型或默认目录无效" => {
                (HostEditorField::Terminal, HostEditorSection::Terminal)
            }
            "SFTP 主机并发必须为 1 到 8" | "SFTP 起始目录或主机并发数无效" => {
                (HostEditorField::Transfer, HostEditorSection::Transfer)
            }
            "RDP 窗口尺寸必须成对填写且在支持范围内" => {
                (HostEditorField::Display, HostEditorSection::Display)
            }
            "代理端口必须为 1 到 65535"
            | "启用代理时必须填写代理地址"
            | "代理地址或凭据配置无效"
            | "代理密码必须绑定有效的代理用户名"
            | "代理凭据字段与连接路由不匹配" => {
                (HostEditorField::Proxy, HostEditorSection::Proxy)
            }
            "跳板端口必须为 1 到 65535" | "跳板名称、认证模式或连接地址无效" => {
                (HostEditorField::JumpHosts, HostEditorSection::JumpHosts)
            }
            "密码认证需要输入新口令或保留已有 vault 凭据。"
            | "私钥认证需要配置 key path 或已有私钥凭据。"
            | "主机认证模式与凭据字段不匹配"
            | "RDP 认证不能使用 SSH 凭据字段"
            | "此协议不接受保存的连接凭据"
            | "私钥路径不能包含控制字符" => (
                HostEditorField::Authentication,
                HostEditorSection::Properties,
            ),
            "私钥正文与私钥文件路径只能填写一项" => {
                (HostEditorField::PrivateKey, HostEditorSection::Properties)
            }
            "主机备注不能超过 4096 个字符或包含不支持的控制字符" => {
                (HostEditorField::Note, HostEditorSection::Properties)
            }
            "只有 SSH 与 SFTP 支持代理或跳板路由" => {
                (HostEditorField::Proxy, HostEditorSection::Proxy)
            }
            _ => return None,
        };
        Some(Self {
            field,
            section,
            message,
        })
    }
}

/// 表单基线只保存非秘密字段；一次性凭据输入只以“是否非空”参与脏状态判断。
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostEditorBaseline {
    common: HostEditorCommonBaseline,
    protocol: HostProtocolBaseline,
}

/// 名称、标签、分组和收藏跨协议共用，必须始终与打开编辑器时的同一基线比较。
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostEditorCommonBaseline {
    name: String,
    tags: String,
    selected_group: Option<String>,
    favorite: bool,
}

/// 隐藏协议只缓存其独有字段的脏态，共用字段恢复原值不能被历史标记阻断关闭。
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostProtocolBaseline {
    protocol: RemoteProtocol,
    authentication_mode: HostAuthenticationMode,
    private_key_file_source: bool,
    proxy_enabled: bool,
    proxy_protocol: ProxyProtocol,
    sftp_enabled: bool,
    preserve_mtime: bool,
    follow_source_symlinks: bool,
    rdp_fullscreen: bool,
    serial_data_bits: SerialDataBits,
    serial_stop_bits: SerialStopBits,
    serial_parity: SerialParity,
    serial_flow_control: SerialFlowControl,
    inputs: Vec<String>,
    note: String,
    jumps: Vec<JumpEditorBaseline>,
}

/// 仅聚合协议独有字段的脏态；共用字段由编辑器按当前值统一判断，不能缓存历史变化。
/// @author kongweiguang
fn has_unsaved_protocol_draft(
    current_protocol_dirty: bool,
    mut cached_protocol_dirty: impl Iterator<Item = bool>,
) -> bool {
    current_protocol_dirty || cached_protocol_dirty.any(|dirty| dirty)
}

/// 手动或原生文件选择只有在非空路径上且确有旧私钥引用时才标记替换来源。
/// @author kongweiguang
pub(super) fn private_key_file_source_selected(path: &str, has_saved_private_key: bool) -> bool {
    !path.trim().is_empty() && has_saved_private_key
}

/// 防止清空路径后继续清除旧凭据；文件来源意图还必须对应非空路径和已保存私钥。
/// @author kongweiguang
fn should_clear_saved_private_key_for_file_source(
    file_source_selected: bool,
    path: &str,
    has_saved_private_key: bool,
) -> bool {
    file_source_selected && !path.trim().is_empty() && has_saved_private_key
}

/// 跳板脏状态基线跳过口令正文，避免为了比较而复制凭据到长期 UI 状态。
#[derive(Clone, Debug, Eq, PartialEq)]
struct JumpEditorBaseline {
    authentication_mode: HostAuthenticationMode,
    inputs: Vec<String>,
}

/// 主目标的通用字段、一次性认证秘密和串口波特率输入，不存储已解密的 vault 内容。
#[derive(Clone)]
pub(super) struct HostEditorInputs {
    pub(super) name: Entity<InputState>,
    pub(super) host: Entity<InputState>,
    pub(super) port: Entity<InputState>,
    pub(super) username: Entity<InputState>,
    pub(super) tags: Entity<InputState>,
    pub(super) secret: Entity<InputState>,
    pub(super) key_path: Entity<InputState>,
    pub(super) private_key: Entity<TextareaState>,
    pub(super) note: Entity<TextareaState>,
    pub(super) passphrase: Entity<InputState>,
    pub(super) serial_baud: Entity<InputState>,
    pub(super) proxy_host: Entity<InputState>,
    pub(super) proxy_port: Entity<InputState>,
    pub(super) proxy_username: Entity<InputState>,
    pub(super) proxy_password: Entity<InputState>,
    pub(super) connect_timeout: Entity<InputState>,
    pub(super) keepalive: Entity<InputState>,
    pub(super) term: Entity<InputState>,
    pub(super) working_directory: Entity<InputState>,
    pub(super) sftp_local_directory: Entity<InputState>,
    pub(super) sftp_remote_directory: Entity<InputState>,
    pub(super) sftp_concurrency: Entity<InputState>,
    pub(super) rdp_width: Entity<InputState>,
    pub(super) rdp_height: Entity<InputState>,
}

/// 新建协议切换时缓存控件实体句柄，保留输入法/选中状态且不复制秘密正文到 Rust 字符串。
#[derive(Clone)]
struct HostProtocolDraft {
    inputs: HostEditorInputs,
    baseline: HostEditorBaseline,
    protocol_dirty_when_hidden: bool,
    authentication_mode: HostAuthenticationMode,
    private_key_file_source: bool,
    proxy_enabled: bool,
    proxy_protocol: ProxyProtocol,
    sftp_enabled: bool,
    preserve_mtime: bool,
    follow_source_symlinks: bool,
    rdp_fullscreen: bool,
    serial_data_bits: SerialDataBits,
    serial_stop_bits: SerialStopBits,
    serial_parity: SerialParity,
    serial_flow_control: SerialFlowControl,
    jumps: Vec<JumpInputs>,
    clear_target_credential: bool,
    clear_private_key: bool,
    clear_passphrase: bool,
    clear_rdp_password: bool,
    clear_proxy_password: bool,
}

impl HostProtocolDraft {
    /// 只缓存协议独有字段的离开时脏态，共用输入会在其它协议继续变化，不能在此冻结判断。
    /// @author kongweiguang
    fn capture(editor: &HostEditor, cx: &gpui_kit::App) -> Self {
        Self {
            inputs: editor.inputs.clone(),
            baseline: editor.baseline.clone(),
            protocol_dirty_when_hidden: editor.current_protocol_is_dirty(cx),
            authentication_mode: editor.authentication_mode,
            private_key_file_source: editor.private_key_file_source,
            proxy_enabled: editor.proxy_enabled,
            proxy_protocol: editor.proxy_protocol,
            sftp_enabled: editor.sftp_enabled,
            preserve_mtime: editor.preserve_mtime,
            follow_source_symlinks: editor.follow_source_symlinks,
            rdp_fullscreen: editor.rdp_fullscreen,
            serial_data_bits: editor.serial_data_bits,
            serial_stop_bits: editor.serial_stop_bits,
            serial_parity: editor.serial_parity,
            serial_flow_control: editor.serial_flow_control,
            jumps: editor.jumps.iter().map(JumpInputs::clone).collect(),
            clear_target_credential: editor.clear_target_credential,
            clear_private_key: editor.clear_private_key,
            clear_passphrase: editor.clear_passphrase,
            clear_rdp_password: editor.clear_rdp_password,
            clear_proxy_password: editor.clear_proxy_password,
        }
    }

    /// 恢复协议字段与其独立基线，使切回旧协议后继续按原始值判断改动。
    /// @author kongweiguang
    fn apply(self, editor: &mut HostEditor) {
        editor.inputs = self.inputs;
        editor.baseline = self.baseline;
        editor.authentication_mode = self.authentication_mode;
        editor.private_key_file_source = self.private_key_file_source;
        editor.proxy_enabled = self.proxy_enabled;
        editor.proxy_protocol = self.proxy_protocol;
        editor.sftp_enabled = self.sftp_enabled;
        editor.preserve_mtime = self.preserve_mtime;
        editor.follow_source_symlinks = self.follow_source_symlinks;
        editor.rdp_fullscreen = self.rdp_fullscreen;
        editor.serial_data_bits = self.serial_data_bits;
        editor.serial_stop_bits = self.serial_stop_bits;
        editor.serial_parity = self.serial_parity;
        editor.serial_flow_control = self.serial_flow_control;
        editor.jumps = self.jumps;
        editor.clear_target_credential = self.clear_target_credential;
        editor.clear_private_key = self.clear_private_key;
        editor.clear_passphrase = self.clear_passphrase;
        editor.clear_rdp_password = self.clear_rdp_password;
        editor.clear_proxy_password = self.clear_proxy_password;
    }
}

/// 串口下拉项以领域枚举作为稳定值，翻译标签只用于展示和搜索。
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct SerialSelectOption<T> {
    value: T,
    label: SharedString,
}

impl<T: Clone + PartialEq> SearchableListItem for SerialSelectOption<T> {
    type Value = T;

    /// 返回短标签，避免四个参数控件在窄窗口内被长枚举名挤压。
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        self.label.clone()
    }

    /// 确认后返回领域枚举，不从中文或英文显示文案反解析配置值。
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// 每个跳板保留独立身份和认证字段，顺序直接对应 schema v3 的 jump-N 凭据作用域。
#[derive(Clone)]
pub(super) struct JumpInputs {
    pub(super) name: Entity<InputState>,
    pub(super) host: Entity<InputState>,
    pub(super) port: Entity<InputState>,
    pub(super) username: Entity<InputState>,
    pub(super) secret: Entity<InputState>,
    pub(super) key_path: Entity<InputState>,
    pub(super) passphrase: Entity<InputState>,
    pub(super) authentication_mode: HostAuthenticationMode,
    pub(super) clear_credential: bool,
    pub(super) clear_passphrase: bool,
    original_authentication: Option<SshAuthentication>,
}

/// 将已保存主机映射成可编辑输入；密码只显示“已关联”状态，不从 vault 解密回填。
impl HostEditor {
    /// 创建新或已保存主机表单；空的新建表单折叠低频元信息，有保存值时展开以便核对和编辑。
    /// 高级项仍使用同一输入/脏状态链路，不维护绕过 CAS 的配置副本。
    /// @author kongweiguang
    pub(super) fn new(
        host: Option<ManagedHost>,
        groups: &[HostGroup],
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Self {
        Self::new_with_protocol(host, groups, RemoteProtocol::Ssh, window, cx)
    }

    /// 新建入口可指定默认协议，已保存主机仍以持久协议为准，避免模态调用方复制表单状态机。
    ///
    /// @author kongweiguang
    pub(super) fn new_with_protocol(
        host: Option<ManagedHost>,
        groups: &[HostGroup],
        requested_protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Self {
        let scroll_handle = ScrollHandle::new();
        let validation_anchor = ScrollAnchor::for_handle(scroll_handle.clone());
        let protocol = initial_host_editor_protocol(
            host.as_ref().map(|value| value.protocol),
            requested_protocol,
        );
        let authentication_mode = host.as_ref().map_or_else(
            || default_authentication(protocol),
            |value| value.authentication_mode,
        );
        let endpoint = host.as_ref().map(|value| &value.endpoint);
        let selected_group = host
            .as_ref()
            .and_then(|value| value.group_id.clone())
            .filter(|id| groups.iter().any(|group| &group.id == id));
        let group_select =
            cx.new(|cx| SelectState::new(group_select_options(groups), None, window, cx));
        group_select.update(cx, |select, cx| {
            select.set_selected_value(&selected_group, window, cx);
        });
        let group_select_subscription = cx.subscribe(
            &group_select,
            |panel, _, event: &SelectEvent<Vec<HostGroupOption>>, cx| {
                if let SelectEvent::Confirm(Some(group_id)) = event {
                    panel.select_editor_group(group_id.clone(), cx);
                }
            },
        );
        let serial = host.as_ref().and_then(|value| value.serial.as_ref());
        let serial_device = serial.map(|value| value.device.to_string_lossy().into_owned());
        let serial_data_bits = serial.map_or(SerialDataBits::Eight, |value| value.data_bits);
        let serial_stop_bits = serial.map_or(SerialStopBits::One, |value| value.stop_bits);
        let serial_parity = serial.map_or(SerialParity::None, |value| value.parity);
        let serial_flow_control =
            serial.map_or(SerialFlowControl::None, |value| value.flow_control);
        let serial_data_bits_select =
            cx.new(|cx| SelectState::new(serial_data_bits_options(), None, window, cx));
        serial_data_bits_select.update(cx, |select, cx| {
            select.set_selected_value(&serial_data_bits, window, cx);
        });
        let serial_stop_bits_select =
            cx.new(|cx| SelectState::new(serial_stop_bits_options(), None, window, cx));
        serial_stop_bits_select.update(cx, |select, cx| {
            select.set_selected_value(&serial_stop_bits, window, cx);
        });
        let serial_parity_select =
            cx.new(|cx| SelectState::new(serial_parity_options(), None, window, cx));
        serial_parity_select.update(cx, |select, cx| {
            select.set_selected_value(&serial_parity, window, cx);
        });
        let serial_flow_select =
            cx.new(|cx| SelectState::new(serial_flow_options(), None, window, cx));
        serial_flow_select.update(cx, |select, cx| {
            select.set_selected_value(&serial_flow_control, window, cx);
        });
        let serial_select_subscriptions = vec![
            cx.subscribe(
                &serial_data_bits_select,
                |panel, _, event: &SelectEvent<Vec<SerialSelectOption<SerialDataBits>>>, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        if let Some(editor) = panel.editor.as_mut() {
                            editor.serial_data_bits = *value;
                        }
                        panel.reset_connection_probe();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &serial_stop_bits_select,
                |panel, _, event: &SelectEvent<Vec<SerialSelectOption<SerialStopBits>>>, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        if let Some(editor) = panel.editor.as_mut() {
                            editor.serial_stop_bits = *value;
                        }
                        panel.reset_connection_probe();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &serial_parity_select,
                |panel, _, event: &SelectEvent<Vec<SerialSelectOption<SerialParity>>>, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        if let Some(editor) = panel.editor.as_mut() {
                            editor.serial_parity = *value;
                        }
                        panel.reset_connection_probe();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &serial_flow_select,
                |panel, _, event: &SelectEvent<Vec<SerialSelectOption<SerialFlowControl>>>, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        if let Some(editor) = panel.editor.as_mut() {
                            editor.serial_flow_control = *value;
                        }
                        panel.reset_connection_probe();
                        cx.notify();
                    }
                },
            ),
        ];
        let proxy = host.as_ref().and_then(|value| value.route.proxy.as_ref());
        let private_key_path = host
            .as_ref()
            .and_then(|value| value.authentication.as_ref())
            .and_then(|auth| match auth {
                SshAuthentication::PrivateKey { path, .. } => path.as_ref(),
                _ => None,
            })
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let private_key = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "粘贴私钥正文（与文件路径二选一）",
                    "Paste private key text (choose this or a key file)",
                ))
                .auto_grow(3, 8)
        });
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "主机备注（可选）",
                    "Host note (optional)",
                ))
                .auto_grow(2, 5)
        });
        note.update(cx, |state, cx| {
            state.set_value(
                host.as_ref()
                    .and_then(|value| value.note.as_deref())
                    .unwrap_or_default()
                    .to_owned(),
                window,
                cx,
            )
        });
        let inputs = HostEditorInputs {
            connect_timeout: create_input(
                window,
                cx,
                "30",
                &host
                    .as_ref()
                    .map_or(30, |value| value.connect_timeout_ms / 1000)
                    .to_string(),
                false,
            ),
            keepalive: create_input(
                window,
                cx,
                "0",
                &host
                    .as_ref()
                    .map_or(60, |value| value.keepalive_interval_ms.unwrap_or(0) / 1000)
                    .to_string(),
                false,
            ),
            term: create_input(
                window,
                cx,
                "xterm-256color",
                host.as_ref()
                    .and_then(|value| value.terminal_options.term.as_deref())
                    .unwrap_or_default(),
                false,
            ),
            working_directory: create_input(
                window,
                cx,
                "/home/user",
                host.as_ref()
                    .and_then(|value| value.terminal_options.initial_working_directory.as_deref())
                    .unwrap_or_default(),
                false,
            ),
            sftp_local_directory: create_input(
                window,
                cx,
                "",
                &host
                    .as_ref()
                    .and_then(|value| value.sftp_options.local_start_directory.as_ref())
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                false,
            ),
            sftp_remote_directory: create_input(
                window,
                cx,
                "/",
                host.as_ref()
                    .and_then(|value| value.sftp_options.remote_start_directory.as_deref())
                    .unwrap_or_default(),
                false,
            ),
            sftp_concurrency: create_input(
                window,
                cx,
                "",
                &host
                    .as_ref()
                    .and_then(|value| value.sftp_options.concurrency)
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                false,
            ),
            rdp_width: create_input(
                window,
                cx,
                "1920",
                &host
                    .as_ref()
                    .and_then(|value| value.rdp_display.width)
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                false,
            ),
            rdp_height: create_input(
                window,
                cx,
                "1080",
                &host
                    .as_ref()
                    .and_then(|value| value.rdp_display.height)
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                false,
            ),
            name: create_input(
                window,
                cx,
                crate::locale::text("主机名称", "Host name"),
                host.as_ref().map_or("", |value| &value.name),
                false,
            ),
            host: create_input(
                window,
                cx,
                host_input_placeholder(protocol),
                serial_device
                    .as_deref()
                    .or_else(|| endpoint.map(|value| value.host.as_str()))
                    .unwrap_or_default(),
                false,
            ),
            port: create_input(
                window,
                cx,
                crate::locale::text("端口", "Port"),
                &endpoint.map_or_else(|| default_port(protocol), |value| value.port.to_string()),
                false,
            ),
            username: create_input(
                window,
                cx,
                username_input_placeholder(protocol),
                endpoint.map_or("", |value| value.username.as_str()),
                false,
            ),
            tags: create_input(
                window,
                cx,
                crate::locale::text("标签，用逗号分隔", "Tags, separated by commas"),
                &host
                    .as_ref()
                    .map_or_else(String::new, |value| value.tags.join(", ")),
                false,
            ),
            secret: create_input(
                window,
                cx,
                crate::locale::text(
                    "新凭据；留空保留现有引用",
                    "New credential; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            key_path: create_input(
                window,
                cx,
                crate::locale::text(
                    "私钥文件路径（不读取文件内容）",
                    "Private key path (file contents are not read)",
                ),
                &private_key_path,
                false,
            ),
            private_key,
            note,
            passphrase: create_input(
                window,
                cx,
                crate::locale::text(
                    "私钥口令；留空保留现有引用",
                    "Key passphrase; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            serial_baud: create_input(
                window,
                cx,
                crate::locale::text("波特率 300–4000000", "Baud rate 300–4000000"),
                &serial.map_or_else(|| "9600".to_owned(), |value| value.baud_rate.to_string()),
                false,
            ),
            proxy_host: create_input(
                window,
                cx,
                crate::locale::text("代理地址", "Proxy address"),
                proxy.map_or("", |value| value.endpoint.host.as_str()),
                false,
            ),
            proxy_port: create_input(
                window,
                cx,
                crate::locale::text("代理端口", "Proxy port"),
                &proxy.map_or_else(
                    || "1080".to_owned(),
                    |value| value.endpoint.port.to_string(),
                ),
                false,
            ),
            proxy_username: create_input(
                window,
                cx,
                crate::locale::text("代理用户名（可选）", "Proxy username (optional)"),
                proxy.map_or("", |value| value.endpoint.username.as_str()),
                false,
            ),
            proxy_password: create_input(
                window,
                cx,
                crate::locale::text(
                    "新代理密码；留空保留现有引用",
                    "New proxy password; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
        };
        let jumps = host
            .as_ref()
            .map(|value| value.route.jump_hosts.iter())
            .into_iter()
            .flatten()
            .map(|jump| JumpInputs::from_model(jump, window, cx))
            .collect();
        let mut editor = Self {
            section: HostEditorSection::Properties,
            validation_issue: None,
            selected_group,
            favorite: host.as_ref().is_some_and(|value| value.is_favorite),
            proxy_enabled: proxy.is_some(),
            proxy_protocol: proxy.map_or(ProxyProtocol::Socks5, |value| value.protocol),
            sftp_enabled: host.as_ref().is_none_or(|value| value.sftp_options.enabled),
            preserve_mtime: host
                .as_ref()
                .is_none_or(|value| value.sftp_options.preserve_mtime),
            follow_source_symlinks: host
                .as_ref()
                .is_some_and(|value| value.sftp_options.follow_source_symlinks),
            rdp_fullscreen: host
                .as_ref()
                .is_none_or(|value| value.rdp_display.fullscreen),
            serial_data_bits,
            serial_stop_bits,
            serial_parity,
            serial_flow_control,
            protocol,
            authentication_mode,
            scroll_handle,
            validation_anchor,
            inputs,
            jumps,
            clear_target_credential: false,
            clear_private_key: false,
            private_key_file_source: false,
            clear_passphrase: false,
            clear_rdp_password: false,
            clear_proxy_password: false,
            group_select,
            serial_data_bits_select,
            serial_stop_bits_select,
            serial_parity_select,
            serial_flow_select,
            original: host,
            baseline: HostEditorBaseline {
                common: HostEditorCommonBaseline {
                    name: String::new(),
                    tags: String::new(),
                    selected_group: None,
                    favorite: false,
                },
                protocol: HostProtocolBaseline {
                    protocol,
                    authentication_mode,
                    private_key_file_source: false,
                    proxy_enabled: false,
                    proxy_protocol: ProxyProtocol::Socks5,
                    sftp_enabled: true,
                    preserve_mtime: true,
                    follow_source_symlinks: false,
                    rdp_fullscreen: true,
                    serial_data_bits,
                    serial_stop_bits,
                    serial_parity,
                    serial_flow_control,
                    inputs: Vec::new(),
                    note: String::new(),
                    jumps: Vec::new(),
                },
            },
            protocol_drafts: Vec::new(),
            _probe_subscriptions: Vec::new(),
            _group_select_subscription: group_select_subscription,
            _serial_select_subscriptions: serial_select_subscriptions,
        };
        editor.baseline = editor.snapshot(cx);
        editor.subscribe_probe_invalidations(window, cx);
        editor
    }

    /// 语言切换只改现存字段的占位文本，不重建输入实体或触碰未保存的用户值。
    /// @author kongweiguang
    pub(super) fn refresh_locale(&self, window: &mut Window, cx: &mut Context<ConnectionsPanel>) {
        let placeholders = [
            (
                &self.inputs.name,
                crate::locale::text("主机名称", "Host name"),
            ),
            (&self.inputs.host, host_input_placeholder(self.protocol)),
            (&self.inputs.port, crate::locale::text("端口", "Port")),
            (
                &self.inputs.username,
                username_input_placeholder(self.protocol),
            ),
            (
                &self.inputs.tags,
                crate::locale::text("标签，用逗号分隔", "Tags, separated by commas"),
            ),
            (
                &self.inputs.secret,
                crate::locale::text(
                    "新凭据；留空保留现有引用",
                    "New credential; leave empty to keep the saved reference",
                ),
            ),
            (
                &self.inputs.key_path,
                crate::locale::text(
                    "私钥文件路径（不读取文件内容）",
                    "Private key path (file contents are not read)",
                ),
            ),
            (
                &self.inputs.passphrase,
                crate::locale::text(
                    "私钥口令；留空保留现有引用",
                    "Key passphrase; leave empty to keep the saved reference",
                ),
            ),
            (
                &self.inputs.serial_baud,
                crate::locale::text("波特率 300–4000000", "Baud rate 300–4000000"),
            ),
            (
                &self.inputs.proxy_host,
                crate::locale::text("代理地址", "Proxy address"),
            ),
            (
                &self.inputs.proxy_port,
                crate::locale::text("代理端口", "Proxy port"),
            ),
            (
                &self.inputs.proxy_username,
                crate::locale::text("代理用户名（可选）", "Proxy username (optional)"),
            ),
            (
                &self.inputs.proxy_password,
                crate::locale::text(
                    "新代理密码；留空保留现有引用",
                    "New proxy password; leave empty to keep the saved reference",
                ),
            ),
        ];
        for (input, placeholder) in placeholders {
            set_input_placeholder(input, placeholder, window, cx);
        }
        self.serial_parity_select.update(cx, |select, cx| {
            select.set_items(serial_parity_options(), window, cx);
        });
        self.serial_flow_select.update(cx, |select, cx| {
            select.set_items(serial_flow_options(), window, cx);
        });
        for jump in &self.jumps {
            jump.refresh_locale(window, cx);
        }
    }

    /// 复制主机时先复用同一字段映射，再断开旧 aggregate 和全部 vault 引用，避免重复凭据所有权。
    /// @author kongweiguang
    pub(super) fn duplicate(
        host: &ManagedHost,
        groups: &[HostGroup],
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Self {
        let mut editor = Self::new(Some(host.clone()), groups, window, cx);
        editor.original = None;
        for jump in &mut editor.jumps {
            jump.original_authentication =
                authentication_without_credentials(jump.original_authentication.take());
        }
        let name = format!("{} - copy", host.name);
        editor
            .inputs
            .name
            .update(cx, |input, cx| input.set_value(name, window, cx));
        editor.baseline = editor.snapshot(cx);
        editor.subscribe_probe_invalidations(window, cx);
        editor
    }

    /// 表单变化使 probe 失效；路径输入切换到文件来源并清除粘贴值，反向粘贴则清路径。
    /// @author kongweiguang
    fn subscribe_probe_invalidations(
        &mut self,
        window: &Window,
        cx: &mut Context<ConnectionsPanel>,
    ) {
        let mut subscriptions: Vec<Subscription> = self
            .input_entities()
            .iter()
            .map(|input| {
                cx.subscribe(input, |panel, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        panel.reset_connection_probe();
                        cx.notify();
                    }
                })
            })
            .collect();
        let private_key = self.inputs.private_key.clone();
        let window_handle = window.window_handle();
        subscriptions.push(cx.subscribe(
            &private_key,
            move |panel, emitted, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let key_path = panel.editor.as_mut().and_then(|editor| {
                        if editor.inputs.private_key.entity_id() != emitted.entity_id()
                            || editor.inputs.private_key.read(cx).value().trim().is_empty()
                        {
                            return None;
                        }
                        editor.clear_private_key = false;
                        editor.private_key_file_source = false;
                        Some(editor.inputs.key_path.clone())
                    });
                    if let Some(key_path) = key_path {
                        let _ = cx.update_window(window_handle, move |_, window, app| {
                            key_path.update(app, |input, cx| {
                                input.set_value(String::new(), window, cx)
                            });
                        });
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }
            },
        ));
        let key_path = self.inputs.key_path.clone();
        let key_path_window_handle = window.window_handle();
        subscriptions.push(cx.subscribe(
            &key_path,
            move |panel, emitted, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let private_key = panel.editor.as_mut().and_then(|editor| {
                        if editor.inputs.key_path.entity_id() != emitted.entity_id() {
                            return None;
                        }
                        let path = editor.inputs.key_path.read(cx).value().to_string();
                        editor.private_key_file_source = private_key_file_source_selected(
                            &path,
                            editor.has_target_private_key(),
                        );
                        (!path.trim().is_empty()).then(|| editor.inputs.private_key.clone())
                    });
                    if let Some(private_key) = private_key {
                        let _ = cx.update_window(key_path_window_handle, move |_, window, app| {
                            private_key.update(app, |state, cx| {
                                state.set_value(String::new(), window, cx)
                            });
                        });
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }
            },
        ));
        self._probe_subscriptions = subscriptions;
    }

    /// 返回所有会影响探测目标或认证材料的输入实体，导航状态不参与探测失效判断。
    /// @author kongweiguang
    fn input_entities(&self) -> Vec<Entity<InputState>> {
        let mut inputs = vec![
            self.inputs.connect_timeout.clone(),
            self.inputs.keepalive.clone(),
            self.inputs.term.clone(),
            self.inputs.working_directory.clone(),
            self.inputs.sftp_local_directory.clone(),
            self.inputs.sftp_remote_directory.clone(),
            self.inputs.sftp_concurrency.clone(),
            self.inputs.rdp_width.clone(),
            self.inputs.rdp_height.clone(),
            self.inputs.name.clone(),
            self.inputs.host.clone(),
            self.inputs.port.clone(),
            self.inputs.username.clone(),
            self.inputs.tags.clone(),
            self.inputs.secret.clone(),
            self.inputs.key_path.clone(),
            self.inputs.passphrase.clone(),
            self.inputs.serial_baud.clone(),
            self.inputs.proxy_host.clone(),
            self.inputs.proxy_port.clone(),
            self.inputs.proxy_username.clone(),
            self.inputs.proxy_password.clone(),
        ];
        for jump in &self.jumps {
            inputs.extend([
                jump.name.clone(),
                jump.host.clone(),
                jump.port.clone(),
                jump.username.clone(),
                jump.secret.clone(),
                jump.key_path.clone(),
                jump.passphrase.clone(),
            ]);
        }
        inputs
    }

    /// 左栏“在此分组中新建”只设置草稿初值，并重置基线以免被误判为用户修改。
    /// @author kongweiguang
    pub(super) fn preselect_group(
        &mut self,
        group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) {
        self.group_select.update(cx, |select, cx| {
            select.set_selected_value(&group_id, window, cx);
        });
        self.selected_group = group_id;
        self.baseline = self.snapshot(cx);
    }

    /// 新建分组回到主机表单时只替换可选项和选中 ID，不能把正在填写的其它字段重置为干净基线。
    /// @author kongweiguang
    pub(super) fn select_group_after_inline_create(
        &mut self,
        groups: &[HostGroup],
        group_id: String,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) {
        self.group_select.update(cx, |select, cx| {
            select.set_items(group_select_options(groups), window, cx);
            select.set_selected_value(&Some(group_id.clone()), window, cx);
            select.focus(window, cx);
        });
        self.selected_group = Some(group_id);
        self.validation_issue = None;
    }

    /// 共用字段实时比较最初基线，协议独有字段聚合各自脏态；改回原值即可直接关闭。
    /// @author kongweiguang
    pub(super) fn is_dirty(&self, cx: &gpui_kit::App) -> bool {
        self.common_snapshot(cx) != self.baseline.common
            || has_unsaved_protocol_draft(
                self.current_protocol_is_dirty(cx),
                self.protocol_drafts
                    .iter()
                    .map(|(_, draft)| draft.protocol_dirty_when_hidden),
            )
    }

    /// 隐藏期间只有共用字段能继续变化，因此离开时仅冻结本协议独有字段和秘密操作的差异。
    /// @author kongweiguang
    fn current_protocol_is_dirty(&self, cx: &gpui_kit::App) -> bool {
        self.protocol_snapshot(cx) != self.baseline.protocol
            || !input_is_empty(&self.inputs.secret, cx)
            || !self.inputs.private_key.read(cx).value().is_empty()
            || !input_is_empty(&self.inputs.passphrase, cx)
            || self.clear_target_credential
            || self.clear_private_key
            || self.clear_passphrase
            || self.clear_rdp_password
            || !input_is_empty(&self.inputs.proxy_password, cx)
            || self.clear_proxy_password
            || self.jumps.iter().any(|jump| {
                !input_is_empty(&jump.secret, cx)
                    || !input_is_empty(&jump.passphrase, cx)
                    || jump.clear_credential
                    || jump.clear_passphrase
            })
    }

    /// 捕获字段基线时跳过秘密输入，串口枚举按类型比较，避免翻译标签参与脏状态。
    /// @author kongweiguang
    fn snapshot(&self, cx: &gpui_kit::App) -> HostEditorBaseline {
        HostEditorBaseline {
            common: self.common_snapshot(cx),
            protocol: self.protocol_snapshot(cx),
        }
    }

    /// 仅读取本协议独有配置，隐藏草稿的比较不受共用名称、分组或收藏变化影响。
    /// @author kongweiguang
    fn protocol_snapshot(&self, cx: &gpui_kit::App) -> HostProtocolBaseline {
        HostProtocolBaseline {
            protocol: self.protocol,
            authentication_mode: self.authentication_mode,
            private_key_file_source: self.private_key_file_source,
            proxy_enabled: self.proxy_enabled,
            proxy_protocol: self.proxy_protocol,
            sftp_enabled: self.sftp_enabled,
            preserve_mtime: self.preserve_mtime,
            follow_source_symlinks: self.follow_source_symlinks,
            rdp_fullscreen: self.rdp_fullscreen,
            serial_data_bits: self.serial_data_bits,
            serial_stop_bits: self.serial_stop_bits,
            serial_parity: self.serial_parity,
            serial_flow_control: self.serial_flow_control,
            inputs: [
                &self.inputs.connect_timeout,
                &self.inputs.keepalive,
                &self.inputs.term,
                &self.inputs.working_directory,
                &self.inputs.sftp_local_directory,
                &self.inputs.sftp_remote_directory,
                &self.inputs.sftp_concurrency,
                &self.inputs.rdp_width,
                &self.inputs.rdp_height,
                &self.inputs.host,
                &self.inputs.port,
                &self.inputs.username,
                &self.inputs.key_path,
                &self.inputs.serial_baud,
                &self.inputs.proxy_host,
                &self.inputs.proxy_port,
                &self.inputs.proxy_username,
            ]
            .into_iter()
            .map(|input| read_input(input, cx))
            .collect(),
            note: self.inputs.note.read(cx).value().to_string(),
            jumps: self
                .jumps
                .iter()
                .map(|jump| JumpEditorBaseline {
                    authentication_mode: jump.authentication_mode,
                    inputs: [
                        &jump.name,
                        &jump.host,
                        &jump.port,
                        &jump.username,
                        &jump.key_path,
                    ]
                    .into_iter()
                    .map(|input| read_input(input, cx))
                    .collect(),
                })
                .collect(),
        }
    }

    /// 共用元数据只读取当前实体，不把协议切换时的用户值误当作新的未修改初值。
    /// @author kongweiguang
    fn common_snapshot(&self, cx: &gpui_kit::App) -> HostEditorCommonBaseline {
        HostEditorCommonBaseline {
            name: read_input(&self.inputs.name, cx),
            tags: read_input(&self.inputs.tags, cx),
            selected_group: self.selected_group.clone(),
            favorite: self.favorite,
        }
    }

    /// 将编辑字段转换成 HostDraft；隐藏协议的高级项沿用原值，不让半填字段阻断其它协议保存。
    /// 串口 endpoint 只由设备路径投影，读取过程不改表单状态或把线制参数混入标签。
    /// @author kongweiguang
    pub(super) fn to_draft(
        &self,
        groups: &[HostGroup],
        cx: &mut Context<ConnectionsPanel>,
    ) -> Result<HostDraft, AppError> {
        let name = read_input(&self.inputs.name, cx).trim().to_owned();
        let host = read_input(&self.inputs.host, cx).trim().to_owned();
        let username = read_input(&self.inputs.username, cx).trim().to_owned();
        let port_text = read_input(&self.inputs.port, cx);
        let port = if self.protocol == RemoteProtocol::Serial {
            0
        } else {
            parse_port(&port_text, self.protocol)?
        };
        let pasted_private_key = self.inputs.private_key.read(cx).value().to_string();
        if matches!(self.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
            && self.authentication_mode == HostAuthenticationMode::PrivateKey
            && !pasted_private_key.trim().is_empty()
            && !read_input(&self.inputs.key_path, cx).trim().is_empty()
        {
            return Err(AppError::invalid("私钥正文与私钥文件路径只能填写一项"));
        }
        if let Some(group_id) = &self.selected_group
            && !groups.iter().any(|group| &group.id == group_id)
        {
            return Err(AppError::not_found(
                "所选主机分组已不存在，请刷新后重新选择",
            ));
        }
        let original_authentication = self
            .original
            .as_ref()
            .and_then(|value| value.authentication.clone());
        let authentication = match (self.protocol, self.authentication_mode) {
            (RemoteProtocol::Ssh | RemoteProtocol::Sftp, HostAuthenticationMode::Agent) => {
                Some(SshAuthentication::Agent)
            }
            (RemoteProtocol::Ssh | RemoteProtocol::Sftp, HostAuthenticationMode::Password) => {
                match original_authentication {
                    Some(SshAuthentication::Password { secret }) => {
                        Some(SshAuthentication::Password { secret })
                    }
                    _ => None,
                }
            }
            (RemoteProtocol::Ssh | RemoteProtocol::Sftp, HostAuthenticationMode::PrivateKey) => {
                let prior = original_authentication;
                let (secret, passphrase) = match prior {
                    Some(SshAuthentication::PrivateKey {
                        secret, passphrase, ..
                    }) => (secret, passphrase),
                    _ => (None, None),
                };
                Some(SshAuthentication::PrivateKey {
                    path: optional_path(&read_input(&self.inputs.key_path, cx))?,
                    secret,
                    passphrase,
                })
            }
            _ => None,
        };
        let rdp_password = project_rdp_password(
            self.protocol,
            self.original
                .as_ref()
                .and_then(|value| value.rdp_password.clone()),
        );
        let route = self.build_route(cx)?;
        let serial = if self.protocol == RemoteProtocol::Serial {
            let target = SerialSessionTarget {
                device: PathBuf::from(&host),
                baud_rate: parse_baud(&read_input(&self.inputs.serial_baud, cx))?,
                data_bits: self.serial_data_bits,
                stop_bits: self.serial_stop_bits,
                parity: self.serial_parity,
                flow_control: self.serial_flow_control,
            };
            Some(target)
        } else {
            None
        };
        let endpoint = if let Some(serial) = &serial {
            let device = serial
                .device
                .to_str()
                .ok_or_else(|| AppError::invalid("串口设备路径必须能编码为 UTF-8"))?;
            HostEndpoint {
                host: device.to_owned(),
                port: 0,
                username: String::new(),
            }
        } else {
            HostEndpoint {
                host,
                port,
                username,
            }
        };
        let tags = read_input(&self.inputs.tags, cx)
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .filter(|tag| !tag.eq_ignore_ascii_case("favorite"))
            .map(str::to_owned)
            .collect();
        Ok(HostDraft {
            id: self.original.as_ref().map(|value| value.id.clone()),
            group_id: self.selected_group.clone(),
            name,
            note: kerminal_application::connections::management::normalize_host_note(Some(
                self.inputs.note.read(cx).value().to_string(),
            )),
            protocol: self.protocol,
            endpoint,
            authentication,
            authentication_mode: self.authentication_mode,
            rdp_password,
            route,
            serial,
            tags,
            is_favorite: self.favorite,
            sort_order: self.original.as_ref().map_or(0, |value| value.sort_order),
            connect_timeout_ms: if matches!(
                self.protocol,
                RemoteProtocol::Ssh | RemoteProtocol::Sftp
            ) {
                u64::from(
                    parse_option_number(
                        &read_input(&self.inputs.connect_timeout, cx),
                        1,
                        600,
                        "SSH 超时必须为 1 到 600 秒",
                    )?
                    .unwrap_or(30),
                ) * 1000
            } else {
                self.original
                    .as_ref()
                    .map_or(30_000, |host| host.connect_timeout_ms)
            },
            keepalive_interval_ms: if matches!(
                self.protocol,
                RemoteProtocol::Ssh | RemoteProtocol::Sftp
            ) {
                parse_option_number(
                    &read_input(&self.inputs.keepalive, cx),
                    0,
                    600,
                    "SSH 心跳必须为 0 到 600 秒",
                )?
                .filter(|value| *value != 0)
                .map(|value| u64::from(value) * 1000)
            } else {
                self.original
                    .as_ref()
                    .map_or(Some(60_000), |host| host.keepalive_interval_ms)
            },
            terminal_options: if self.protocol == RemoteProtocol::Ssh {
                HostTerminalOptions {
                    term: optional_option_text(&read_input(&self.inputs.term, cx)),
                    initial_working_directory: optional_option_text(&read_input(
                        &self.inputs.working_directory,
                        cx,
                    )),
                }
            } else {
                self.original
                    .as_ref()
                    .map(|host| host.terminal_options.clone())
                    .unwrap_or_default()
            },
            sftp_options: if matches!(self.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
                HostSftpOptions {
                    enabled: self.sftp_enabled,
                    local_start_directory: optional_option_text(&read_input(
                        &self.inputs.sftp_local_directory,
                        cx,
                    ))
                    .map(PathBuf::from),
                    remote_start_directory: optional_option_text(&read_input(
                        &self.inputs.sftp_remote_directory,
                        cx,
                    )),
                    preserve_mtime: self.preserve_mtime,
                    follow_source_symlinks: self.follow_source_symlinks,
                    concurrency: parse_option_number(
                        &read_input(&self.inputs.sftp_concurrency, cx),
                        1,
                        8,
                        "SFTP 主机并发必须为 1 到 8",
                    )?,
                }
            } else {
                self.original
                    .as_ref()
                    .map(|host| host.sftp_options.clone())
                    .unwrap_or_default()
            },
            rdp_display: if self.protocol == RemoteProtocol::Rdp {
                RdpDisplayOptions {
                    fullscreen: self.rdp_fullscreen,
                    width: parse_option_number(
                        &read_input(&self.inputs.rdp_width, cx),
                        640,
                        8192,
                        "RDP 宽度必须为 640 到 8192",
                    )?,
                    height: parse_option_number(
                        &read_input(&self.inputs.rdp_height, cx),
                        480,
                        8192,
                        "RDP 高度必须为 480 到 8192",
                    )?,
                }
            } else {
                self.original
                    .as_ref()
                    .map(|host| host.rdp_display.clone())
                    .unwrap_or_default()
            },
        })
    }

    /// 仅将新输入或显式来源切换交给 Vault 组合用例；空路径继续保留原私钥引用。
    /// @author kongweiguang
    pub(super) fn credential_changes(
        &mut self,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Vec<HostCredentialChange> {
        let mut changes = Vec::new();
        match self.protocol {
            RemoteProtocol::Ssh | RemoteProtocol::Sftp => match self.authentication_mode {
                HostAuthenticationMode::Password => {
                    if self.clear_target_credential {
                        changes.push(HostCredentialChange::Clear {
                            slot: HostCredentialSlot::TargetPassword,
                        });
                    } else {
                        push_replace(
                            &mut changes,
                            HostCredentialSlot::TargetPassword,
                            read_input(&self.inputs.secret, cx),
                        );
                    }
                }
                HostAuthenticationMode::PrivateKey => {
                    let key = self.inputs.private_key.read(cx).value().to_string();
                    if !key.trim().is_empty() {
                        push_replace_textarea(
                            &mut changes,
                            HostCredentialSlot::TargetPrivateKey,
                            key,
                        );
                    } else if self.clear_private_key
                        || should_clear_saved_private_key_for_file_source(
                            self.private_key_file_source,
                            &read_input(&self.inputs.key_path, cx),
                            self.has_target_private_key(),
                        )
                    {
                        changes.push(HostCredentialChange::Clear {
                            slot: HostCredentialSlot::TargetPrivateKey,
                        });
                    } else {
                        // 空输入保持已保存引用不变；只有显式清除标记会生成 Clear 用例。
                    }
                    if self.clear_passphrase {
                        changes.push(HostCredentialChange::Clear {
                            slot: HostCredentialSlot::TargetKeyPassphrase,
                        });
                    } else {
                        push_replace(
                            &mut changes,
                            HostCredentialSlot::TargetKeyPassphrase,
                            read_input(&self.inputs.passphrase, cx),
                        );
                    }
                }
                HostAuthenticationMode::Agent => {}
            },
            RemoteProtocol::Rdp => {
                if self.clear_rdp_password {
                    changes.push(HostCredentialChange::Clear {
                        slot: HostCredentialSlot::RdpPassword,
                    });
                } else {
                    push_replace(
                        &mut changes,
                        HostCredentialSlot::RdpPassword,
                        read_input(&self.inputs.secret, cx),
                    );
                }
            }
            RemoteProtocol::Telnet | RemoteProtocol::Serial => {}
        }
        for (index, jump) in self.jumps.iter_mut().enumerate() {
            match jump.authentication_mode {
                HostAuthenticationMode::Agent => {}
                HostAuthenticationMode::Password => {
                    if jump.clear_credential {
                        changes.push(HostCredentialChange::Clear {
                            slot: HostCredentialSlot::JumpPassword(index),
                        });
                    } else {
                        push_replace(
                            &mut changes,
                            HostCredentialSlot::JumpPassword(index),
                            read_input(&jump.secret, cx),
                        );
                    }
                }
                HostAuthenticationMode::PrivateKey => {
                    if jump.clear_passphrase {
                        changes.push(HostCredentialChange::Clear {
                            slot: HostCredentialSlot::JumpKeyPassphrase(index),
                        });
                    } else {
                        push_replace(
                            &mut changes,
                            HostCredentialSlot::JumpKeyPassphrase(index),
                            read_input(&jump.passphrase, cx),
                        );
                    }
                }
            }
        }
        if matches!(self.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) && self.proxy_enabled
        {
            if self.clear_proxy_password {
                append_proxy_credential_change(&mut changes, true, String::new());
            } else {
                append_proxy_credential_change(
                    &mut changes,
                    false,
                    read_input(&self.inputs.proxy_password, cx),
                );
            }
        }
        changes
    }

    /// 代理和跳板只在 SSH/SFTP 有效；明文不进入草稿，空输入保留 vault 引用。
    /// 该映射只读取表单并构造路由值，保持 `&self` 以避免无关放宽调用方的可变借用。
    /// @author kongweiguang
    fn build_route(
        &self,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Result<HostRouteSettings, AppError> {
        let proxy = if self.proxy_enabled {
            let host = read_input(&self.inputs.proxy_host, cx).trim().to_owned();
            if host.is_empty() {
                return Err(AppError::invalid("启用代理时必须填写代理地址"));
            }
            let port = read_input(&self.inputs.proxy_port, cx)
                .parse::<u16>()
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(|| AppError::invalid("代理端口必须为 1 到 65535"))?;
            let username = read_input(&self.inputs.proxy_username, cx)
                .trim()
                .to_owned();
            let original_proxy = self
                .original
                .as_ref()
                .and_then(|value| value.route.proxy.as_ref());
            let password = proxy_password_matches_route(
                original_proxy,
                self.proxy_protocol,
                &host,
                Some(port),
                &username,
            )
            .then(|| original_proxy.and_then(|proxy| proxy.password.clone()))
            .flatten();
            Some(ProxyProfile {
                protocol: self.proxy_protocol,
                endpoint: HostEndpoint {
                    host,
                    port,
                    username,
                },
                password,
            })
        } else {
            None
        };
        let mut jump_hosts = Vec::with_capacity(self.jumps.len());
        for jump in &self.jumps {
            jump_hosts.push(jump.to_model(cx)?);
        }
        Ok(HostRouteSettings { proxy, jump_hosts })
    }

    /// 将已输入密钥和口令字段清空，缩短秘密在界面组件状态中的驻留时间。
    /// @author kongweiguang
    pub(super) fn clear_secrets(&self, window: &mut Window, cx: &mut Context<ConnectionsPanel>) {
        clear_input(&self.inputs.secret, window, cx);
        clear_textarea(&self.inputs.private_key, window, cx);
        clear_input(&self.inputs.passphrase, window, cx);
        clear_input(&self.inputs.proxy_password, window, cx);
        for jump in &self.jumps {
            clear_input(&jump.secret, window, cx);
            clear_input(&jump.passphrase, window, cx);
        }
        for (_, draft) in &self.protocol_drafts {
            clear_input(&draft.inputs.secret, window, cx);
            clear_textarea(&draft.inputs.private_key, window, cx);
            clear_input(&draft.inputs.passphrase, window, cx);
            clear_input(&draft.inputs.proxy_password, window, cx);
            for jump in &draft.jumps {
                clear_input(&jump.secret, window, cx);
                clear_input(&jump.passphrase, window, cx);
            }
        }
    }

    /// 新协议仅为独有默认值建基线，共用字段沿用打开时基线，切换不能使用户编辑变成初值。
    /// 编辑态锁定持久协议，避免误改 schema 语义。
    /// @author kongweiguang
    pub(super) fn switch_new_protocol(
        &mut self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> bool {
        if self.original.is_some() || self.protocol == protocol {
            return false;
        }
        self.protocol_drafts
            .retain(|(saved_protocol, _)| *saved_protocol != self.protocol);
        self.protocol_drafts
            .push((self.protocol, HostProtocolDraft::capture(self, cx)));
        let restored = self
            .protocol_drafts
            .iter()
            .position(|(saved_protocol, _)| *saved_protocol == protocol)
            .map(|index| self.protocol_drafts.remove(index).1);
        let restored_existing_draft = if let Some(draft) = restored {
            draft.apply(self);
            true
        } else {
            self.inputs = self.new_protocol_inputs(protocol, window, cx);
            self.authentication_mode = default_authentication(protocol);
            self.proxy_enabled = false;
            self.proxy_protocol = ProxyProtocol::Socks5;
            self.sftp_enabled = true;
            self.preserve_mtime = true;
            self.follow_source_symlinks = false;
            self.rdp_fullscreen = true;
            self.serial_data_bits = SerialDataBits::Eight;
            self.serial_stop_bits = SerialStopBits::One;
            self.serial_parity = SerialParity::None;
            self.serial_flow_control = SerialFlowControl::None;
            self.jumps.clear();
            self.clear_target_credential = false;
            self.clear_private_key = false;
            self.private_key_file_source = false;
            self.clear_passphrase = false;
            self.clear_rdp_password = false;
            self.clear_proxy_password = false;
            false
        };
        self.protocol = protocol;
        if !restored_existing_draft {
            let common_baseline = self.baseline.common.clone();
            self.baseline = self.snapshot(cx);
            self.baseline.common = common_baseline;
        }
        self.section = HostEditorSection::normalize(protocol, self.section);
        self.validation_issue = None;
        self.scroll_handle.set_offset(Default::default());
        self.subscribe_probe_invalidations(window, cx);
        true
    }

    /// 为首次进入的协议创建空字段，复用名称/标签句柄，其余字段按协议默认值独立持有。
    /// @author kongweiguang
    fn new_protocol_inputs(
        &self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> HostEditorInputs {
        let name = self.inputs.name.clone();
        let tags = self.inputs.tags.clone();
        let private_key = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "粘贴私钥正文（与文件路径二选一）",
                    "Paste private key text (choose this or a key file)",
                ))
                .auto_grow(3, 8)
        });
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "主机备注（可选）",
                    "Host note (optional)",
                ))
                .auto_grow(2, 5)
        });
        HostEditorInputs {
            name,
            host: create_input(window, cx, host_input_placeholder(protocol), "", false),
            port: create_input(
                window,
                cx,
                crate::locale::text("端口", "Port"),
                &default_port(protocol).to_string(),
                false,
            ),
            username: create_input(window, cx, username_input_placeholder(protocol), "", false),
            tags,
            secret: create_input(
                window,
                cx,
                crate::locale::text(
                    "新凭据；留空保留现有引用",
                    "New credential; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            key_path: create_input(
                window,
                cx,
                crate::locale::text(
                    "私钥文件路径（不读取文件内容）",
                    "Private key path (file contents are not read)",
                ),
                "",
                false,
            ),
            private_key,
            note,
            passphrase: create_input(
                window,
                cx,
                crate::locale::text(
                    "私钥口令；留空保留现有引用",
                    "Key passphrase; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            serial_baud: create_input(
                window,
                cx,
                crate::locale::text("波特率 300–4000000", "Baud rate 300–4000000"),
                "9600",
                false,
            ),
            proxy_host: create_input(
                window,
                cx,
                crate::locale::text("代理地址", "Proxy address"),
                "",
                false,
            ),
            proxy_port: create_input(
                window,
                cx,
                crate::locale::text("代理端口", "Proxy port"),
                "1080",
                false,
            ),
            proxy_username: create_input(
                window,
                cx,
                crate::locale::text("代理用户名（可选）", "Proxy username (optional)"),
                "",
                false,
            ),
            proxy_password: create_input(
                window,
                cx,
                crate::locale::text(
                    "新代理密码；留空保留现有引用",
                    "New proxy password; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            connect_timeout: create_input(window, cx, "30", "30", false),
            keepalive: create_input(window, cx, "0", "0", false),
            term: create_input(window, cx, "xterm-256color", "xterm-256color", false),
            working_directory: create_input(window, cx, "/home/user", "", false),
            sftp_local_directory: create_input(window, cx, "", "", false),
            sftp_remote_directory: create_input(window, cx, "/", "/", false),
            sftp_concurrency: create_input(window, cx, "", "", false),
            rdp_width: create_input(window, cx, "1920", "", false),
            rdp_height: create_input(window, cx, "1080", "", false),
        }
    }

    /// 只报告是否已有 opaque 密文关联，界面据此显示状态但不检查或读取秘密正文。
    pub(super) fn has_target_credential(&self) -> bool {
        let Some(host) = self.original.as_ref() else {
            return false;
        };
        match (self.protocol, self.authentication_mode) {
            (RemoteProtocol::Ssh | RemoteProtocol::Sftp, HostAuthenticationMode::Password) => {
                matches!(
                    host.authentication.as_ref(),
                    Some(SshAuthentication::Password { .. })
                )
            }
            (RemoteProtocol::Ssh | RemoteProtocol::Sftp, HostAuthenticationMode::PrivateKey) => {
                self.has_target_private_key()
            }
            (RemoteProtocol::Rdp, _) => host.rdp_password.is_some(),
            _ => false,
        }
    }

    /// 只报告已保存私钥正文的 opaque vault 引用，绝不读取或回显解密内容。
    /// @author kongweiguang
    pub(super) fn has_target_private_key(&self) -> bool {
        self.original.as_ref().is_some_and(|host| {
            matches!(
                host.authentication.as_ref(),
                Some(SshAuthentication::PrivateKey {
                    secret: Some(_),
                    ..
                })
            )
        })
    }

    /// 查询目标私钥口令是否存在，只回传状态以阻止 UI 回显 vault 内容。
    pub(super) fn has_target_passphrase(&self) -> bool {
        self.original.as_ref().is_some_and(|host| {
            matches!(
                host.authentication.as_ref(),
                Some(SshAuthentication::PrivateKey {
                    passphrase: Some(_),
                    ..
                })
            )
        })
    }

    /// 只报告代理密码是否关联 opaque vault ref，不读取解密后的秘密正文。
    pub(super) fn has_proxy_password(&self, cx: &mut Context<ConnectionsPanel>) -> bool {
        let original_proxy = self
            .original
            .as_ref()
            .and_then(|host| host.route.proxy.as_ref())
            .filter(|proxy| proxy.password.is_some());
        proxy_password_matches_route(
            original_proxy,
            self.proxy_protocol,
            &read_input(&self.inputs.proxy_host, cx),
            read_input(&self.inputs.proxy_port, cx).parse::<u16>().ok(),
            &read_input(&self.inputs.proxy_username, cx),
        )
    }
}

/// 保存主机的协议不可被入口覆盖；只有新建草稿采用入口请求的默认协议。
///
/// @author kongweiguang
pub(super) fn initial_host_editor_protocol(
    saved_protocol: Option<RemoteProtocol>,
    requested_protocol: RemoteProtocol,
) -> RemoteProtocol {
    saved_protocol.unwrap_or(requested_protocol)
}

/// 复制认证模式与公开 key path，但剥离所有 vault 引用，要求用户为副本重新录入秘密。
/// 缺少凭据时只阻止密码认证进入草稿用例，不覆盖已经保存的 Vault 引用。
/// @author kongweiguang
fn authentication_without_credentials(
    authentication: Option<SshAuthentication>,
) -> Option<SshAuthentication> {
    match authentication {
        Some(SshAuthentication::Password { .. }) => None,
        Some(SshAuthentication::PrivateKey { path, .. }) => Some(SshAuthentication::PrivateKey {
            path,
            secret: None,
            passphrase: None,
        }),
        Some(SshAuthentication::Agent) => Some(SshAuthentication::Agent),
        None => None,
    }
}

/// 只有代理协议、地址、端口和用户名全匹配时才沿用旧 ref，避免编辑路由后误复用密码。
fn proxy_password_matches_route(
    original: Option<&ProxyProfile>,
    protocol: ProxyProtocol,
    host: &str,
    port: Option<u16>,
    username: &str,
) -> bool {
    original.is_some_and(|proxy| {
        proxy.password.is_some()
            && proxy.protocol == protocol
            && proxy.endpoint.host == host.trim()
            && Some(proxy.endpoint.port) == port
            && proxy.endpoint.username == username.trim()
    })
}

/// 将代理密码表单状态投影成 Replace/Clear；空输入本身不产生改动，保留旧 opaque ref。
fn append_proxy_credential_change(
    changes: &mut Vec<HostCredentialChange>,
    clearing: bool,
    secret: String,
) {
    if clearing {
        changes.push(HostCredentialChange::Clear {
            slot: HostCredentialSlot::ProxyPassword,
        });
    } else {
        push_replace(changes, HostCredentialSlot::ProxyPassword, secret);
    }
}

/// 仅 RDP 草稿继承原 RDP 凭据；协议转换要从新主机引用集中移除不适用的旧引用。
/// @author kongweiguang
fn project_rdp_password(
    protocol: RemoteProtocol,
    existing: Option<CredentialRef>,
) -> Option<CredentialRef> {
    if protocol == RemoteProtocol::Rdp {
        existing
    } else {
        None
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_form.rs"]
mod tests;

impl JumpInputs {
    /// 刷新已存在跳板输入的占位文本，不改变用户正在编辑的地址或秘密。
    /// @author kongweiguang
    fn refresh_locale(&self, window: &mut Window, cx: &mut Context<ConnectionsPanel>) {
        let placeholders = [
            (
                &self.name,
                crate::locale::text("跳板名称", "Jump host name"),
            ),
            (
                &self.host,
                crate::locale::text("跳板地址", "Jump host address"),
            ),
            (&self.port, crate::locale::text("SSH 端口", "SSH port")),
            (
                &self.username,
                crate::locale::text("SSH 用户名", "SSH username"),
            ),
            (
                &self.secret,
                crate::locale::text(
                    "跳板密码；留空保留",
                    "Jump host password; leave empty to keep",
                ),
            ),
            (
                &self.key_path,
                crate::locale::text("跳板私钥路径", "Jump host private key path"),
            ),
            (
                &self.passphrase,
                crate::locale::text(
                    "跳板私钥口令；留空保留",
                    "Jump host key passphrase; leave empty to keep",
                ),
            ),
        ];
        for (input, placeholder) in placeholders {
            set_input_placeholder(input, placeholder, window, cx);
        }
    }

    /// 由受信任主机快照构造跳板输入，并只回显公开 key path 与认证模式。
    fn from_model(
        jump: &ManagedJumpHost,
        window: &mut Window,
        cx: &mut Context<ConnectionsPanel>,
    ) -> Self {
        let path = jump
            .authentication
            .as_ref()
            .and_then(|auth| match auth {
                SshAuthentication::PrivateKey { path, .. } => path.as_ref(),
                _ => None,
            })
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            name: create_input(
                window,
                cx,
                crate::locale::text("跳板名称", "Jump host name"),
                &jump.name,
                false,
            ),
            host: create_input(
                window,
                cx,
                crate::locale::text("跳板地址", "Jump host address"),
                &jump.endpoint.host,
                false,
            ),
            port: create_input(
                window,
                cx,
                crate::locale::text("SSH 端口", "SSH port"),
                &jump.endpoint.port.to_string(),
                false,
            ),
            username: create_input(
                window,
                cx,
                crate::locale::text("SSH 用户名", "SSH username"),
                &jump.endpoint.username,
                false,
            ),
            secret: create_input(
                window,
                cx,
                crate::locale::text(
                    "跳板密码；留空保留",
                    "Jump host password; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            key_path: create_input(
                window,
                cx,
                crate::locale::text("跳板私钥路径", "Jump host private key path"),
                &path,
                false,
            ),
            passphrase: create_input(
                window,
                cx,
                crate::locale::text(
                    "跳板私钥口令；留空保留",
                    "Jump host key passphrase; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            authentication_mode: jump.authentication_mode,
            clear_credential: false,
            clear_passphrase: false,
            original_authentication: jump.authentication.clone(),
        }
    }

    /// 添加一条空白 Agent 跳板，让用户先明确填写网络目标再选择需要的认证方式。
    pub(super) fn empty(window: &mut Window, cx: &mut Context<ConnectionsPanel>) -> Self {
        Self {
            name: create_input(
                window,
                cx,
                crate::locale::text("跳板名称", "Jump host name"),
                "Jump",
                false,
            ),
            host: create_input(
                window,
                cx,
                crate::locale::text("跳板地址", "Jump host address"),
                "",
                false,
            ),
            port: create_input(
                window,
                cx,
                crate::locale::text("SSH 端口", "SSH port"),
                "22",
                false,
            ),
            username: create_input(
                window,
                cx,
                crate::locale::text("SSH 用户名", "SSH username"),
                "",
                false,
            ),
            secret: create_input(
                window,
                cx,
                crate::locale::text(
                    "跳板密码；留空保留",
                    "Jump host password; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            key_path: create_input(
                window,
                cx,
                crate::locale::text("跳板私钥路径", "Jump host private key path"),
                "",
                false,
            ),
            passphrase: create_input(
                window,
                cx,
                crate::locale::text(
                    "跳板私钥口令；留空保留",
                    "Jump host key passphrase; leave empty to keep the saved reference",
                ),
                "",
                true,
            ),
            authentication_mode: HostAuthenticationMode::Agent,
            clear_credential: false,
            clear_passphrase: false,
            original_authentication: Some(SshAuthentication::Agent),
        }
    }

    /// 将跳板表单映射为应用层类型并沿用原 opaque credential refs。
    fn to_model(&self, cx: &mut Context<ConnectionsPanel>) -> Result<ManagedJumpHost, AppError> {
        let name = read_input(&self.name, cx).trim().to_owned();
        let host = read_input(&self.host, cx).trim().to_owned();
        let port = read_input(&self.port, cx)
            .parse::<u16>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| AppError::invalid("跳板端口必须为 1 到 65535"))?;
        let username = read_input(&self.username, cx).trim().to_owned();
        let authentication = match (
            self.authentication_mode,
            self.original_authentication.clone(),
        ) {
            (HostAuthenticationMode::Agent, _) => Some(SshAuthentication::Agent),
            (HostAuthenticationMode::Password, Some(SshAuthentication::Password { secret })) => {
                Some(SshAuthentication::Password { secret })
            }
            (HostAuthenticationMode::Password, _) => None,
            (HostAuthenticationMode::PrivateKey, prior) => {
                let (secret, passphrase) = match prior {
                    Some(SshAuthentication::PrivateKey {
                        secret, passphrase, ..
                    }) => (secret, passphrase),
                    _ => (None, None),
                };
                Some(SshAuthentication::PrivateKey {
                    path: optional_path(&read_input(&self.key_path, cx))?,
                    secret,
                    passphrase,
                })
            }
        };
        Ok(ManagedJumpHost {
            name,
            endpoint: HostEndpoint {
                host,
                port,
                username,
            },
            authentication,
            authentication_mode: self.authentication_mode,
        })
    }

    /// 清空跳板表单中仍持有的秘密文本，避免删除路由后暂存引用继续保留在 UI。
    pub(super) fn clear_secrets(&self, window: &mut Window, cx: &mut Context<ConnectionsPanel>) {
        clear_input(&self.secret, window, cx);
        clear_input(&self.passphrase, window, cx);
    }

    /// 只报告跳板凭据是否通过 opaque ref 保存，不读取解密后的秘密。
    pub(super) fn has_password(&self) -> bool {
        matches!(
            self.original_authentication.as_ref(),
            Some(SshAuthentication::Password { .. })
        )
    }

    /// 返回原认证快照供表单显示是否存在 vault ref，不读取也不复制秘密正文。
    pub(super) fn original_authentication_ref(&self) -> Option<&SshAuthentication> {
        self.original_authentication.as_ref()
    }
}

/// 刷新已有输入的占位文字；只改显示提示，不重建或清空输入状态。
/// @author kongweiguang
pub(super) fn set_input_placeholder(
    input: &Entity<InputState>,
    placeholder: SharedString,
    window: &mut Window,
    cx: &mut Context<ConnectionsPanel>,
) {
    input.update(cx, |state, cx| {
        state.set_placeholder(placeholder, window, cx)
    });
}

/// 可选数字只接受十进制整数；空值保留继承语义，错误不静默替换用户输入。
///
/// @author kongweiguang
fn parse_option_number(
    value: &str,
    minimum: u16,
    maximum: u16,
    message: &'static str,
) -> Result<Option<u16>, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|value| (minimum..=maximum).contains(value))
        .map(Some)
        .ok_or_else(|| AppError::invalid(message))
}

/// 空白高级字段表示继承；非空值只去掉表单边界空格，路径合法性仍由用例统一判断。
///
/// @author kongweiguang
fn optional_option_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// 创建带占位文本的 GPUI Kit 输入，秘密字段在 native editor state 中立即进入掩码呈现。
pub(super) fn create_input(
    window: &mut Window,
    cx: &mut Context<ConnectionsPanel>,
    placeholder: impl Into<SharedString>,
    value: &str,
    masked: bool,
) -> Entity<InputState> {
    let placeholder = placeholder.into();
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.clone())
            .default_value(value)
    });
    if masked {
        input.update(cx, |state, cx| state.set_masked(true, window, cx));
    }
    input
}

/// 读取一个输入值时只创建一次短生命周期副本，调用方不得将其写入日志或 host draft。
pub(super) fn read_input(input: &Entity<InputState>, cx: &gpui_kit::App) -> String {
    input.read(cx).value().to_string()
}

/// 只读取秘密输入是否为空，避免脏状态检查复制口令或私钥正文。
fn input_is_empty(input: &Entity<InputState>, cx: &gpui_kit::App) -> bool {
    input.read(cx).value().is_empty()
}

/// 保存完成或表单取消后清空输入组件内仍持有的秘密文本。
/// @author kongweiguang
pub(super) fn clear_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<ConnectionsPanel>,
) {
    input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
}

/// 清理多行秘密控件，避免关闭或保存后私钥正文继续留在输入实体中。
/// @author kongweiguang
pub(super) fn clear_textarea(
    input: &Entity<TextareaState>,
    window: &mut Window,
    cx: &mut Context<ConnectionsPanel>,
) {
    input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
}

/// 忽略空白占位，只有新输入的秘密才会触发 unique-scope vault 写入。
/// @author kongweiguang
fn push_replace(changes: &mut Vec<HostCredentialChange>, slot: HostCredentialSlot, secret: String) {
    if !secret.is_empty() {
        changes.push(HostCredentialChange::Replace {
            slot,
            secret: kerminal_application::connections::credentials::SecretValue::new(secret),
        });
    }
}

/// 将私钥正文包装成 vault 输入，空输入表示保留原引用而不生成空凭据写入。
/// @author kongweiguang
fn push_replace_textarea(
    changes: &mut Vec<HostCredentialChange>,
    slot: HostCredentialSlot,
    secret: String,
) {
    if !secret.is_empty() {
        changes.push(HostCredentialChange::Replace {
            slot,
            secret: kerminal_application::connections::credentials::SecretValue::new(secret),
        });
    }
}

/// 端口字段留空时使用各网络协议冻结默认值；Serial 不使用网络端口。
fn parse_port(value: &str, protocol: RemoteProtocol) -> Result<u16, AppError> {
    if protocol == RemoteProtocol::Serial {
        return Ok(0);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| AppError::invalid("连接端口必须为 1 到 65535"))
}

/// 只保存公开的 key path，不通过 UI 读取或记录私钥正文。
fn optional_path(value: &str) -> Result<Option<PathBuf>, AppError> {
    let value = value.trim();
    if value.chars().any(char::is_control) {
        return Err(AppError::invalid("私钥路径不能包含控制字符"));
    }
    Ok((!value.is_empty()).then(|| PathBuf::from(value)))
}

/// 约束当前串口表单的波特率范围，避免 adapter 收到平台驱动拒绝的数值。
fn parse_baud(value: &str) -> Result<u32, AppError> {
    let baud = value
        .parse::<u32>()
        .ok()
        .filter(|baud| (300..=4_000_000).contains(baud))
        .ok_or_else(|| AppError::invalid("波特率必须为 300 到 4000000"))?;
    Ok(baud)
}

/// 返回协议默认端口，旧文件已显式保存的值仍优先回显。
/// @author kongweiguang
fn default_port(protocol: RemoteProtocol) -> String {
    match protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => "22",
        RemoteProtocol::Telnet => "23",
        RemoteProtocol::Rdp => "3389",
        RemoteProtocol::Serial => "0",
    }
    .to_owned()
}

/// 按协议提示用户名必填性，避免 SSH/SFTP 字段错误暗示可以留空。
/// @author kongweiguang
pub(super) fn username_input_placeholder(protocol: RemoteProtocol) -> SharedString {
    match protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => {
            crate::locale::text("SSH 用户名", "SSH username")
        }
        RemoteProtocol::Rdp | RemoteProtocol::Telnet => {
            crate::locale::text("用户名（可选）", "Username (optional)")
        }
        RemoteProtocol::Serial => crate::locale::text("用户名", "Username"),
    }
}

/// 按协议提示主机地址或串口设备路径；协议切换只变更占位提示，不改用户输入。
/// @author kongweiguang
pub(super) fn host_input_placeholder(protocol: RemoteProtocol) -> SharedString {
    match protocol {
        RemoteProtocol::Serial => crate::locale::text("设备路径", "Device path"),
        RemoteProtocol::Ssh
        | RemoteProtocol::Sftp
        | RemoteProtocol::Telnet
        | RemoteProtocol::Rdp => crate::locale::text("主机地址", "Host address"),
    }
}

/// 串口数据位选项使用短数字标签，减少下拉菜单和紧凑表单的横向占用。
/// @author kongweiguang
fn serial_data_bits_options() -> Vec<SerialSelectOption<SerialDataBits>> {
    [
        (SerialDataBits::Five, "5"),
        (SerialDataBits::Six, "6"),
        (SerialDataBits::Seven, "7"),
        (SerialDataBits::Eight, "8"),
    ]
    .into_iter()
    .map(|(value, label)| SerialSelectOption {
        value,
        label: SharedString::from(label),
    })
    .collect()
}

/// 串口停止位只允许冻结配置中的整数选项，避免 UI 输入平台不支持的浮点值。
/// @author kongweiguang
fn serial_stop_bits_options() -> Vec<SerialSelectOption<SerialStopBits>> {
    [(SerialStopBits::One, "1"), (SerialStopBits::Two, "2")]
        .into_iter()
        .map(|(value, label)| SerialSelectOption {
            value,
            label: SharedString::from(label),
        })
        .collect()
}

/// 校验下拉显示短本地化名称，但确认后传递稳定领域枚举。
/// @author kongweiguang
fn serial_parity_options() -> Vec<SerialSelectOption<SerialParity>> {
    [
        (SerialParity::None, crate::locale::text("无校验", "None")),
        (SerialParity::Even, crate::locale::text("偶校验", "Even")),
        (SerialParity::Odd, crate::locale::text("奇校验", "Odd")),
    ]
    .into_iter()
    .map(|(value, label)| SerialSelectOption { value, label })
    .collect()
}

/// 流控下拉保留 none/software/hardware 领域值，只本地化用户可读短标签。
/// @author kongweiguang
fn serial_flow_options() -> Vec<SerialSelectOption<SerialFlowControl>> {
    [
        (
            SerialFlowControl::None,
            crate::locale::text("无流控", "None"),
        ),
        (
            SerialFlowControl::Software,
            crate::locale::text("软件流控", "Software flow control"),
        ),
        (
            SerialFlowControl::Hardware,
            crate::locale::text("硬件流控", "Hardware flow control"),
        ),
    ]
    .into_iter()
    .map(|(value, label)| SerialSelectOption { value, label })
    .collect()
}

/// 新建 SSH/SFTP 优先密码认证，编辑时由已保存模式覆盖；其它协议保留其既有模式。
/// @author kongweiguang
pub(super) fn default_authentication(protocol: RemoteProtocol) -> HostAuthenticationMode {
    match protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp | RemoteProtocol::Rdp => {
            HostAuthenticationMode::Password
        }
        RemoteProtocol::Telnet | RemoteProtocol::Serial => HostAuthenticationMode::Agent,
    }
}
