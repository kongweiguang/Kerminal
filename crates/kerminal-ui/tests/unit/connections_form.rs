//! 连接编辑器代理密码的保留、替换和显式清除状态回归。
//!
//! @author kongweiguang

use super::super::form_render::{endpoint_port_column_min_width_px, network_field_widths_px};
use super::{
    HostCredentialChange, HostCredentialSlot, HostEditorField, HostEditorSection,
    HostEditorValidationIssue, append_proxy_credential_change, default_authentication,
    has_unsaved_protocol_draft, initial_host_editor_protocol, optional_option_text,
    parse_option_number, private_key_file_source_selected, project_rdp_password,
    proxy_password_matches_route, serial_data_bits_options, serial_flow_options,
    serial_parity_options, serial_stop_bits_options,
    should_clear_saved_private_key_for_file_source,
};
use kerminal_application::{AppError, connections::management::HostAuthenticationMode};
use kerminal_domain::connections::{
    CredentialRef, HostEndpoint, ProxyProfile, ProxyProtocol, RemoteProtocol,
};
use kerminal_domain::sessions::{SerialDataBits, SerialFlowControl, SerialParity, SerialStopBits};

/// 活跃页干净时仍需保留隐藏协议的离开时 dirty，切回后才能正确提示关闭确认。
/// @author kongweiguang
#[test]
fn hidden_protocol_dirty_state_is_aggregated_with_active_draft() {
    assert!(has_unsaved_protocol_draft(false, [true, false].into_iter()));
    assert!(has_unsaved_protocol_draft(true, [false, false].into_iter()));
    assert!(!has_unsaved_protocol_draft(
        false,
        [false, false].into_iter()
    ));
}

/// 文件来源只在非空路径且存在旧引用时要求清除旧 vault key；空值不能误删凭据。
/// @author kongweiguang
#[test]
fn private_key_file_source_clears_only_an_existing_key_for_a_nonempty_path() {
    assert!(private_key_file_source_selected(
        "C:\\keys\\id_ed25519",
        true
    ));
    assert!(!private_key_file_source_selected("", true));
    assert!(!private_key_file_source_selected(
        "C:\\keys\\id_ed25519",
        false
    ));

    assert!(should_clear_saved_private_key_for_file_source(
        true,
        "C:\\keys\\id_ed25519",
        true,
    ));
    assert!(!should_clear_saved_private_key_for_file_source(
        true, "", true
    ));
    assert!(!should_clear_saved_private_key_for_file_source(
        true,
        "C:\\keys\\id_ed25519",
        false,
    ));
    assert!(!should_clear_saved_private_key_for_file_source(
        false,
        "C:\\keys\\id_ed25519",
        true,
    ));
}

/// 端口列同时容纳通用标签与至少 80px 输入，防止默认端口值在窄布局中被裁切。
/// @author kongweiguang
#[test]
fn endpoint_port_column_reserves_readable_input_width() {
    assert!(endpoint_port_column_min_width_px() >= 184.0);
}

/// 网络标签复用 6rem 对齐轴，范围说明移至输入下方，数字控件仍保留可读上限。
/// @author kongweiguang
#[test]
fn network_numeric_fields_keep_readable_labels_and_bounded_controls() {
    let (label_width, control_max_width) = network_field_widths_px();

    assert_eq!(label_width, 96.0);
    assert!((144.0..=224.0).contains(&control_max_width));
}

/// 空白字段只表示保留已保存的 opaque ref，不得生成 vault 写入请求。
#[test]
fn empty_proxy_password_keeps_saved_reference() {
    let mut changes = Vec::new();
    append_proxy_credential_change(&mut changes, false, String::new());

    assert!(changes.is_empty());
}

/// 新密码只产生代理密码槽位的替换请求，明文不会被放入主机草稿。
#[test]
fn entered_proxy_password_replaces_the_proxy_slot() {
    let mut changes = Vec::new();
    append_proxy_credential_change(&mut changes, false, "one-time secret".to_owned());

    assert!(matches!(
        changes.as_slice(),
        [HostCredentialChange::Replace {
            slot: HostCredentialSlot::ProxyPassword,
            ..
        }]
    ));
}

/// 用户显式点选清除时，即使密码输入为空也要提交独立的 Clear 操作。
#[test]
fn explicit_clear_removes_the_proxy_slot() {
    let mut changes = Vec::new();
    append_proxy_credential_change(&mut changes, true, String::new());

    assert!(matches!(
        changes.as_slice(),
        [HostCredentialChange::Clear {
            slot: HostCredentialSlot::ProxyPassword
        }]
    ));
}

