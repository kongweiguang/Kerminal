//! Logs 的历史范围与迟到响应回归测试。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
};

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    commands::{CommandHistoryEntry, CommandHistorySource, CommandHistoryTarget},
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    sessions::{
        ContainerRuntime, ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget,
        SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget,
        TelnetSessionTarget, TerminalSize,
    },
};

use super::super::history_query::{HistoryScope, PAGE_SIZE, page_window};
use super::{
    HistoryFillFeedback, HistoryFillStatus, HistoryReadKey, LogsTarget, LogsTerminalFillCompleted,
    history_fill_command_is_valid, history_fill_completion_matches, operation_matches_target,
    response_matches_request,
};

/// 构造绑定一致的 UI 会话样本，确保测试使用与 Root 相同的稳定身份字段。
///
/// @author kongweiguang
fn ui_session(target: SessionTarget) -> (UiSessionBinding, SessionInfo, PaneId) {
    let session_id = SessionId::parse("session-logs-test").expect("valid session identity");
    let pane_id = PaneId::parse("pane-logs-test").expect("valid pane identity");
    let tab_id = TabId::parse("tab-logs-test").expect("valid tab identity");
    let owner = SessionOwner::Ui {
        tab_id,
        pane_id: pane_id.clone(),
    };
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: owner.clone(),
        session_generation: 7,
    };
    let info = SessionInfo {
        session_id,
        target,
        owner,
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    };
    (binding, info, pane_id)
}

/// 构造最小历史行；UI 展示测试只控制命令上下文，不依赖仓库、数据库或终端进程。
///
/// @author kongweiguang
fn history_entry() -> CommandHistoryEntry {
    CommandHistoryEntry {
        id: "history-entry-logs-test".to_owned(),
        command: "pwd".to_owned(),
        source: CommandHistorySource::User,
        target: CommandHistoryTarget::Local,
        session_id: None,
        pane_id: None,
        tab_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: None,
        shell: None,
        created_at: "2026-10-19T12:00:00Z".to_owned(),
    }
}

/// 旧 Logs 以 pane、目标和可信 host 过滤，不把短生命周期 session 写入历史 scope。
///
/// @author kongweiguang
#[test]
fn telnet_scope_keeps_pane_target_and_host_without_session_filter() {
    let host_id = HostId::parse("host-logs-test").expect("valid host identity");
    let (binding, info, pane_id) = ui_session(SessionTarget::Telnet(TelnetSessionTarget {
        host: "terminal.example.test".to_owned(),
        port: 23,
    }));
    let target = LogsTarget::from_ui_session(binding, &info, Some(host_id.clone()))
        .expect("matching UI binding creates a Logs target");

    let request = target.scope().list_request(
        "  deploy ".to_owned(),
        Some(CommandHistorySource::User),
        100,
    );
    let clear = target.scope().clear_request();

    assert_eq!(request.target, Some(CommandHistoryTarget::Telnet));
    assert_eq!(request.pane_id, Some(pane_id.clone()));
    assert_eq!(request.remote_host_id, Some(host_id.clone()));
    assert_eq!(request.session_id, None);
    assert_eq!(request.query.as_deref(), Some("  deploy "));
    assert_eq!(request.source, Some(CommandHistorySource::User));
    assert_eq!(request.limit, Some(100));
    assert_eq!(clear.target, Some(CommandHistoryTarget::Telnet));
    assert_eq!(clear.pane_id, Some(pane_id));
    assert_eq!(clear.remote_host_id, Some(host_id));
    assert_eq!(clear.session_id, None);
}

/// SSH 可以从已解析的强类型会话目标获得 HostId，而不会读取或解析 endpoint 文本。
///
/// @author kongweiguang
#[test]
fn ssh_scope_uses_typed_host_id_fallback() {
    let host_id = HostId::parse("ssh-host-logs-test").expect("valid host identity");
    let (binding, info, pane_id) = ui_session(SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: None,
    }));
    let target = LogsTarget::from_ui_session(binding, &info, None)
        .expect("matching UI binding creates a Logs target");

    assert_eq!(target.scope().target, Some(CommandHistoryTarget::Ssh));
    assert_eq!(target.scope().pane_id, Some(pane_id));
    assert_eq!(target.scope().remote_host_id, Some(host_id));
    assert_eq!(target.scope().session_id, None);
}

