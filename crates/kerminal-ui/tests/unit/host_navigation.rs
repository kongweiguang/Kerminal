//! 左侧主机树筛选行为的隔离测试，不创建 GPUI window 或访问配置仓储。
//!
//! @author kongweiguang

use kerminal_application::connections::management::{
    HostAuthenticationMode, HostGroup, HostRouteSettings, ManagedHost,
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
    operations::{ContainerRuntime, ContainerTarget},
    preferences::{ConfigRevision, TerminalProfile},
};

use kerminal_application::preferences::VersionedProfile;
use kerminal_application::workspace::WorkspacePinnedContainer;

use super::{
    GroupNavigationItem, HostContextMenuActions, HostNavigationEvent, HostPrimaryAction,
    SavedNavigationTarget, all_groups_are_collapsed, apply_saved_navigation_target, filtered_hosts,
    filtered_profiles, group_navigation_items, group_sort_order_after_pin,
    host_context_menu_actions, host_draft_for_group, host_group_accessibility_label,
    host_navigation_layout_metrics, is_group_pinned, is_open_gesture,
    local_host_accessibility_label, merge_session_status, navigation_group_ids,
    pinned_container_matches_query, primary_host_action, profile_group_is_ungrouped,
    should_render_saved_group_header, split_target_hosts, unique_default_profile_name,
};
use crate::features::connections::HostSessionStatus;

/// 保存的远端/本地目标互斥选中，并清除先前不同类型的导航身份。
/// @author kongweiguang
#[test]
fn saved_navigation_selection_switches_by_stable_id() {
    let host_id = HostId::new();
    let mut selected_host = None;
    let mut selected_profile = Some("old-profile".to_owned());

    apply_saved_navigation_target(
        &mut selected_host,
        &mut selected_profile,
        SavedNavigationTarget::Host(host_id.clone()),
    );
    assert_eq!(selected_host, Some(host_id));
    assert_eq!(selected_profile, None);

    apply_saved_navigation_target(
        &mut selected_host,
        &mut selected_profile,
        SavedNavigationTarget::LocalProfile("profile-2".to_owned()),
    );
    assert_eq!(selected_host, None);
    assert_eq!(selected_profile.as_deref(), Some("profile-2"));
}

/// Kit 在 render 时占用 hover 槽；直接探测该槽仍为空，避免整行按钮只在实窗启动时崩溃。
/// 此检查覆盖构造约束，不替代 Kit 渲染或原生点击验收。
/// @author kongweiguang
#[test]
fn navigation_rows_leave_hover_style_to_kit_render() {
    use gpui_kit::{InteractiveElement as _, WindowAppearance};

    for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
        let theme = crate::theme::ThemeTokens::for_appearance(appearance);
        for selected in [false, true] {
            let metrics = super::host_navigation_layout_metrics(
                kerminal_application::preferences::InterfaceDensity::Compact,
            );
            let _ =
                super::navigation_row_button("navigation-style-probe", selected, theme, metrics)
                    .hover(|style| style);
        }
    }
}

/// 三档 GPUI 侧栏密度跟随 main 的控件高度、列表节奏、行高和主机图标尺寸。
/// @author kongweiguang
#[test]
fn sidebar_density_matches_main_navigation_geometry() {
    use kerminal_application::preferences::InterfaceDensity;

    let compact = host_navigation_layout_metrics(InterfaceDensity::Compact);
    assert_eq!(
        (
            compact.control_height,
            compact.row_height,
            compact.group_header_height,
            compact.tree_gap,
            compact.row_gap,
            compact.icon_size,
        ),
        (28., 40., 32., 7., 8., 28.)
    );

    let comfortable = host_navigation_layout_metrics(InterfaceDensity::Comfortable);
    assert_eq!(
        (
            comfortable.control_height,
            comfortable.row_height,
            comfortable.group_header_height,
            comfortable.tree_gap,
            comfortable.row_gap,
            comfortable.icon_size,
        ),
        (32., 48., 32., 10., 10., 30.)
    );

    let spacious = host_navigation_layout_metrics(InterfaceDensity::Spacious);
    assert_eq!(
        (
            spacious.control_height,
            spacious.row_height,
            spacious.group_header_height,
            spacious.tree_gap,
            spacious.row_gap,
            spacious.icon_size,
        ),
        (36., 54., 32., 12., 12., 32.)
    );
}