/// 只有原代理协议、地址、端口和用户名都一致时才保留旧 vault 密码引用。
#[test]
fn proxy_password_is_not_reused_after_route_identity_changes() {
    let original = ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: "proxy.example".to_owned(),
            port: 1080,
            username: "proxy-user".to_owned(),
        },
        password: Some(CredentialRef::new("test:proxy-password").expect("valid test ref")),
    };

    assert!(proxy_password_matches_route(
        Some(&original),
        ProxyProtocol::Socks5,
        " proxy.example ",
        Some(1080),
        "proxy-user",
    ));
    assert!(!proxy_password_matches_route(
        Some(&original),
        ProxyProtocol::Socks5,
        "other.example",
        Some(1080),
        "proxy-user",
    ));
    assert!(!proxy_password_matches_route(
        Some(&original),
        ProxyProtocol::HttpConnect,
        "proxy.example",
        Some(1080),
        "proxy-user",
    ));
    assert!(!proxy_password_matches_route(
        Some(&original),
        ProxyProtocol::Socks5,
        "proxy.example",
        Some(1080),
        "other-user",
    ));
}

/// 编辑侧栏按协议列出全部实际设置；身份与认证收在首个属性页，协议切换后回到该页。
///
/// @author kongweiguang
#[test]
fn editor_sections_follow_protocol_capabilities() {
    assert_eq!(
        HostEditorSection::for_protocol(RemoteProtocol::Ssh),
        &[
            HostEditorSection::Properties,
            HostEditorSection::Proxy,
            HostEditorSection::JumpHosts,
            HostEditorSection::Network,
            HostEditorSection::Terminal,
            HostEditorSection::Transfer,
        ]
    );
    assert!(
        !HostEditorSection::for_protocol(RemoteProtocol::Sftp)
            .contains(&HostEditorSection::Terminal)
    );
    assert!(
        HostEditorSection::for_protocol(RemoteProtocol::Sftp)
            .contains(&HostEditorSection::Transfer)
    );
    assert_eq!(
        HostEditorSection::for_protocol(RemoteProtocol::Rdp),
        &[HostEditorSection::Properties, HostEditorSection::Display]
    );
    assert_eq!(
        HostEditorSection::for_protocol(RemoteProtocol::Telnet),
        &[HostEditorSection::Properties]
    );
    assert_eq!(
        HostEditorSection::for_protocol(RemoteProtocol::Serial),
        &[HostEditorSection::Properties, HostEditorSection::Terminal,]
    );
    assert_eq!(
        HostEditorSection::normalize(RemoteProtocol::Rdp, HostEditorSection::Proxy),
        HostEditorSection::Properties
    );
    assert_eq!(
        HostEditorSection::normalize(RemoteProtocol::Ssh, HostEditorSection::Proxy),
        HostEditorSection::Proxy
    );
}

/// 已知校验错误定位到真实输入或高级分区；未知错误继续由通用状态反馈承接。
/// @author kongweiguang
#[test]
fn editor_validation_errors_project_to_local_fields_without_guessing_unknowns() {
    let name_error = AppError::invalid("主机名称必须为 1 到 128 个可见字符");
    let name_issue = HostEditorValidationIssue::from_error(
        &name_error,
        "Host name must contain 1 to 128 visible characters".to_owned(),
    )
    .expect("known name validation is localizable");
    assert_eq!(name_issue.field, HostEditorField::Name);
    assert_eq!(name_issue.section, HostEditorSection::Properties);

    let timeout_error = AppError::invalid("SSH 超时必须为 1 到 600 秒");
    let timeout_issue = HostEditorValidationIssue::from_error(
        &timeout_error,
        "SSH timeout must be between 1 and 600 seconds".to_owned(),
    )
    .expect("known timeout validation is localizable");
    assert_eq!(timeout_issue.field, HostEditorField::Network);
    assert_eq!(timeout_issue.section, HostEditorSection::Network);

    let unknown_error = AppError::invalid("a validation not mapped by the editor");
    assert!(
        HostEditorValidationIssue::from_error(&unknown_error, "safe message".to_owned()).is_none()
    );
}

/// 分组与标签始终位于可见的属性分区，校验失败可直接定位到当前表单。
/// @author kongweiguang
#[test]
fn group_and_tag_validation_errors_point_to_visible_properties() {
    let group_error = AppError::not_found("所选主机分组已不存在，请刷新后重新选择");
    let group_issue = HostEditorValidationIssue::from_error(
        &group_error,
        "The selected host group no longer exists".to_owned(),
    )
    .expect("known group validation is localizable");
    assert_eq!(group_issue.field, HostEditorField::Group);
    assert_eq!(group_issue.section, HostEditorSection::Properties);

    let tags_error = AppError::invalid("主机标签不能包含控制字符");
    let tags_issue = HostEditorValidationIssue::from_error(
        &tags_error,
        "Host tags cannot contain control characters".to_owned(),
    )
    .expect("known tag validation is localizable");
    assert_eq!(tags_issue.field, HostEditorField::Tags);
    assert_eq!(tags_issue.section, HostEditorSection::Properties);
}

