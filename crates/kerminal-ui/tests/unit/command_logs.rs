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
use super::{LogsTarget, operation_matches_target, response_matches_target};

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

/// 八条一页且总查询最多一百条，页码输入在空列表和边界外均确定性夹紧。
///
/// @author kongweiguang
#[test]
fn page_window_clamps_empty_partial_and_out_of_range_pages() {
    assert_eq!(page_window(0, 0), (1, 0, 0));
    assert_eq!(page_window(9, 9), (2, 8, 9));
    assert_eq!(page_window(usize::MAX, 100), (13, 96, 100));
}

/// 空列表或只有一页时不展示无法使用的分页按钮；超过页容量后才需要分页交互。
///
/// @author kongweiguang
#[test]
fn pagination_is_hidden_until_more_than_one_page_exists() {
    assert!(!super::pagination_visible(0));
    assert!(!super::pagination_visible(PAGE_SIZE));
    assert!(super::pagination_visible(PAGE_SIZE + 1));
}

/// 空白搜索与默认来源应视作无筛选，与 HistoryScope 构造查询时的搜索归一化一致。
///
/// @author kongweiguang
#[test]
fn history_filter_activity_matches_request_query_normalization() {
    assert!(!super::history_filters_active("", None));
    assert!(!super::history_filters_active("   ", None));
    assert!(super::history_filters_active("deploy", None));
    assert!(super::history_filters_active(
        "",
        Some(CommandHistorySource::User)
    ));
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

/// 异步结果只有活动状态、generation 与完整 binding 同时一致时才能进入当前 Logs Entity。
///
/// @author kongweiguang
#[test]
fn late_responses_are_rejected_after_deactivation_generation_or_target_changes() {
    let (binding, info, _) = ui_session(SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("sh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::new(),
    }));
    let target = LogsTarget::from_ui_session(binding.clone(), &info, None)
        .expect("matching UI binding creates a Logs target");

    assert!(response_matches_target(true, 5, 5, Some(&target), &target));
    assert!(!response_matches_target(
        false,
        5,
        5,
        Some(&target),
        &target
    ));
    assert!(!response_matches_target(true, 6, 5, Some(&target), &target));
    assert!(!response_matches_target(true, 5, 5, None, &target));

    let mut new_binding = binding;
    new_binding.session_generation += 1;
    let new_target = LogsTarget::from_ui_session(new_binding, &info, None)
        .expect("session generation does not invalidate the matching snapshot itself");
    assert!(!response_matches_target(
        true,
        5,
        5,
        Some(&new_target),
        &target
    ));
    assert!(operation_matches_target(true, Some(&target), &target));
    assert!(!operation_matches_target(false, Some(&target), &target));
    assert!(!operation_matches_target(true, Some(&new_target), &target));
}