/// 片段管理器的历史副本来源跟随 pane，不能被 Logs 的主机/协议筛选缩窄而遗漏旧命令。
///
/// @author kongweiguang
#[test]
fn snippet_history_scope_keeps_only_pane_and_limit() {
    let host_id = HostId::parse("ssh-host-snippet-history").expect("valid host identity");
    let (binding, info, pane_id) = ui_session(SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: None,
        command: None,
    }));
    let target = LogsTarget::from_ui_session(binding, &info, None)
        .expect("matching binding creates a history target");
    let request = target.snippet_history_request(20);
    assert_eq!(request.pane_id, Some(pane_id));
    assert_eq!(request.limit, Some(20));
    assert_eq!(request.target, None);
    assert_eq!(request.remote_host_id, None);
    assert_eq!(request.session_id, None);
    assert_eq!(request.query, None);
    assert_eq!(request.source, None);
}

/// typed pane target 构造拒绝冲突的 SSH HostId，并要求 Telnet/Serial host 由同 pane 映射提供。
///
/// @author kongweiguang
#[test]
fn ui_target_constructor_validates_ssh_host_and_never_guesses_telnet_serial_host() {
    let ssh_host = HostId::parse("ssh-host-from-target").expect("valid host identity");
    let other_host = HostId::parse("different-selected-host").expect("valid host identity");
    let (ssh_binding, ssh_info, _) = ui_session(SessionTarget::Ssh(SshSessionTarget {
        host_id: ssh_host.clone(),
        working_directory: None,
        command: None,
    }));
    assert!(
        LogsTarget::from_ui_target(
            ssh_binding.clone(),
            &ssh_info.target,
            Some(other_host.clone()),
        )
        .is_none()
    );
    let ssh = LogsTarget::from_ui_target(ssh_binding, &ssh_info.target, Some(ssh_host.clone()))
        .expect("matching trusted SSH host is accepted");
    assert_eq!(ssh.scope().remote_host_id, Some(ssh_host));

    let (telnet_binding, telnet_info, pane_id) =
        ui_session(SessionTarget::Telnet(TelnetSessionTarget {
            host: "telnet.example.test".to_owned(),
            port: 23,
        }));
    let telnet = LogsTarget::from_ui_target(telnet_binding.clone(), &telnet_info.target, None)
        .expect("pane and protocol are sufficient when no saved host mapping exists");
    assert_eq!(telnet.scope().target, Some(CommandHistoryTarget::Telnet));
    assert_eq!(telnet.scope().pane_id, Some(pane_id.clone()));
    assert_eq!(telnet.scope().remote_host_id, None);
    assert_eq!(telnet.scope().session_id, None);

    let trusted_telnet_host = HostId::parse("telnet-saved-host").expect("valid host identity");
    let telnet = LogsTarget::from_ui_target(
        telnet_binding,
        &telnet_info.target,
        Some(trusted_telnet_host.clone()),
    )
    .expect("explicit same-pane saved host is accepted");
    assert_eq!(telnet.scope().remote_host_id, Some(trusted_telnet_host));

    let (local_binding, local_info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let local = LogsTarget::from_ui_target(local_binding, &local_info.target, Some(other_host))
        .expect("local pane is accepted without carrying unrelated host state");
    assert_eq!(local.scope().target, Some(CommandHistoryTarget::Local));
    assert_eq!(local.scope().remote_host_id, None);

    let (serial_binding, serial_info, serial_pane) = ui_session(SessionTarget::Serial(
        SerialSessionTarget::new(PathBuf::from("COM3")),
    ));
    let serial = LogsTarget::from_ui_target(serial_binding.clone(), &serial_info.target, None)
        .expect("serial can be scoped without inventing a host");
    assert_eq!(serial.scope().target, Some(CommandHistoryTarget::Serial));
    assert_eq!(serial.scope().pane_id, Some(serial_pane));
    assert_eq!(serial.scope().remote_host_id, None);
    assert_eq!(serial.scope().session_id, None);
    let trusted_serial_host = HostId::parse("serial-saved-host").expect("valid host identity");
    let serial = LogsTarget::from_ui_target(
        serial_binding,
        &serial_info.target,
        Some(trusted_serial_host.clone()),
    )
    .expect("explicit saved host is accepted for the same serial pane target");
    assert_eq!(serial.scope().remote_host_id, Some(trusted_serial_host));

    let agent_binding = UiSessionBinding {
        session_id: SessionId::parse("agent-session-target").expect("valid session identity"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::new(),
        },
        session_generation: 1,
    };
    assert!(LogsTarget::from_ui_target(agent_binding, &ssh_info.target, None,).is_none());
}

/// 串口无主机映射时不猜设备身份；本地和容器历史不继承传入的远端 host 筛选。
///
/// @author kongweiguang
#[test]
fn missing_or_inapplicable_host_mapping_does_not_guess_scope() {
    let (serial_binding, serial_info, serial_pane) = ui_session(SessionTarget::Serial(
        SerialSessionTarget::new(PathBuf::from("COM3")),
    ));
    let serial = LogsTarget::from_ui_session(serial_binding, &serial_info, None)
        .expect("matching serial binding creates a Logs target");
    assert_eq!(serial.scope().target, Some(CommandHistoryTarget::Serial));
    assert_eq!(serial.scope().pane_id, Some(serial_pane));
    assert_eq!(serial.scope().remote_host_id, None);

    let host_id = HostId::parse("unrelated-host").expect("valid host identity");
    let (local_binding, local_info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let local = LogsTarget::from_ui_session(local_binding, &local_info, Some(host_id.clone()))
        .expect("matching local binding creates a Logs target");
    assert_eq!(local.scope().target, Some(CommandHistoryTarget::Local));
    assert_eq!(local.scope().remote_host_id, None);

    let (container_binding, container_info, _) =
        ui_session(SessionTarget::Container(ContainerSessionTarget {
            host_id: Some(host_id.clone()),
            runtime: ContainerRuntime::Docker,
            container_id: "container-id".to_owned(),
            command: Vec::new(),
        }));
    let container = LogsTarget::from_ui_session(container_binding, &container_info, Some(host_id))
        .expect("matching container binding creates a Logs target");
    assert_eq!(
        container.scope().target,
        Some(CommandHistoryTarget::Container)
    );
    assert_eq!(container.scope().remote_host_id, None);
}

/// 后台 Agent、MCP 和 Service session 不作为用户 Logs pane，且 binding 必须精确匹配快照。
///
/// @author kongweiguang
#[test]
fn logs_target_rejects_mismatched_or_non_ui_binding() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let mut mismatched = binding.clone();
    mismatched.session_generation += 1;
    assert!(LogsTarget::from_ui_session(mismatched, &info, None).is_some());

    let agent_session_id = AgentSessionId::new();
    let owner = SessionOwner::Agent { agent_session_id };
    let agent_binding = UiSessionBinding {
        session_id: info.session_id.clone(),
        owner: owner.clone(),
        session_generation: 7,
    };
    let mut agent_info = info.clone();
    agent_info.owner = owner;
    assert!(LogsTarget::from_ui_session(agent_binding, &agent_info, None).is_none());

    let wrong_session = UiSessionBinding {
        session_id: SessionId::parse("another-session").expect("valid session identity"),
        owner: binding.owner,
        session_generation: binding.session_generation,
    };
    assert!(LogsTarget::from_ui_session(wrong_session, &info, None).is_none());
}

/// 命令工作区历史仍按显式 session 筛选；Logs pane scope 则保持 session 为空。
///
/// @author kongweiguang
#[test]
fn history_scopes_preserve_distinct_workspace_and_logs_contracts() {
    let session_id = SessionId::parse("workspace-session").expect("valid session identity");
    let workspace = HistoryScope::session(Some(session_id.clone()));
    let request = workspace.list_request(String::new(), None, 100);
    let clear = workspace.clear_request();
    assert_eq!(request.session_id, Some(session_id.clone()));
    assert_eq!(request.target, None);
    assert_eq!(clear.session_id, Some(session_id));
    assert_eq!(clear.target, None);

    let pane = PaneId::parse("logs-pane").expect("valid pane identity");
    let logs = HistoryScope::pane(CommandHistoryTarget::Local, pane, None);
    assert_eq!(logs.list_request(String::new(), None, 100).session_id, None);
    assert_eq!(logs.clear_request().session_id, None);
}

/// 初始八条容量在第一次布局前仍可分页；空列表及超界页码始终确定性夹紧。
///
/// @author kongweiguang
#[test]
fn page_window_clamps_empty_partial_and_out_of_range_pages() {
    assert_eq!(page_window(0, 0, PAGE_SIZE), (1, 0, 0));
    assert_eq!(page_window(9, 9, PAGE_SIZE), (2, 8, 9));
    assert_eq!(page_window(usize::MAX, 100, PAGE_SIZE), (13, 96, 100));
}

/// 空列表或只有一页时不展示无法使用的分页按钮；超过页容量后才需要分页交互。
///
/// @author kongweiguang
#[test]
fn pagination_is_hidden_until_more_than_one_page_exists() {
    assert!(!super::pagination_visible(0, PAGE_SIZE));
    assert!(!super::pagination_visible(PAGE_SIZE, PAGE_SIZE));
    assert!(super::pagination_visible(PAGE_SIZE + 1, PAGE_SIZE));
}

/// 分页容量始终与实际绘制的密度及 rem 保持一致，临界高度必须容纳完整末行。
///
/// @author kongweiguang
#[test]
fn logs_capacity_uses_card_height_and_reserves_pagination_only_when_needed() {
    use crate::theme::UiMetrics;
    use gpui_kit::px;
    use kerminal_application::preferences::InterfaceDensity;

    for (density, rem) in [
        (InterfaceDensity::Compact, 16.),
        (InterfaceDensity::Compact, 24.),
        (InterfaceDensity::Comfortable, 16.),
        (InterfaceDensity::Spacious, 20.),
    ] {
        let geometry = super::HistoryGeometry::from_metrics(UiMetrics::resolve(density, px(rem)));
        let row = geometry.row_height.as_f32();
        let header = geometry.header_height.as_f32();
        let pagination = geometry.pagination_height.as_f32();
        let eight_rows = 2. + header + row * 8.;
        assert_eq!(super::history_page_size(eight_rows, 8, geometry), 8);
        let paged_eight_rows = eight_rows + pagination;
        assert_eq!(super::history_page_size(paged_eight_rows, 24, geometry), 8);
        assert_eq!(
            super::history_page_size(paged_eight_rows - 0.5, 24, geometry),
            7
        );
        assert_eq!(
            super::history_page_size(paged_eight_rows + row * 8., 24, geometry),
            16
        );
        assert_eq!(super::history_page_size(0., 24, geometry), 1);
        assert_eq!(super::history_page_size(100_000., 100, geometry), 100);
    }
}

/// 动态分页在窗口缩放、删除和空结果时仍覆盖真实末页，不遗失或重复划分记录。
///
/// @author kongweiguang
#[test]
fn logs_dynamic_pages_clamp_and_partition_all_entries() {
    for size in [1, 7, 8, 16, 24, 100] {
        let mut covered = Vec::new();
        for page in 1..=24_usize.div_ceil(size) {
            let (_, start, end) = super::logs_page_window(page, 24, size);
            covered.extend(start..end);
        }
        assert_eq!(covered, (0..24).collect::<Vec<_>>());
    }
    assert_eq!(super::logs_page_window(3, 24, 16), (2, 16, 24));
    assert_eq!(super::logs_page_window(2, 16, 16), (1, 0, 16));
    assert_eq!(super::logs_page_window(0, 0, 16), (1, 0, 0));
}

/// 页内范围按 UI 的一基起始位置显示，空列表和越界范围都保持可读的零计数。
///
/// @author kongweiguang
#[test]
fn history_range_label_matches_recent_commands_header() {
    assert_eq!(super::history_range_label(0, 0, 0), "0 / 0");
    assert_eq!(super::history_range_label(0, 1, 1), "1-1 / 1");
    assert_eq!(super::history_range_label(8, 11, 11), "9-11 / 11");
    assert_eq!(super::history_range_label(8, 20, 11), "9-11 / 11");
}

/// 损坏或历史格式的时间不能被强制转成1970年；合法Unix秒按当前系统时区投影。
///
/// @author kongweiguang
#[test]
fn history_timestamp_preserves_unknown_values_and_uses_local_time() {
    for value in ["", "0", "-1", "invalid", "2026-10-28T01:00:00Z"] {
        assert_eq!(super::history_timestamp(value), value);
    }
    let seconds = 1_800_000_000;
    let expected = chrono::DateTime::from_timestamp(seconds, 0)
        .expect("valid fixture timestamp")
        .with_timezone(&chrono::Local)
        .format("%m/%d %H:%M")
        .to_string();
    assert_eq!(super::history_timestamp(&seconds.to_string()), expected);
}

/// 来源筛选菜单维持“全部 + 每种来源各一项”，防止收拢按钮时遗漏历史类型。
///
/// @author kongweiguang
#[test]
fn source_filter_options_cover_every_history_source_once() {
    let sources = super::source_options().map(|(source, _)| source);

    assert_eq!(
        sources,
        [
            None,
            Some(CommandHistorySource::User),
            Some(CommandHistorySource::Broadcast),
            Some(CommandHistorySource::Snippet),
            Some(CommandHistorySource::Workflow),
            Some(CommandHistorySource::Tool),
        ]
    );
}

/// cwd 优先显示为路径；缺失时明确展示真实主机/会话或终端目标上下文。
///
/// @author kongweiguang
#[test]
fn history_entry_context_prefers_cwd_then_labeled_fallbacks() {
    let host_id = HostId::parse("host-logs-context-test").expect("valid host identity");
    let session_id = SessionId::parse("session-logs-context-test").expect("valid session id");
    let host_display_names = HashMap::new();
    let mut entry = history_entry();
    entry.cwd = Some("C:\\projects\\kerminal".to_owned());
    entry.remote_host_id = Some(host_id.clone());
    entry.session_id = Some(session_id.clone());

    assert_eq!(
        super::history_entry_context(&entry, None, &host_display_names),
        "C:\\projects\\kerminal"
    );

    entry.cwd = Some("  ".to_owned());
    let host_context = super::history_entry_context(&entry, None, &host_display_names);
    assert!(host_context.contains("host-logs-context-test"));

    entry.cwd = None;
    entry.remote_host_id = None;
    let session_context = super::history_entry_context(&entry, None, &host_display_names);
    assert!(session_context.contains("session-logs-context-test"));

    entry.session_id = None;
    let (binding, session, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding, &session, None)
        .expect("matching UI binding creates a Logs target");
    assert_eq!(
        super::history_entry_context(&entry, Some(&target), &host_display_names),
        target.label().to_string()
    );
}

/// 可见上下文优先采用已加载的主机显示名，但可访问文本仍携带稳定 ID 以便精确核对目标。
///
/// @author kongweiguang
#[test]
fn history_context_uses_host_display_name_and_preserves_host_id_in_details() {
    let host_id = HostId::parse("ui-live-ssh-r24").expect("valid host identity");
    let mut entry = history_entry();
    entry.remote_host_id = Some(host_id.clone());
    let host_display_names = HashMap::from([(host_id.clone(), "Release host".into())]);

    let context = super::history_entry_context(&entry, None, &host_display_names);
    let detail = super::history_entry_context_detail(&entry, None, &host_display_names);

    assert!(context.contains("Release host"));
    assert!(!context.contains(host_id.as_str()));
    assert!(detail.contains("Release host"));
    assert!(detail.contains(host_id.as_str()));
}

/// 隐藏期间仍接受同一请求结果，筛选、代次或目标变化则阻止旧回执进入当前 Logs Entity。
///
/// @author kongweiguang
#[test]
fn late_responses_follow_request_identity_after_deactivation() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding.clone(), &info, None)
        .expect("matching UI binding creates a Logs target");
    let request = HistoryReadKey {
        target: target.clone(),
        query: "git status".to_owned(),
        source: Some(CommandHistorySource::User),
    };

    // Deactivation preserves the current request identity, so its bounded read may still settle.
    assert!(response_matches_request(5, 5, Some(&request), &request));
    assert!(!response_matches_request(6, 5, Some(&request), &request));
    assert!(!response_matches_request(5, 5, None, &request));

    let changed_query = HistoryReadKey {
        query: "git log".to_owned(),
        ..request.clone()
    };
    assert!(!response_matches_request(
        5,
        5,
        Some(&changed_query),
        &request
    ));
    let changed_source = HistoryReadKey {
        source: None,
        ..request.clone()
    };
    assert!(!response_matches_request(
        5,
        5,
        Some(&changed_source),
        &request
    ));

    let mut new_binding = binding;
    new_binding.session_generation += 1;
    let new_target = LogsTarget::from_ui_session(new_binding, &info, None)
        .expect("session generation does not invalidate the matching snapshot itself");
    let new_request = HistoryReadKey {
        target: new_target.clone(),
        ..request.clone()
    };
    assert!(!response_matches_request(
        5,
        5,
        Some(&new_request),
        &request
    ));
    assert!(operation_matches_target(true, Some(&target), &target));
    assert!(!operation_matches_target(false, Some(&target), &target));
    assert!(!operation_matches_target(true, Some(&new_target), &target));
}