/// 只构造导航筛选所需的完整 managed host 快照，备注为空以保持筛选样例不含额外语义。
fn host(
    name: &str,
    address: &str,
    group_id: Option<&str>,
    favorite: bool,
    tags: &[&str],
) -> ManagedHost {
    ManagedHost {
        id: HostId::new(),
        group_id: group_id.map(str::to_owned),
        name: name.to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: address.to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        is_favorite: favorite,
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("fixture-revision".to_owned()),
    }
}

/// 为左栏筛选测试构造稳定的本地 Profile 快照，不依赖磁盘配置和 GPUI 状态。
/// @author kongweiguang
fn profile(id: &str, group_id: Option<&str>) -> VersionedProfile {
    VersionedProfile {
        profile: TerminalProfile {
            id: id.to_owned(),
            name: id.to_owned(),
            shell: "pwsh".to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Default::default(),
            is_default: false,
            sidebar_group_id: group_id.map(str::to_owned),
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        },
        revision: ConfigRevision::from_sha256(format!("revision-{id}")),
    }
}

/// 标题投影只接受唯一且可读的默认 Profile，避免把排序首项误称为运行目标。
///
/// @author kongweiguang
#[test]
fn default_profile_title_requires_one_named_default() {
    let mut default = profile("PowerShell 7", None);
    default.profile.is_default = true;
    assert_eq!(
        unique_default_profile_name(&[default.clone()]).map(|name| name.to_string()),
        Some("PowerShell 7".to_owned())
    );
    assert!(unique_default_profile_name(&[]).is_none());

    let mut second = profile("Another", None);
    second.profile.is_default = true;
    assert!(unique_default_profile_name(&[default, second]).is_none());

    let mut unnamed = profile("blank", None);
    unnamed.profile.is_default = true;
    unnamed.profile.name = "  ".to_owned();
    assert!(unique_default_profile_name(&[unnamed]).is_none());
}

/// 保存的空分组可在普通列表中管理，搜索时不会把所有空组都放出来。
/// @author kongweiguang
#[test]
fn saved_empty_group_visibility_respects_search() {
    assert!(should_render_saved_group_header("Servers", "", false));
    assert!(should_render_saved_group_header("Servers", "serv", false));
    assert!(!should_render_saved_group_header(
        "Servers", "database", false
    ));
    assert!(should_render_saved_group_header(
        "Servers", "database", true
    ));
}

/// 整行分组按钮暴露项目数、展开状态与键盘动作；断言中英完整文案以避免依赖并行测试中的全局语言。
/// @author kongweiguang
#[test]
fn group_row_accessibility_label_exposes_state_and_keyboard_action() {
    let collapsed = host_group_accessibility_label("生产", 3, true, false).to_string();
    assert!(matches!(
        collapsed.as_str(),
        "分组：生产，3 项，已折叠，按 Enter 或空格展开"
            | "Group: 生产, 3 items, collapsed. Press Enter or Space to expand."
    ));

    let expanded = host_group_accessibility_label("Production", 0, false, true).to_string();
    assert!(matches!(
        expanded.as_str(),
        "分组：Production，已置顶，0 项，已展开，按 Enter 或空格折叠"
            | "Group: Production, pinned, 0 items, expanded. Press Enter or Space to collapse."
    ));
}

/// 置顶继续沿用旧版 sort_order<0 的持久化约定，并以每 10 位间隔将条目推到对应序列末端。
/// @author kongweiguang
#[test]
fn group_pin_order_uses_legacy_negative_sort_order_without_extra_schema() {
    let groups = vec![
        HostGroup {
            id: "pinned:first".to_owned(),
            name: "Pinned first".to_owned(),
            sort_order: -20,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        },
        HostGroup {
            id: "pinned:second".to_owned(),
            name: "Pinned second".to_owned(),
            sort_order: -10,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        },
        HostGroup {
            id: "group:prod".to_owned(),
            name: "Production".to_owned(),
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        },
        HostGroup {
            id: "group:dev".to_owned(),
            name: "Development".to_owned(),
            sort_order: 30,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        },
    ];

    assert!(is_group_pinned(-1));
    assert!(!is_group_pinned(0));
    assert_eq!(
        group_sort_order_after_pin(&groups, "group:prod", true),
        Some(-30)
    );
    assert_eq!(
        group_sort_order_after_pin(&groups, "pinned:second", false),
        Some(40)
    );
    assert_eq!(
        group_sort_order_after_pin(&groups, "pinned:first", true),
        Some(-20)
    );
    assert_eq!(group_sort_order_after_pin(&groups, "missing", true), None);
}

