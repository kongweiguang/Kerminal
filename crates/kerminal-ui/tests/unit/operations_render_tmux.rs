//! tmux 窄栏只在接入条件受限时显示简短、面向用户的提示。
//!
//! @author kongweiguang

use super::super::actions::resolve_tmux_create_working_directory;
use super::tmux_attach_guidance;
use super::{
    SectionPhase,
    tmux::{
        TmuxHeaderTone, TmuxPanelPresentation, default_tmux_session_name, sort_tmux_panel_sessions,
        tmux_detach_shortcut, tmux_header_status, tmux_header_tone,
        tmux_panel_session_matches_current, tmux_quickref_copy_text, tmux_ssh_host_display_label,
        tmux_tool_rail_feedback_visible, tmux_tool_rail_header_status,
        tmux_tool_rail_session_status,
    },
};
use kerminal_application::operations::TmuxCapability;
use kerminal_domain::operations::TmuxSession;

/// 就绪状态不占正文，未绑定与目标不匹配时保留各自的可执行原因。
///
/// @author kongweiguang
#[test]
fn tmux_attach_guidance_is_conditional_and_user_facing() {
    assert_eq!(tmux_attach_guidance(true, false), None);
    assert_eq!(tmux_attach_guidance(true, true), None);
    assert_eq!(
        tmux_attach_guidance(false, false),
        Some((
            "请聚焦当前目标对应的终端，再连接会话。",
            "Focus a terminal for this target to attach.",
        ))
    );
    assert_eq!(
        tmux_attach_guidance(false, true),
        Some((
            "当前终端与列表目标不一致，请切换到对应终端。",
            "The focused terminal does not match this list; switch to the matching terminal.",
        ))
    );
}

/// 标题状态只在真实 tmux 探测成功时呈现可用色，并保留失败与取消的重试语义。
///
/// @author kongweiguang
#[test]
fn tmux_header_reflects_query_phase_and_capability() {
    let available = TmuxCapability {
        target_ref: "local".to_owned(),
        available: true,
        version: Some("tmux 3.2a".to_owned()),
        reason: None,
    };
    let unavailable = TmuxCapability {
        target_ref: "local".to_owned(),
        available: false,
        version: None,
        reason: Some("tmux command was not found".to_owned()),
    };

    assert!(!tmux_header_status(SectionPhase::Ready, Some(&available)).is_empty());
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, Some(&available)),
        TmuxHeaderTone::Ready
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Ready, Some(&unavailable)),
        "tmux command was not found"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, Some(&unavailable)),
        TmuxHeaderTone::Error
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Loading, Some(&available)),
        TmuxHeaderTone::Loading
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Cancelled, None),
        TmuxHeaderTone::Neutral
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Ready, None),
        "Status unknown"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Ready, None),
        TmuxHeaderTone::Neutral
    );
    assert_eq!(
        tmux_header_status(SectionPhase::Failed, Some(&available)),
        "Read failed; retry available"
    );
    assert_eq!(
        tmux_header_tone(SectionPhase::Failed, Some(&available)),
        TmuxHeaderTone::Error
    );
}

/// 固定栏的展开集合按 session ID 独立保存，快捷命令折叠不会改写会话详情状态。
///
/// @author kongweiguang
#[test]
fn tmux_panel_presentation_keeps_independent_expand_states() {
    let mut presentation = TmuxPanelPresentation::default();
    assert!(presentation.expanded_session_ids.is_empty());
    assert!(!presentation.quickref_open);

    assert!(presentation.toggle_session_details("$1"));
    assert!(presentation.toggle_session_details("$2"));
    presentation.toggle_quickref();
    assert!(presentation.quickref_open);
    assert_eq!(presentation.expanded_session_ids.len(), 2);

    assert!(!presentation.toggle_session_details("$1"));
    presentation.toggle_quickref();
    assert!(!presentation.quickref_open);
    assert!(presentation.expanded_session_ids.contains("$2"));
}

/// 列表优先展示本 pane 当前会话，其次已连接会话，再按活动时间排序且不跨 target_ref 匹配。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_sessions_follow_current_attach_and_activity_priority() {
    let sessions = vec![
        tmux_session("recent", "recent", false, Some(500), "local"),
        tmux_session("attached", "attached", true, Some(5), "local"),
        tmux_session("current", "current", false, Some(1), "local"),
        tmux_session("current", "other target", false, Some(100), "ssh:other"),
    ];

    let sorted = sort_tmux_panel_sessions(sessions, Some("current"), None, "local");

    assert_eq!(
        sorted
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>(),
        ["current", "attached", "recent", "other target"]
    );
}

/// 名称匹配仅兼容同一 host target 下当前绑定的 session，其他 target 同名项不能抢占当前标记。
///
/// @author kongweiguang
#[test]
fn tmux_panel_current_name_match_keeps_target_scope() {
    let local = tmux_session("$2", "shared-name", false, Some(1), "local");
    let remote = tmux_session("$3", "shared-name", true, Some(500), "ssh:other");

    assert!(tmux_panel_session_matches_current(
        &local,
        None,
        Some("shared-name"),
        "local",
    ));
    assert!(!tmux_panel_session_matches_current(
        &remote,
        None,
        Some("shared-name"),
        "local",
    ));

    let sorted = sort_tmux_panel_sessions(vec![remote, local], None, Some("shared-name"), "local");
    assert_eq!(sorted[0].target_ref, "local");
}