/// 行内发送必须与应用层单行 Paste 约束一致，不能让历史换行隐式执行终端命令。
///
/// @author kongweiguang
#[test]
fn history_fill_allows_single_line_and_rejects_internal_newlines() {
    assert!(history_fill_command_is_valid(" git status "));
    assert!(history_fill_command_is_valid("\n\tgit\tstatus\r\n"));
    assert!(!history_fill_command_is_valid(" \r\n\t "));
    assert!(!history_fill_command_is_valid("echo first\necho second"));
    assert!(!history_fill_command_is_valid("echo first\recho second"));
}

/// 历史填入完成后只有原 pane、原代次及已确认回执能触发焦点返回。
///
/// @author kongweiguang
#[test]
fn history_fill_focus_ticket_rejects_stale_or_unconfirmed_target() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding.clone(), &info, None)
        .expect("matching UI binding creates a Logs target");
    let event = LogsTerminalFillCompleted {
        binding: binding.clone(),
        expected_focus: None,
        generation: 11,
    };

    assert!(history_fill_completion_matches(
        true,
        11,
        Some(&target),
        false,
        false,
        &event
    ));
    assert!(!history_fill_completion_matches(
        false,
        11,
        Some(&target),
        false,
        false,
        &event
    ));
    assert!(!history_fill_completion_matches(
        true,
        12,
        Some(&target),
        false,
        false,
        &event
    ));
    assert!(!history_fill_completion_matches(
        true,
        11,
        Some(&target),
        true,
        false,
        &event
    ));
    assert!(!history_fill_completion_matches(
        true,
        11,
        Some(&target),
        false,
        true,
        &event
    ));
    assert!(!history_fill_completion_matches(
        true, 11, None, false, false, &event
    ));

    let mut replacement_binding = binding;
    replacement_binding.session_generation += 1;
    let replacement_target = LogsTarget::from_ui_session(replacement_binding, &info, None)
        .expect("changed generation remains a valid new target");
    assert!(!history_fill_completion_matches(
        true,
        11,
        Some(&replacement_target),
        false,
        false,
        &event
    ));
}