/// 全部折叠入口覆盖保存组及未分组条目，空目录与部分折叠状态按明确规则处理。
/// @author kongweiguang
#[test]
fn global_group_toggle_projects_saved_and_ungrouped_sections() {
    let hosts = vec![
        host("Known", "known.example", Some("group:prod"), false, &[]),
        host("Loose", "loose.example", None, false, &[]),
        host(
            "Orphan",
            "orphan.example",
            Some("group:deleted"),
            false,
            &[],
        ),
    ];
    let profiles = vec![profile("local", None)];
    let groups = vec![HostGroup {
        id: "group:prod".to_owned(),
        name: "Production".to_owned(),
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }];
    let ids = navigation_group_ids(&hosts, &profiles, &groups);

    assert_eq!(
        ids.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["group:prod", "__ungrouped"]
    );
    assert!(!all_groups_are_collapsed(&[], &Default::default()));
    assert!(!all_groups_are_collapsed(&ids, &Default::default()));
    assert!(all_groups_are_collapsed(
        &ids,
        &ids.iter().cloned().collect()
    ));
}

/// main 的分组行保持固定紧凑高度，密度只调整主机行与间距，整行命中不增加隐形高度。
/// @author kongweiguang
#[test]
fn group_row_hit_area_does_not_increase_navigation_density() {
    for (density, expected_group_height) in [
        (
            kerminal_application::preferences::InterfaceDensity::Compact,
            32.0,
        ),
        (
            kerminal_application::preferences::InterfaceDensity::Comfortable,
            32.0,
        ),
        (
            kerminal_application::preferences::InterfaceDensity::Spacious,
            32.0,
        ),
    ] {
        let metrics = super::host_navigation_layout_metrics(density);
        assert_eq!(metrics.group_header_height, expected_group_height);
        assert!(metrics.group_header_height < metrics.row_height);
    }
}

/// 搜索在名称、地址和标签中不区分 ASCII 大小写，并忽略外围空格。
#[test]
fn search_matches_name_address_and_tags() {
    let hosts = vec![
        host("Database", "db.internal", None, false, &[]),
        host("Build", "ci.internal", None, false, &["Staging"]),
    ];

    assert_eq!(filtered_hosts(&hosts, &[], "  DATABASE ").len(), 1);
    assert_eq!(filtered_hosts(&hosts, &[], "DB.INTERNAL").len(), 1);
    assert_eq!(filtered_hosts(&hosts, &[], "staging").len(), 1);
}

/// 搜索为空时保留全部分组与收藏状态不同的主机，不再投影“全部/收藏/其他”额外筛选层。
#[test]
fn unfiltered_navigation_keeps_all_hosts_in_the_group_tree() {
    let hosts = vec![
        host("Favorite", "fav.internal", Some("group:prod"), true, &[]),
        host(
            "Production",
            "prod.internal",
            Some("group:prod"),
            false,
            &[],
        ),
        host("Local", "local.internal", None, true, &[]),
        host("Stale", "stale.internal", Some("group:deleted"), false, &[]),
    ];
    let groups = vec![HostGroup {
        id: "group:prod".to_owned(),
        name: "Production group".to_owned(),
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }];

    assert_eq!(filtered_hosts(&hosts, &groups, "").len(), 4);
    assert_eq!(filtered_hosts(&hosts, &groups, "production group").len(), 2);
}