/// 会话标签比颜色更直接地传达当前 pane、其它客户端和未连接状态。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_session_status_names_connection_scope() {
    assert_eq!(
        tmux_tool_rail_session_status(true, true),
        ("当前终端", "Current terminal")
    );
    assert_eq!(
        tmux_tool_rail_session_status(false, true),
        ("已连接", "Attached")
    );
    assert_eq!(
        tmux_tool_rail_session_status(false, false),
        ("未连接", "Detached")
    );
}

/// 显示反馈时必须有真实状态文案，避免空 flex 子项在标题卡与会话列表间累加间距。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_feedback_visibility_omits_only_empty_rows() {
    assert!(tmux_tool_rail_feedback_visible(
        SectionPhase::NotLoaded,
        false
    ));
    assert!(tmux_tool_rail_feedback_visible(
        SectionPhase::Loading,
        false
    ));
    assert!(tmux_tool_rail_feedback_visible(
        SectionPhase::Cancelled,
        false
    ));
    assert!(!tmux_tool_rail_feedback_visible(SectionPhase::Ready, false));
    assert!(!tmux_tool_rail_feedback_visible(
        SectionPhase::Failed,
        false
    ));
    assert!(tmux_tool_rail_feedback_visible(SectionPhase::Ready, true));
    assert!(tmux_tool_rail_feedback_visible(SectionPhase::Failed, true));
}

/// 复制快捷键写入人类可读的按键串，发送时才使用 Ctrl-B 控制字节。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_shortcut_copy_uses_display_text() {
    let detach = tmux_detach_shortcut().expect("the exit action has a fixed Ctrl-B+d entry");

    assert_eq!(tmux_quickref_copy_text(detach), "Ctrl-b d");
    assert_eq!(detach.payload, "\u{2}d");
}

/// 创建名称沿用 main 的目录名、目标名回退、tmux 字符规整和首个空闲数字后缀。
///
/// @author kongweiguang
#[test]
fn tmux_default_session_name_matches_main_and_avoids_collisions() {
    let timestamp = "20261027-120304";
    let first = default_tmux_session_name(Some("/srv/my api"), "production", timestamp, &[]);
    assert_eq!(first, "my-api-20261027-120304");

    let existing = vec![first.clone(), format!("{first}-2")];
    assert_eq!(
        default_tmux_session_name(None, "124.70.71.166 (root)", timestamp, &existing),
        "124_70_71_166-root-20261027-120304"
    );
    assert_eq!(
        default_tmux_session_name(Some("/srv/my api"), "production", timestamp, &existing,),
        format!("{first}-3")
    );
    assert_eq!(
        default_tmux_session_name(Some("/"), "", timestamp, &[]),
        "kerminal-20261027-120304"
    );
}

/// 工作目录提交只接受弹框打开时冻结的值；空白输入不会读取后来 pane 的 cwd。
///
/// @author kongweiguang
#[test]
fn tmux_create_working_directory_uses_frozen_snapshot_even_when_empty() {
    assert_eq!(
        resolve_tmux_create_working_directory("", Some("/opened/with")),
        Some("/opened/with".to_owned())
    );
    assert_eq!(resolve_tmux_create_working_directory("  \t", None), None);
    assert_eq!(
        resolve_tmux_create_working_directory(" /custom/path ", Some("/opened/with")),
        Some("/custom/path".to_owned())
    );
}

/// 标题状态优先展示版本并保留重试语义；并行测试共享全局 locale，状态文案接受两种产品语言。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_header_shows_capability_and_recovery_state() {
    let available = TmuxCapability {
        target_ref: "local".to_owned(),
        available: true,
        version: Some("tmux 3.2a".to_owned()),
        reason: None,
    };
    let unavailable = TmuxCapability {
        target_ref: "local".to_owned(),
        available: false,
        version: None,
        reason: Some("tmux command was not found".to_owned()),
    };

    assert_eq!(
        tmux_tool_rail_header_status(SectionPhase::Ready, Some(&available), true),
        "tmux 3.2a"
    );
    assert_eq!(
        tmux_tool_rail_header_status(SectionPhase::Ready, Some(&unavailable), true),
        "tmux command was not found"
    );
    let loading = tmux_tool_rail_header_status(SectionPhase::Loading, Some(&available), true);
    assert!(matches!(
        loading.as_str(),
        "正在检测 tmux…" | "Checking tmux…"
    ));
    let unavailable_target =
        tmux_tool_rail_header_status(SectionPhase::Ready, Some(&available), false);
    assert!(matches!(
        unavailable_target.as_str(),
        "目标不可用" | "Target unavailable"
    ));
}

/// 固定栏标题使用 Main 的目标别名，不把 SSH 传输类型拼进主机名称。
///
/// @author kongweiguang
#[test]
fn tmux_tool_rail_header_uses_host_alias_without_ssh_prefix() {
    assert_eq!(
        tmux_ssh_host_display_label(Some("Right rail fixture"), "host-id"),
        "Right rail fixture"
    );
    assert_eq!(tmux_ssh_host_display_label(None, "host-id"), "host-id");
    assert_eq!(
        tmux_ssh_host_display_label(Some("  "), "host-id"),
        "host-id"
    );
}

/// 构造只包含 renderer 排序和状态所需字段的 session 快照。
///
/// @author kongweiguang
fn tmux_session(
    id: &str,
    name: &str,
    attached: bool,
    activity_at: Option<u64>,
    target_ref: &str,
) -> TmuxSession {
    TmuxSession {
        id: id.to_owned(),
        name: name.to_owned(),
        attached,
        clients: if attached { 1 } else { 0 },
        windows: 1,
        current_path: None,
        created_at: None,
        activity_at,
        target_ref: target_ref.to_owned(),
    }
}