/// 新列表交互只移除已确认反馈；错误和需要用户核对的未知结果不能被刷新或筛选吞掉。
///
/// @author kongweiguang
#[test]
fn history_list_interaction_keeps_unresolved_fill_feedback() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding, &info, None)
        .expect("matching UI binding creates a Logs target");
    let mut feedback = HistoryFillFeedback::default();
    let generation = feedback.begin_interaction();
    assert!(feedback.confirm(generation, "filled".to_owned()));

    feedback.begin_interaction();
    assert_eq!(feedback.status, None);

    feedback.fail("write failed".to_owned());
    feedback.begin_interaction();
    assert_eq!(
        feedback.status,
        Some(HistoryFillStatus::Problem("write failed".to_owned()))
    );

    feedback.outcome_unknown(target.clone());
    feedback.begin_interaction();
    assert_eq!(feedback.unknown_target, Some(target));
    assert_eq!(feedback.status, None);
}

/// 用户在回执到达前操作列表时，旧成功不得复现或抢焦点，但迟到的失败和未知仍须可见。
///
/// @author kongweiguang
#[test]
fn history_list_interaction_rejects_stale_success_and_keeps_late_failures() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding.clone(), &info, None)
        .expect("matching UI binding creates a Logs target");
    let mut feedback = HistoryFillFeedback::default();
    let pending = feedback.begin_interaction();
    let event = LogsTerminalFillCompleted {
        binding,
        expected_focus: None,
        generation: pending,
    };
    feedback.begin_interaction();

    assert!(!feedback.confirm(pending, "filled".to_owned()));
    assert_eq!(feedback.status, None);
    assert!(!history_fill_completion_matches(
        true,
        feedback.generation,
        Some(&target),
        false,
        false,
        &event
    ));
    feedback.fail("late failure".to_owned());
    assert_eq!(
        feedback.status,
        Some(HistoryFillStatus::Problem("late failure".to_owned()))
    );
    feedback.outcome_unknown(target.clone());
    assert_eq!(feedback.unknown_target, Some(target));
}