/// 已删除分组的 Profile 归入未分组投影；菜单新建沿用相同归一化，不能发出悬空组 ID。
/// @author kongweiguang
#[test]
fn profiles_with_missing_group_remain_in_ungrouped_navigation() {
    let profiles = vec![
        profile("kept", Some("group:dev")),
        profile("orphaned", Some("group:deleted")),
        profile("plain", None),
    ];
    let groups = vec![HostGroup {
        id: "group:dev".to_owned(),
        name: "Development".to_owned(),
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }];

    let all_profiles = filtered_profiles(&profiles, &groups, "");
    assert_eq!(all_profiles.len(), 3);
    let ungrouped = all_profiles
        .iter()
        .filter(|row| profile_group_is_ungrouped(row.profile.sidebar_group_id.as_deref(), &groups))
        .collect::<Vec<_>>();
    assert_eq!(ungrouped.len(), 2);
    assert_eq!(
        filtered_profiles(&profiles, &groups, "orphaned")
            .first()
            .map(|row| row.profile.id.as_str()),
        Some("orphaned")
    );
    assert!(profile_group_is_ungrouped(Some("group:deleted"), &groups));
    assert_eq!(
        super::effective_navigation_group(Some("group:deleted"), &groups),
        None
    );
    assert_eq!(super::effective_navigation_group(None, &groups), None);
    assert_eq!(
        super::effective_navigation_group(Some("group:dev"), &groups).as_deref(),
        Some("group:dev")
    );
}