/// SFTP picker 新建默认进入 SFTP 表单，而编辑已有连接始终保留磁盘协议。
/// @author kongweiguang
#[test]
fn requested_initial_protocol_only_applies_to_new_hosts() {
    assert_eq!(
        initial_host_editor_protocol(None, RemoteProtocol::Sftp),
        RemoteProtocol::Sftp
    );
    assert_eq!(
        initial_host_editor_protocol(Some(RemoteProtocol::Ssh), RemoteProtocol::Sftp),
        RemoteProtocol::Ssh
    );
}

/// 串口选择器展示短标签但提交冻结领域枚举，不从翻译后的文案反解析配置。
/// @author kongweiguang
#[test]
fn serial_selectors_keep_typed_values_and_compact_labels() {
    assert_eq!(
        serial_data_bits_options()
            .into_iter()
            .map(|option| (option.value, option.label.to_string()))
            .collect::<Vec<_>>(),
        vec![
            (SerialDataBits::Five, "5".to_owned()),
            (SerialDataBits::Six, "6".to_owned()),
            (SerialDataBits::Seven, "7".to_owned()),
            (SerialDataBits::Eight, "8".to_owned()),
        ]
    );
    assert_eq!(
        serial_stop_bits_options()
            .into_iter()
            .map(|option| (option.value, option.label.to_string()))
            .collect::<Vec<_>>(),
        vec![
            (SerialStopBits::One, "1".to_owned()),
            (SerialStopBits::Two, "2".to_owned()),
        ]
    );
    assert_eq!(
        serial_parity_options()
            .into_iter()
            .map(|option| option.value)
            .collect::<Vec<_>>(),
        vec![SerialParity::None, SerialParity::Even, SerialParity::Odd]
    );
    assert_eq!(
        serial_flow_options()
            .into_iter()
            .map(|option| option.value)
            .collect::<Vec<_>>(),
        vec![
            SerialFlowControl::None,
            SerialFlowControl::Software,
            SerialFlowControl::Hardware,
        ]
    );
}

/// 继承、关闭与非法输入必须可区分，避免高级项被静默改成默认值后保存。
///
/// @author kongweiguang
#[test]
fn optional_host_numbers_reject_invalid_values_without_coercion() {
    assert_eq!(
        parse_option_number("", 1, 8, "invalid").expect("empty inherits"),
        None
    );
    assert_eq!(
        parse_option_number("0", 0, 600, "invalid").expect("zero disables"),
        Some(0)
    );
    assert!(parse_option_number("0", 1, 8, "invalid").is_err());
    assert!(parse_option_number("1.5", 1, 8, "invalid").is_err());
    assert!(parse_option_number("9", 1, 8, "invalid").is_err());
    assert_eq!(optional_option_text("  "), None);
    assert_eq!(
        optional_option_text(" xterm-256color ").as_deref(),
        Some("xterm-256color")
    );
}

/// 保存 RDP 时保留原凭据；转换到 Telnet 或 Serial 时草稿不得携带 RDP vault 引用。
/// @author kongweiguang
#[test]
fn rdp_credential_is_retained_only_for_rdp_drafts() {
    let reference = CredentialRef::new("test:rdp-password").expect("valid test ref");
    assert_eq!(
        project_rdp_password(RemoteProtocol::Rdp, Some(reference.clone())),
        Some(reference.clone())
    );
    for protocol in [RemoteProtocol::Telnet, RemoteProtocol::Serial] {
        assert_eq!(
            project_rdp_password(protocol, Some(reference.clone())),
            None
        );
    }
}

/// 新建 SSH/SFTP 直接使用密码认证默认值，编辑器会另行保留已保存的认证模式。
/// @author kongweiguang
#[test]
fn new_ssh_and_sftp_default_to_password_authentication() {
    for protocol in [RemoteProtocol::Ssh, RemoteProtocol::Sftp] {
        assert_eq!(
            default_authentication(protocol),
            HostAuthenticationMode::Password
        );
    }
}

/// 序列设备路径由串口分区承载，领域校验回投必须切换到该分区并定位设备字段。
/// @author kongweiguang
#[test]
fn serial_device_validation_targets_the_serial_section() {
    let issue = HostEditorValidationIssue::from_error(
        &AppError::invalid("串口设备路径必须能编码为 UTF-8"),
        "设备路径无效".to_owned(),
    )
    .expect("serial device errors are mapped locally");

    assert_eq!(issue.field, HostEditorField::Host);
    assert_eq!(issue.section, HostEditorSection::Terminal);
}