/// 本地主机和远程连接使用同一排序；并列名称有稳定类型顺序但身份仍各自保留。
/// @author kongweiguang
#[test]
fn local_and_remote_hosts_interleave_by_order_and_name() {
    let mut remote_charlie = host("Charlie", "charlie.internal", Some("group"), false, &[]);
    remote_charlie.sort_order = 1;
    let mut remote_equal = host("Equal", "equal.internal", Some("group"), false, &[]);
    remote_equal.sort_order = 2;

    let mut local_alpha = profile("local-alpha", Some("group"));
    local_alpha.profile.name = "Alpha".to_owned();
    local_alpha.profile.sort_order = 1;
    let mut local_bravo = profile("local-bravo", Some("group"));
    local_bravo.profile.name = "Bravo".to_owned();
    local_bravo.profile.sort_order = 1;
    let mut local_equal = profile("local-equal", Some("group"));
    local_equal.profile.name = "Equal".to_owned();
    local_equal.profile.sort_order = 2;

    let items = group_navigation_items(
        &[remote_charlie, remote_equal],
        &[local_alpha, local_bravo, local_equal],
        &[],
        "",
    );
    let displayed = items
        .iter()
        .map(|item| match item {
            GroupNavigationItem::RemoteHost(host) => host.name.as_str(),
            GroupNavigationItem::LocalHost(profile) => profile.profile.name.as_str(),
            GroupNavigationItem::PinnedContainer(_) => panic!("no pins were projected"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        displayed,
        vec!["Alpha", "Bravo", "Charlie", "Equal", "Equal"]
    );
    assert!(matches!(&items[0], &GroupNavigationItem::LocalHost(_)));
    assert!(matches!(&items[2], &GroupNavigationItem::RemoteHost(_)));
    assert!(matches!(&items[3], &GroupNavigationItem::LocalHost(_)));
    assert!(matches!(&items[4], &GroupNavigationItem::RemoteHost(_)));
}

/// 固定容器排在父 SSH 主机之后，名称或容器 ID 命中搜索时仍保留父行。
/// @author kongweiguang
#[test]
fn pinned_container_is_nested_after_its_parent_and_searchable() {
    let parent = host("Production", "prod.internal", Some("group"), false, &[]);
    let pin = WorkspacePinnedContainer {
        target: ContainerTarget {
            host_id: Some(parent.id.clone()),
            runtime: ContainerRuntime::Podman,
            container_id: "long-container-id-123".to_owned(),
        },
        name: "api-worker".to_owned(),
        description: "registry/api · running".to_owned(),
        group_id: Some("group".to_owned()),
    };

    let items = group_navigation_items(
        std::slice::from_ref(&parent),
        &[],
        std::slice::from_ref(&pin),
        "",
    );
    assert!(matches!(
        items.as_slice(),
        [
            GroupNavigationItem::RemoteHost(_),
            GroupNavigationItem::PinnedContainer(_)
        ]
    ));
    assert!(pinned_container_matches_query(&pin, "api-worker"));
    assert!(pinned_container_matches_query(&pin, "long-container-id"));
    assert!(!pinned_container_matches_query(&pin, "unrelated"));
}

/// 导航朗读本地主机术语，不把内部 Profile 命名泄露到用户可见的辅助技术文案。
/// @author kongweiguang
#[test]
fn local_host_accessible_name_uses_product_language() {
    let label = local_host_accessibility_label("PowerShell").to_string();
    assert!(label.contains("PowerShell"));
    assert!(!label.to_lowercase().contains("profile"));
    assert!(label.starts_with("本地主机：") || label.starts_with("Local host:"));
}

/// 右键菜单动作只投影为对应的本地主机事件，不隐式选择该行或切换中央工作区。
/// @author kongweiguang
#[test]
fn local_host_context_actions_preserve_identity_without_selecting_row() {
    let id = "local-profile-17".to_owned();
    let edit = super::local_profile_action_event(id.clone(), super::LocalProfileAction::Edit);
    assert_eq!(
        edit,
        HostNavigationEvent::EditLocalProfileRequested {
            profile_id: id.clone()
        }
    );
    assert!(!matches!(
        edit,
        HostNavigationEvent::LocalProfileSelected { .. }
    ));
    let copy = super::local_profile_action_event(id.clone(), super::LocalProfileAction::Copy);
    assert_eq!(
        copy,
        HostNavigationEvent::CopyLocalProfileRequested {
            profile_id: id.clone()
        }
    );
    assert!(!matches!(
        copy,
        HostNavigationEvent::LocalProfileSelected { .. }
    ));
    let delete = super::local_profile_action_event(id.clone(), super::LocalProfileAction::Delete);
    assert_eq!(
        delete,
        HostNavigationEvent::DeleteLocalProfileRequested {
            profile_id: id.clone()
        }
    );
    assert!(!matches!(
        delete,
        HostNavigationEvent::LocalProfileSelected { .. }
    ));
}

/// 鼠标单击仍只选择，双击只连接一次；Kit 的 Enter/Space 确认不能因 click_count 为 1 被降级为选择。
/// @author kongweiguang
#[test]
fn only_second_click_in_a_click_sequence_opens_the_target() {
    use gpui_kit::{ClickEvent, KeyboardButton, KeyboardClickEvent, MouseClickEvent, MouseUpEvent};

    for (click_count, opens) in [(1, false), (2, true), (3, false)] {
        let event = ClickEvent::Mouse(MouseClickEvent {
            up: MouseUpEvent {
                click_count,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(is_open_gesture(&event), opens);
    }
    for button in [KeyboardButton::Enter, KeyboardButton::Space] {
        let event = ClickEvent::Keyboard(KeyboardClickEvent {
            button,
            ..Default::default()
        });
        assert_eq!(event.click_count(), 1);
        assert!(is_open_gesture(&event));
    }
}

/// SFTP-only 主机双击进入文件工作区，其它协议走原连接映射。
#[test]
fn primary_action_respects_sftp_only_protocol() {
    assert_eq!(
        primary_host_action(RemoteProtocol::Sftp),
        HostPrimaryAction::OpenSftp
    );
    assert_eq!(
        primary_host_action(RemoteProtocol::Ssh),
        HostPrimaryAction::Connect
    );
    assert_eq!(
        primary_host_action(RemoteProtocol::Rdp),
        HostPrimaryAction::Connect
    );
}

/// 右键菜单动作按当前协议与已启用设置投影；禁用时仍保留 SSH 终端，SFTP-only 不伪造主动作。
/// 同一策略也由菜单回调按最新主机快照复验，覆盖菜单打开后能力被关闭的情况。
///
/// @author kongweiguang
#[test]
fn host_context_menu_actions_follow_enabled_sftp_capability() {
    let enabled_ssh = host("SSH", "ssh.example", None, false, &[]);
    assert_eq!(
        host_context_menu_actions(&enabled_ssh),
        HostContextMenuActions {
            show_primary: true,
            show_open_files: true,
            show_transfer_tab: true,
        }
    );

    let mut disabled_ssh = enabled_ssh;
    disabled_ssh.sftp_options.enabled = false;
    assert_eq!(
        host_context_menu_actions(&disabled_ssh),
        HostContextMenuActions {
            show_primary: true,
            show_open_files: false,
            show_transfer_tab: false,
        }
    );

    let mut enabled_sftp_only = host("SFTP", "sftp.example", None, false, &[]);
    enabled_sftp_only.protocol = RemoteProtocol::Sftp;
    assert_eq!(
        host_context_menu_actions(&enabled_sftp_only),
        HostContextMenuActions {
            show_primary: true,
            show_open_files: false,
            show_transfer_tab: true,
        }
    );

    enabled_sftp_only.sftp_options.enabled = false;
    assert_eq!(
        host_context_menu_actions(&enabled_sftp_only),
        HostContextMenuActions {
            show_primary: false,
            show_open_files: false,
            show_transfer_tab: false,
        }
    );

    let mut rdp = host("RDP", "rdp.example", None, false, &[]);
    rdp.protocol = RemoteProtocol::Rdp;
    assert_eq!(
        host_context_menu_actions(&rdp),
        HostContextMenuActions {
            show_primary: true,
            show_open_files: false,
            show_transfer_tab: false,
        }
    );
}

/// 分屏目标只允许可启动终端的主机协议；纯 SFTP 与 RDP 不得伪装成 shell。
#[test]
fn split_target_filter_excludes_file_and_remote_desktop_hosts() {
    let mut hosts = vec![
        host("SSH", "ssh.example", None, false, &[]),
        host("Telnet", "telnet.example", None, false, &[]),
        host("Serial", "serial.example", None, false, &[]),
        host("SFTP", "sftp.example", None, false, &[]),
        host("RDP", "rdp.example", None, false, &[]),
    ];
    hosts[1].protocol = RemoteProtocol::Telnet;
    hosts[2].protocol = RemoteProtocol::Serial;
    hosts[3].protocol = RemoteProtocol::Sftp;
    hosts[4].protocol = RemoteProtocol::Rdp;

    let candidates = split_target_hosts(&hosts, &[], "");

    assert_eq!(candidates.len(), 3);
    assert!(candidates.iter().all(|host| matches!(
        host.protocol,
        RemoteProtocol::Ssh | RemoteProtocol::Telnet | RemoteProtocol::Serial
    )));
}

/// 分组移动只替换 group ID，所有连接、认证引用和协议默认值都沿用同一份草稿。
#[test]
fn moving_host_preserves_the_complete_connection_draft() {
    let mut original = host(
        "Build",
        "ci.internal",
        Some("group:old"),
        true,
        &["staging"],
    );
    original.endpoint.username = "deploy".to_owned();
    original.connect_timeout_ms = 45_000;
    original.keepalive_interval_ms = Some(15_000);

    let moved = host_draft_for_group(&original, Some("group:new".to_owned()));

    assert_eq!(moved.id.as_ref(), Some(&original.id));
    assert_eq!(moved.group_id.as_deref(), Some("group:new"));
    assert_eq!(moved.name, original.name);
    assert_eq!(moved.protocol, original.protocol);
    assert_eq!(moved.endpoint, original.endpoint);
    assert_eq!(moved.authentication, original.authentication);
    assert_eq!(moved.rdp_password, original.rdp_password);
    assert_eq!(moved.route, original.route);
    assert_eq!(moved.tags, original.tags);
    assert_eq!(moved.is_favorite, original.is_favorite);
    assert_eq!(moved.connect_timeout_ms, original.connect_timeout_ms);
    assert_eq!(moved.keepalive_interval_ms, original.keepalive_interval_ms);
    assert_eq!(moved.terminal_options, original.terminal_options);
    assert_eq!(moved.sftp_options, original.sftp_options);
    assert_eq!(moved.rdp_display, original.rdp_display);
}

/// 汇总多个同主机会话时 Ready 优先，其次保留 Opening，避免失败 pane 遮住可用连接。
#[test]
fn live_session_projection_prefers_ready_then_opening() {
    assert_eq!(
        merge_session_status(HostSessionStatus::Failed, HostSessionStatus::Ready),
        HostSessionStatus::Ready
    );
    assert_eq!(
        merge_session_status(HostSessionStatus::Failed, HostSessionStatus::Opening),
        HostSessionStatus::Opening
    );
    assert_eq!(
        merge_session_status(HostSessionStatus::Failed, HostSessionStatus::Failed),
        HostSessionStatus::Failed
    );
}

#[path = "host_navigation_menus.rs"]
mod host_navigation_menus;
