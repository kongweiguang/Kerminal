//! Logs 的历史范围与迟到响应回归测试。
//!
//! @author kongweiguang

// 真实视图接线回归复用隔离命令服务，避免测试读取用户配置或历史数据库。
// 共享 fixture 在 crate 根只声明一次，这里用 use 引入，避免重复载入同一文件。
use crate::root_commands_service;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gpui_kit::{AppContext as _, ElementId, Entity, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandsService,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionFilter, SessionRuntimeMetadata,
        SessionService, SessionSubscription, ShellCommandOutcome, ShellCommandRequest,
        TerminalCommandContext, TerminalInputEvent, TerminalPalette, TerminalSearchRequest,
        TerminalSearchResult, TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
};
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget,
    },
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    sessions::{
        ContainerRuntime, ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget,
        SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget,
        TelnetSessionTarget, TerminalSize,
    },
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

use super::super::history_query::{HistoryScope, PAGE_SIZE, page_window};
use super::{
    HistoryFillFeedback, HistoryFillStatus, HistoryReadKey, LogsTarget, LogsTerminalFillCompleted,
    LogsView, history_count_label, history_fill_command_is_valid, history_fill_completion_matches,
    operation_matches_target, response_matches_request,
};

impl LogsView {
    /// 真实 LogsView 回归需要确定性的“系统允许动效”前提，避免测试依赖当前桌面设置。
    /// 注入方法放在外置测试模块，src 只保留常驻字段与 `cfg!(test)` 读取分支。
    ///
    /// @author kongweiguang
    pub(crate) fn set_motion_allowed_for_test(&mut self, allowed: bool) {
        self.motion_allowed_for_test = Some(allowed);
    }
}

/// 构造绑定一致的 UI 会话样本，确保测试使用与 Root 相同的稳定身份字段。
///
/// @author kongweiguang
fn ui_session(target: SessionTarget) -> (UiSessionBinding, SessionInfo, PaneId) {
    ui_session_identity(target, "session-logs-test", "pane-logs-test")
}

/// 允许同一 Tab 下构造不同的 session/pane，覆盖真实右栏目标切换而不改共享默认样本。
///
/// @author kongweiguang
fn ui_session_identity(
    target: SessionTarget,
    session: &str,
    pane: &str,
) -> (UiSessionBinding, SessionInfo, PaneId) {
    let session_id = SessionId::parse(session).expect("valid session identity");
    let pane_id = PaneId::parse(pane).expect("valid pane identity");
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

/// 只满足 LogsView 组合所需的空会话端口；任何真实终端操作都显式失败，避免 fixture 假装能力。
///
/// @author kongweiguang
struct EmptySessionService;

#[async_trait]
impl SessionService for EmptySessionService {
    /// 测试不创建真实会话，显式拒绝避免隐藏 fake 能力。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        Err(AppError::unsupported("test session create"))
    }

    /// 取消路径与 create 同样拒绝，保持 fake 的边界可见。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test session create cancelled"));
        }
        Err(AppError::unsupported("test session create"))
    }

    /// 快照只读取空列表，确保运行态数据来自实际 application service。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        Ok(Vec::new())
    }

    /// 测试 fake 没有真实 transport 元数据。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        Err(AppError::unsupported("test runtime metadata"))
    }

    /// 测试 fake 不模拟输入字节流。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        Err(AppError::unsupported("test terminal write"))
    }

    /// 测试 fake 不模拟 GPUI 键盘编码。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test terminal input event"))
    }

    /// 测试 fake 不实现可见 UI 的命令行提交。
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported("test UI command line"))
    }

    /// 测试 fake 不调整真实网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        Err(AppError::unsupported("test terminal resize"))
    }

    /// 测试 fake 不维护主题 palette。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test palette"))
    }

    /// 测试 fake 不裁剪 scrollback。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        Err(AppError::unsupported("test scrollback limit"))
    }

    /// 测试 fake 不生成 GPUI 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        Err(AppError::unsupported("test render snapshot"))
    }

    /// 测试 fake 不生成纯文本快照。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        Err(AppError::unsupported("test plaintext snapshot"))
    }

    /// 测试 fake 不维护滚动视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        Err(AppError::unsupported("test scroll"))
    }

    /// 测试 fake 不维护 UI 绑定滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test UI scroll"))
    }

    /// 测试 fake 不维护网格选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test selection"))
    }

    /// 测试 fake 没有选中文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        Ok(None)
    }

    /// 测试 fake 不实现 scrollback 搜索。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        Err(AppError::unsupported("test search"))
    }

    /// 测试 fake 没有可重连目标。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test reconnect cancelled"));
        }
        Err(AppError::unsupported("test reconnect"))
    }

    /// 测试 fake 没有可关闭会话。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        Err(AppError::not_found("session not found"))
    }

    /// 测试 fake 立即完成关闭，不维护独立 deadline。
    async fn close_until(
        &self,
        session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        self.close(session_id).await
    }

    /// 测试 fake 没有可清理的会话。
    async fn close_all(&self) -> AppResult<()> {
        Ok(())
    }

    /// 测试 fake 不广播 UI 更新。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        Err(AppError::unsupported("test subscribe"))
    }

    /// 测试 fake 不执行交互 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test shell command cancelled"));
        }
        Err(AppError::unsupported("test shell command"))
    }
}

/// 按调用序号返回脚本结果，并可在指定序号挂起，便于检查目标/筛选变化后的真实等待窗口。
/// 未列出的调用默认返回空列表，避免测试读取真实数据库或无限等待。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
enum ScriptedReply {
    Empty,
    EntryA,
    EntriesA,
    EntriesB,
    Error,
}

struct ScriptedHistory {
    calls: Arc<AtomicUsize>,
    gates: BTreeMap<usize, Arc<Semaphore>>,
    replies: Vec<ScriptedReply>,
}

#[async_trait]
impl CommandHistoryRepository for ScriptedHistory {
    /// 历史服务可用；测试只控制 list，不触碰真实存储。
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 先计数再等待门控，让测试能区分“请求已发起”和“结果已接纳”；
    /// 失败与空结果都走真实响应路径，不用同步布尔函数模拟。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        let call = self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(gate) = self.gates.get(&call) {
            let _permit = gate.acquire().await.expect("scripted history gate");
        }
        match self
            .replies
            .get(call)
            .copied()
            .unwrap_or(ScriptedReply::Empty)
        {
            ScriptedReply::EntryA => {
                let mut entry = history_entry();
                entry.id = "history-entry-a".to_owned();
                entry.command = "a-command".to_owned();
                Ok(vec![entry])
            }
            ScriptedReply::EntriesA => {
                let mut first = history_entry();
                first.id = "history-entry-a1".to_owned();
                first.command = "a-command-1".to_owned();
                let mut second = history_entry();
                second.id = "history-entry-a2".to_owned();
                second.command = "a-command-2".to_owned();
                Ok(vec![first, second])
            }
            ScriptedReply::EntriesB => {
                let mut first = history_entry();
                first.id = "history-entry-b1".to_owned();
                first.command = "b-command-1".to_owned();
                let mut second = history_entry();
                second.id = "history-entry-b2".to_owned();
                second.command = "b-command-2".to_owned();
                Ok(vec![first, second])
            }
            ScriptedReply::Empty => Ok(Vec::new()),
            ScriptedReply::Error => Err(AppError::unsupported("scripted history failure")),
        }
    }

    /// 脚本只覆盖读取，任何历史写入都必须显式失败。
    async fn append(&self, _draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        Err(AppError::unsupported("scripted history append"))
    }

    /// 脚本只覆盖读取，删除必须显式失败。
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        Err(AppError::unsupported("scripted history delete"))
    }

    /// 脚本只覆盖读取，清理必须显式失败。
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        Err(AppError::unsupported("scripted history clear"))
    }
}

/// 同时脚本化读取、删除与清理结果，用于验证写操作成功后的权威回读是否保留确认提示。
/// 删除/清理调用计数独立记录，测试据此确认写操作真实发生，而不是只看到刷新；
/// 读取可按调用序号门控或失败，从而分别观察回读等待、成功和失败三种边界。
///
/// @author kongweiguang
struct ScriptedMutationHistory {
    list_calls: Arc<AtomicUsize>,
    delete_calls: Arc<AtomicUsize>,
    clear_calls: Arc<AtomicUsize>,
    gate_list_call: Option<usize>,
    gate: Option<Arc<Semaphore>>,
    fail_list_call: Option<usize>,
    /// 删除回执门控，用于制造“用户先换筛选、删除回执后到”的迟到场景。
    delete_gate: Option<Arc<Semaphore>>,
    entries: Vec<CommandHistoryEntry>,
    delete_result: AppResult<bool>,
    clear_result: AppResult<u64>,
}

#[async_trait]
impl CommandHistoryRepository for ScriptedMutationHistory {
    /// 测试历史始终可用，避免把装配失败误判为业务失败。
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 首次读取返回脚本条目；指定调用序号可停在门控或返回错误，
    /// 让写操作确认在“回读等待”“回读成功”“回读失败”下都被真实观察到。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        let call = self.list_calls.fetch_add(1, Ordering::AcqRel);
        if self.gate_list_call == Some(call)
            && let Some(gate) = self.gate.as_ref()
        {
            let _permit = gate.acquire().await.expect("mutation history gate");
        }
        if self.fail_list_call == Some(call) {
            return Err(AppError::unsupported("scripted mutation history failure"));
        }
        Ok(self.entries.clone())
    }

    /// 测试不追加历史，显式失败以免伪造成功。
    async fn append(&self, _draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        Err(AppError::unsupported("scripted mutation append"))
    }

    /// 返回预设删除结果；调用计数证明写操作真实发生。
    /// 可选门控用于让回执晚于用户的新筛选，验证旧确认不会写回新交互。
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        self.delete_calls.fetch_add(1, Ordering::AcqRel);
        if let Some(gate) = self.delete_gate.as_ref() {
            let _permit = gate.acquire().await.expect("mutation delete gate");
        }
        match &self.delete_result {
            Ok(value) => Ok(*value),
            Err(error) => Err(error.clone()),
        }
    }

    /// 返回预设清理结果；调用计数证明写操作真实发生。
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        self.clear_calls.fetch_add(1, Ordering::AcqRel);
        match &self.clear_result {
            Ok(value) => Ok(*value),
            Err(error) => Err(error.clone()),
        }
    }
}

/// 有界推进 GPUI 与 Tokio，直到真实可观察条件成立；超时明确失败，不依赖固定调度次数。
///
/// @author kongweiguang
fn wait_until(
    runtime: &Runtime,
    visual: &mut VisualTestContext,
    description: &str,
    mut condition: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        if condition(visual) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::yield_now();
    }
}

/// 绘制两帧，确保 debug selector 读到最新加载/空态/行内容。
///
/// @author kongweiguang
fn draw_logs(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
}

/// 读取实际绘制后的无障碍计数名称；未知状态必须与视觉占位分开表达。
///
/// @author kongweiguang
fn rendered_count_label(visual: &mut VisualTestContext) -> Option<String> {
    let id: ElementId = "logs-history-count".into();
    visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id)
            .and_then(|snapshot| snapshot.label().map(str::to_owned))
    })
}

/// 单独核对计数节点暴露的真实角色与值：`test_support` 在角色为 `None` 时仍会记录
/// `aria_label`，只看 label 的断言无法发现“有名称但没有 AccessKit 节点”的假绿。
/// AccessKit 的 Label 角色把文本内容放在 value，Windows 可访问名称只读 value，
/// 因此角色和值必须同时存在。这里只能证明元素声明了可进入无障碍树的静态文本语义，
/// 真实 AccessKit 树需要系统无障碍客户端激活；headless 测试平台没有适配器，
/// 不能据此宣称读屏验收通过。
///
/// @author kongweiguang
fn rendered_count_a11y(visual: &mut VisualTestContext) -> (Option<gpui_kit::Role>, Option<String>) {
    let id: ElementId = "logs-history-count".into();
    visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id)
            .map(|snapshot| (snapshot.role(), snapshot.value().map(str::to_owned)))
            .unwrap_or_default()
    })
}

/// 复用产品本地化入口构造期望文案，避免测试把当前界面语言硬编码成中文或英文。
///
/// @author kongweiguang
fn unknown_count_aria_label() -> String {
    crate::locale::text(
        "命令历史数量尚未确认",
        "Command history count not yet confirmed",
    )
    .to_string()
}

/// 由同一 Tab 下的不同 pane/session 构造目标，确保 A/B 是真实可区分的历史 scope。
///
/// @author kongweiguang
fn logs_target(target: SessionTarget, session: &str, pane: &str) -> LogsTarget {
    let (binding, info, _) = ui_session_identity(target, session, pane);
    LogsTarget::from_ui_session(binding, &info, None).expect("matching UI binding creates a target")
}

/// 挂载真实 LogsView 并返回实体句柄与窗口上下文，避免每个写操作回归重复窗口样板。
/// 借用生命周期跟随 `app`，调用方仍持有唯一的可视上下文。
///
/// @author kongweiguang
fn mount_logs_view(
    app: &mut TestAppContext,
    commands: Arc<CommandsService>,
    executor: ApplicationTaskExecutor,
) -> (Entity<LogsView>, &mut VisualTestContext) {
    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands, executor, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");
    (view, visual)
}

/// 构造属于指定 pane 的两条历史行；删除前的 scope 复核要求 pane 与目标一致，
/// 因此不能复用没有 pane 的默认样本。
///
/// @author kongweiguang
fn mutation_entries(pane: &PaneId) -> Vec<CommandHistoryEntry> {
    ["a1", "a2"]
        .into_iter()
        .map(|suffix| {
            let mut entry = history_entry();
            entry.id = format!("history-entry-{suffix}");
            entry.command = format!("mutation-command-{suffix}");
            entry.pane_id = Some(pane.clone());
            entry
        })
        .collect()
}

/// 本地目标样本：写操作回归只需要一个真实可区分的 pane scope。
///
/// @author kongweiguang
fn local_logs_target(session: &str, pane: &str) -> LogsTarget {
    logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        session,
        pane,
    )
}

/// 隐藏期间完成的首个目标结果也必须消费入场，否则重显后的同目标刷新会再次压暗可见数据。
/// 这里走真实 set_target/set_active 与异步历史读取，不复用纯状态机的直接调用。
///
/// @author kongweiguang
#[gpui_kit::test]
fn hidden_first_result_consumes_entrance_before_same_target_refresh(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let (commands, calls) =
        root_commands_service::empty_commands_service_with_calls(Arc::new(EmptySessionService));
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target_a = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-a",
        "pane-logs-a",
    );
    let target_b = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-b.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-b",
        "pane-logs-b",
    );

    // 先让 A 真正成功并建立“初始挂载”基线；它的首个结果不播放入场。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_motion_allowed_for_test(true);
            view.set_active(true, cx);
            view.set_target(Some(target_a), cx);
        });
    });
    wait_until(&runtime, visual, "initial A successful read", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && calls.history_lists.load(Ordering::Acquire) >= 1
        })
    });
    let a_calls = calls.history_lists.load(Ordering::Acquire);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.loading, "initial A read should complete");
        assert!(
            logs.error.is_none(),
            "initial A read should succeed: {:?}",
            logs.error
        );
        assert!(
            !logs.data_entrance.awaiting_result(),
            "initial mount must not arm a target entrance"
        );
    });

    // B 的目标变化必须先登记等待并发起查询，随后才在隐藏期间成功。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_b), cx));
    });
    visual.update(|_, app| {
        assert!(
            view.read(app).data_entrance.awaiting_result(),
            "B target change must arm the entrance before data is ready"
        );
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_active(false, cx));
    });
    wait_until(&runtime, visual, "hidden B successful read", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && calls.history_lists.load(Ordering::Acquire) > a_calls
        })
    });
    let hidden_calls = calls.history_lists.load(Ordering::Acquire);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.loading, "hidden B read should complete");
        assert!(
            logs.error.is_none(),
            "hidden B read should succeed: {:?}",
            logs.error
        );
        assert!(
            !logs.data_entrance.awaiting_result(),
            "a current hidden success must consume the target entrance"
        );
        assert_eq!(
            logs.started_entrances_for_test, 0,
            "a hidden success must consume without starting a frame-driven entrance"
        );
        assert!(!logs.data_entrance.is_running(Instant::now()));
    });

    // 重显后的同目标刷新必须真实成功完成，且不得重放隐藏期已消费的入场。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_active(true, cx));
    });
    wait_until(
        &runtime,
        visual,
        "re-show same-target successful refresh",
        |visual| {
            visual.update(|_, app| {
                let logs = view.read(app);
                !logs.loading && calls.history_lists.load(Ordering::Acquire) > hidden_calls
            })
        },
    );
    visual.update(|_, app| {
        let logs = view.read(app);
        let now = Instant::now();
        assert!(!logs.loading, "re-show refresh should complete");
        assert!(
            logs.error.is_none(),
            "re-show refresh should succeed: {:?}",
            logs.error
        );
        assert!(!logs.data_entrance.awaiting_result());
        assert!(
            !logs.data_entrance.is_running(now),
            "same-target refresh after a hidden success must not replay the entrance"
        );
        assert_eq!(logs.data_entrance.opacity(now), 1.0);
        assert_eq!(
            logs.started_entrances_for_test, 0,
            "a re-show refresh must not start a suppressed hidden entrance"
        );
    });
}

/// 目标切换等待期保留列表容器但清空旧行；同筛选刷新保留可信空态与计数，
/// 新筛选和新目标等待期使用中性计数且不显示空态；成功后才显示可信 `0 / 0`。
///
/// @author kongweiguang
#[gpui_kit::test]
fn target_change_wait_keeps_list_container_and_trusted_empty_count(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let b_gate = Arc::new(Semaphore::new(0));
    let refresh_gate = Arc::new(Semaphore::new(0));
    let filter_gate = Arc::new(Semaphore::new(0));
    let target_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([
            (1, b_gate.clone()),
            (2, refresh_gate.clone()),
            (3, filter_gate.clone()),
            (4, target_gate.clone()),
        ]),
        replies: vec![ScriptedReply::EntryA],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target_a = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-a",
        "pane-logs-a",
    );
    let target_b = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-b.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-b",
        "pane-logs-b",
    );

    // A 先返回一条可识别记录，证明旧行真实进入列表而不是空夹具误通过。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target_a.clone()), cx);
        });
    });
    wait_until(&runtime, visual, "initial A record read", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.entries.len(), 1);
    });
    assert!(
        visual.debug_bounds("rr-logs-row-history-entry-a").is_some(),
        "A command row should be rendered"
    );
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some("1-1 / 1"),
        "trusted A snapshot should render its range"
    );
    // 名称与角色必须同时存在：只有 label 而没有角色时 GPUI 不会创建 AccessKit 节点，
    // 该计数节点不会出现在无障碍树中，也就无法由此读出计数。
    // 该断言在移除 `.role(Role::Label)` 后立即变红。
    assert_eq!(
        rendered_count_a11y(visual),
        (Some(gpui_kit::Role::Label), Some("1-1 / 1".to_owned())),
        "the trusted count must declare a static text role and expose its range as the Label value"
    );
    assert!(visual.debug_bounds("rr-logs-empty").is_none());
    let list_bounds = visual
        .debug_bounds("rr-logs-list")
        .expect("A history list should be rendered");

    // B 读取停在门控上：旧 A 行立即消失，但列表容器保留，按钮继续忙态，计数用中性占位。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_b), cx));
    });
    wait_until(&runtime, visual, "B read started", |_| {
        history_calls.load(Ordering::Acquire) >= 2
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(!logs.history_snapshot_is_current());
        assert!(logs.entries.is_empty());
        assert!(logs.error.is_none());
        assert!(logs.data_entrance.awaiting_result());
    });
    assert!(
        visual.debug_bounds("rr-logs-list").is_some(),
        "list container should remain during loading"
    );
    assert_eq!(
        visual.debug_bounds("rr-logs-list"),
        Some(list_bounds),
        "loading should not move or resize the list container"
    );
    assert!(
        visual.debug_bounds("rr-logs-row-history-entry-a").is_none(),
        "old A row must not survive the target change"
    );
    assert!(
        visual.debug_bounds("rr-logs-empty").is_none(),
        "loading must not show the empty-history state"
    );
    // 读屏名称解释“尚未确认”，视觉子文本仍保持中性占位；两者不能共用同一字符串。
    assert_eq!(history_count_label(false, 0, 0, 0), "— / —");
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str()),
        "unknown count should expose a localized accessibility name instead of a dash placeholder"
    );
    // 未知占位同样需要真实角色和 Label value，否则“数量尚未确认”这句读屏名称
    // 既不会进入树，也不会被 Windows 作为可访问名称读取。
    assert_eq!(
        rendered_count_a11y(visual),
        (
            Some(gpui_kit::Role::Label),
            Some(unknown_count_aria_label())
        ),
        "the unknown-count placeholder must expose a static text role and its localized value"
    );
    assert!(
        visual.debug_bounds("rr-logs-refresh-loading").is_some(),
        "refresh button should expose its loading state"
    );
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_none());

    // B 成功返回空列表后，未知数量才变成可信的 0 / 0，并恢复既有空态。
    b_gate.add_permits(1);
    wait_until(&runtime, visual, "B empty success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert!(logs.entries.is_empty());
        assert!(!logs.data_entrance.awaiting_result());
    });
    assert!(visual.debug_bounds("rr-logs-row-history-entry-a").is_none());
    assert!(
        visual.debug_bounds("rr-logs-empty").is_some(),
        "successful empty result should show the existing empty state"
    );
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some("0 / 0"),
        "successful empty result should show a real 0 / 0"
    );
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_some());

    // 同目标刷新在已有可信空快照时保留 0 / 0，不因重新加载退回未知占位。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.reload_history(cx));
    });
    wait_until(&runtime, visual, "same-target refresh started", |_| {
        history_calls.load(Ordering::Acquire) >= 3
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(logs.history_snapshot_is_current());
        assert!(logs.entries.is_empty());
    });
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some("0 / 0"),
        "trusted empty count should survive same-target refresh"
    );
    assert!(
        visual.debug_bounds("rr-logs-empty").is_some(),
        "same-filter refresh keeps the already trusted empty state"
    );
    assert!(visual.debug_bounds("rr-logs-refresh-loading").is_some());

    refresh_gate.add_permits(1);
    wait_until(&runtime, visual, "same-target refresh success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 3
        })
    });
    draw_logs(visual);
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_some());

    // 新筛选在途：旧空快照不属于当前 query，计数与空态都不能沿用。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("filtered".to_owned(), cx));
    });
    wait_until(&runtime, visual, "filter change started", |_| {
        history_calls.load(Ordering::Acquire) >= 4
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(logs.error.is_none());
        assert!(!logs.history_snapshot_is_current());
        assert!(logs.entries.is_empty());
    });
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str()),
        "a new query must not reuse the previous empty snapshot"
    );
    assert!(
        visual.debug_bounds("rr-logs-empty").is_none(),
        "a new query in flight must not claim a confirmed empty result"
    );
    assert!(visual.debug_bounds("rr-logs-refresh-loading").is_some());

    filter_gate.add_permits(1);
    wait_until(&runtime, visual, "filter change success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 4
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        assert!(view.read(app).history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());

    // 切回另一 target 仍保留查询：新 target 等待期必须清掉旧筛选的确认状态。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_a.clone()), cx));
    });
    wait_until(&runtime, visual, "target switch after filter", |_| {
        history_calls.load(Ordering::Acquire) >= 5
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(!logs.history_snapshot_is_current());
        assert!(logs.entries.is_empty());
    });
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-empty").is_none());
    assert!(visual.debug_bounds("rr-logs-refresh-loading").is_some());

    target_gate.add_permits(1);
    wait_until(&runtime, visual, "target switch success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 5
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        assert!(view.read(app).history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_some());
}

/// 新筛选失败后不得借用旧空快照；重试等待仍保持中性，成功后才恢复空态。
/// 隐藏期间成功的同筛选结果仍更新可信 key，重显刷新等待继续保留空态与计数。
///
/// @author kongweiguang
#[gpui_kit::test]
fn filter_failure_retry_and_hidden_result_keep_only_current_trusted_empty_state(
    app: &mut TestAppContext,
) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let retry_gate = Arc::new(Semaphore::new(0));
    let hidden_gate = Arc::new(Semaphore::new(0));
    let reshow_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([
            (2, retry_gate.clone()),
            (3, hidden_gate.clone()),
            (4, reshow_gate.clone()),
        ]),
        replies: vec![
            ScriptedReply::Empty,
            ScriptedReply::Error,
            ScriptedReply::Empty,
            ScriptedReply::Empty,
            ScriptedReply::Empty,
        ],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-failure",
        "pane-logs-failure",
    );

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "initial empty success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        assert!(view.read(app).history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());

    // 新筛选失败：旧的空快照不属于当前 query，失败态显示时不能同时保留空态或旧计数。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("failure".to_owned(), cx));
    });
    wait_until(&runtime, visual, "filter failure", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && logs.error.is_some() && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_some());
        assert!(!logs.history_snapshot_is_current());
    });
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-empty").is_none());
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_some());

    // 重试同一筛选：等待期仍是未知状态，成功后才建立当前可信空快照。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.reload_history(cx));
    });
    wait_until(&runtime, visual, "filter retry started", |_| {
        history_calls.load(Ordering::Acquire) >= 3
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(logs.error.is_none());
        assert!(!logs.history_snapshot_is_current());
    });
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-empty").is_none());
    assert!(visual.debug_bounds("rr-logs-refresh-loading").is_some());

    retry_gate.add_permits(1);
    wait_until(&runtime, visual, "filter retry success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 3
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        assert!(view.read(app).history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());

    // 隐藏期间的同筛选成功也必须更新可信 key，否则重显后的刷新会退回未知占位。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("hidden".to_owned(), cx));
    });
    wait_until(&runtime, visual, "hidden filter read started", |_| {
        history_calls.load(Ordering::Acquire) >= 4
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_active(false, cx));
    });
    hidden_gate.add_permits(1);
    wait_until(&runtime, visual, "hidden result accepted", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 4
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.active);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert!(!logs.data_entrance.awaiting_result());
    });

    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_active(true, cx));
    });
    wait_until(&runtime, visual, "reshow refresh started", |_| {
        history_calls.load(Ordering::Acquire) >= 5
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.active);
        assert!(logs.loading);
        assert!(logs.history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(
        visual.debug_bounds("rr-logs-empty").is_some(),
        "reshow refresh with the same trusted filter keeps the empty state"
    );
    assert!(visual.debug_bounds("rr-logs-refresh-loading").is_some());

    reshow_gate.add_permits(1);
    wait_until(&runtime, visual, "reshow refresh success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 5
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        assert!(view.read(app).history_snapshot_is_current());
    });
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());
    assert!(visual.debug_bounds("rr-logs-refresh-idle").is_some());
}

/// 未加载/未请求不等于成功空结果：inactive 时换目标或换筛选后，空态和计数都必须保持未知。
/// 只有实际成功的当前筛选快照能证明空历史；该测试不经过任何 loading 窗口，锁住该可达分支。
///
/// @author kongweiguang
#[gpui_kit::test]
fn inactive_unknown_target_or_filter_never_shows_confirmed_empty_state(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::new(),
        replies: vec![ScriptedReply::Empty],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-inactive",
        "pane-logs-inactive",
    );

    // inactive 换目标不会发起请求：这是“未知但未 loading”，不能显示空态。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target.clone()), cx));
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.active);
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(logs.entries.is_empty());
        assert!(!logs.history_snapshot_is_current());
    });
    assert_eq!(history_calls.load(Ordering::Acquire), 0);
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-empty").is_none());

    // 成功空快照建立后，inactive 换筛选同样不发起请求，旧快照必须立即失效。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_active(true, cx));
    });
    wait_until(&runtime, visual, "active empty success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    assert_eq!(rendered_count_label(visual).as_deref(), Some("0 / 0"));
    assert!(visual.debug_bounds("rr-logs-empty").is_some());

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(false, cx);
            view.set_query("other".to_owned(), cx);
        });
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.active);
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(!logs.history_snapshot_is_current());
    });
    assert_eq!(history_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(
        visual.debug_bounds("rr-logs-empty").is_none(),
        "an inactive filter change must not reuse the previous confirmed empty snapshot"
    );
}

/// 非空旧快照在换筛选等待/失败/重试期间不能继续渲染，也不能开放行动作、清理或分页。
/// 新筛选成功后恢复新行；同筛选刷新仍保留可信旧行，仅由按钮忙态/动作门禁收敛。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_filter_snapshot_hides_rows_and_actions_until_current_result(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let filter_gate = Arc::new(Semaphore::new(0));
    let retry_gate = Arc::new(Semaphore::new(0));
    let refresh_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([
            (1, filter_gate.clone()),
            (3, retry_gate.clone()),
            (4, refresh_gate.clone()),
        ]),
        replies: vec![
            ScriptedReply::EntriesA,
            ScriptedReply::EntriesB,
            ScriptedReply::Error,
            ScriptedReply::EntriesB,
            ScriptedReply::EntriesB,
        ],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-stale",
        "pane-logs-stale",
    );

    // 先建立两条可识别旧行，确保后续“不可见”不是空夹具造成的假通过。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "initial A rows", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(logs.displayed_entries().len(), 2);
        assert!(logs.history_snapshot_is_current());
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-a1")
            .is_some(),
        "initial first row should be rendered"
    );
    assert!(
        visual
            .debug_bounds("rr-logs-copy-history-entry-a1")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-fill-history-entry-a1")
            .is_some()
    );
    assert!(visual.debug_bounds("rr-logs-clear-enabled").is_some());

    // 新筛选等待：旧行仍在内存，但 display/动作/清理/分页都必须关闭。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("b".to_owned(), cx));
    });
    wait_until(&runtime, visual, "filter waiting", |_| {
        history_calls.load(Ordering::Acquire) >= 2
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert_eq!(logs.entries.len(), 2);
        assert_eq!(logs.displayed_entries().len(), 0);
        assert!(!logs.history_snapshot_is_current());
        assert_eq!(logs.page, 1);
    });
    // 旧快照不可复制/翻页；用状态副作用验证门禁，不触碰真实剪贴板。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            assert!(view.status.is_none());
            view.copy_entry("history-entry-a1", cx);
            assert!(view.status.is_none());
            view.set_page(2, cx);
            assert_eq!(view.page, 1);
        });
    });
    for (row, copy) in [
        (
            "rr-logs-row-history-entry-a1",
            "rr-logs-copy-history-entry-a1",
        ),
        (
            "rr-logs-row-history-entry-a2",
            "rr-logs-copy-history-entry-a2",
        ),
        (
            "rr-logs-row-history-entry-b1",
            "rr-logs-copy-history-entry-b1",
        ),
        (
            "rr-logs-row-history-entry-b2",
            "rr-logs-copy-history-entry-b2",
        ),
    ] {
        assert!(visual.debug_bounds(row).is_none());
        assert!(visual.debug_bounds(copy).is_none());
    }
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-pagination").is_none());
    assert!(visual.debug_bounds("rr-logs-clear-disabled").is_some());
    assert!(visual.debug_bounds("rr-logs-empty").is_none());

    // 新筛选成功后才替换为 B 行并恢复动作/清理。
    filter_gate.add_permits(1);
    wait_until(&runtime, visual, "filter B success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(logs.displayed_entries().len(), 2);
        assert!(logs.history_snapshot_is_current());
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-a1")
            .is_none()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-copy-history-entry-b1")
            .is_some()
    );
    assert!(visual.debug_bounds("rr-logs-clear-enabled").is_some());
    assert_ne!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );

    // 新筛选失败：旧 B 行仍在内存，但当前 key 不匹配，不能显示或允许动作。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("fail".to_owned(), cx));
    });
    wait_until(&runtime, visual, "filter failure", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && logs.error.is_some() && history_calls.load(Ordering::Acquire) >= 3
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(logs.entries.len(), 2);
        assert_eq!(logs.displayed_entries().len(), 0);
        assert!(!logs.history_snapshot_is_current());
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            assert!(view.status.is_none());
            view.copy_entry("history-entry-b1", cx);
            assert!(view.status.is_none());
        });
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_none()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-copy-history-entry-b1")
            .is_none()
    );
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );
    assert!(visual.debug_bounds("rr-logs-pagination").is_none());
    assert!(visual.debug_bounds("rr-logs-clear-disabled").is_some());

    // 重试等待仍不显示旧行；成功后才恢复当前筛选结果。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.reload_history(cx));
    });
    wait_until(&runtime, visual, "retry waiting", |_| {
        history_calls.load(Ordering::Acquire) >= 4
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert_eq!(logs.displayed_entries().len(), 0);
        assert!(!logs.history_snapshot_is_current());
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_none()
    );
    assert!(visual.debug_bounds("rr-logs-clear-disabled").is_some());

    retry_gate.add_permits(1);
    wait_until(&runtime, visual, "retry success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 4
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(logs.displayed_entries().len(), 2);
        assert!(logs.history_snapshot_is_current());
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-copy-history-entry-b1")
            .is_some()
    );
    assert!(visual.debug_bounds("rr-logs-clear-enabled").is_some());

    // 同筛选刷新：可信行保持可见，等待只由刷新按钮忙态表达。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.reload_history(cx));
    });
    wait_until(&runtime, visual, "same-filter refresh waiting", |_| {
        history_calls.load(Ordering::Acquire) >= 5
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading);
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.displayed_entries().len(), 2);
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some(),
        "same-filter refresh keeps the trusted rows"
    );
    assert!(
        visual
            .debug_bounds("rr-logs-copy-history-entry-b1")
            .is_some()
    );
    assert_ne!(
        rendered_count_label(visual).as_deref(),
        Some(unknown_count_aria_label().as_str())
    );

    refresh_gate.add_permits(1);
    wait_until(&runtime, visual, "same-filter refresh success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 5
        })
    });
    draw_logs(visual);
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some()
    );
    assert!(visual.debug_bounds("rr-logs-clear-enabled").is_some());
}

/// 已建立基线后切目标，首个可见成功结果才真正启动一次入场；同目标刷新只更新可信数据，
/// 不重新压暗列表。系统动效结论用仅测试注入固定，避免依赖当前 Windows 桌面设置。
///
/// @author kongweiguang
#[gpui_kit::test]
fn visible_target_change_starts_one_entrance_and_refresh_does_not_replay(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    app.update(|app| app.set_reduce_motion(false));
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let b_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([(1, b_gate.clone())]),
        replies: vec![
            ScriptedReply::EntryA,
            ScriptedReply::EntriesB,
            ScriptedReply::EntriesB,
        ],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target_a = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-visible-a",
        "pane-logs-visible-a",
    );
    let target_b = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-visible-b.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-visible-b",
        "pane-logs-visible-b",
    );

    // 首屏成功只建立基线，不播放入场。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_motion_allowed_for_test(true);
            view.set_active(true, cx);
            view.set_target(Some(target_a), cx);
        });
    });
    wait_until(&runtime, visual, "visible baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(logs.started_entrances_for_test, 0);
        assert!(!logs.data_entrance.awaiting_result());
        assert!(logs.history_snapshot_is_current());
    });

    // 切到真正不同的目标：等待真实结果，首个可见成功才启动一次入场。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_b), cx));
    });
    visual.update(|_, app| {
        assert!(
            view.read(app).data_entrance.awaiting_result(),
            "a real target change must arm exactly one entrance"
        );
    });
    wait_until(&runtime, visual, "target B read started", |_| {
        history_calls.load(Ordering::Acquire) >= 2
    });
    b_gate.add_permits(1);
    wait_until(&runtime, visual, "visible B success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert_eq!(logs.displayed_entries().len(), 2);
        assert!(logs.history_snapshot_is_current());
        assert_eq!(
            logs.started_entrances_for_test, 1,
            "the first visible result of the new target must start one entrance"
        );
        assert!(!logs.data_entrance.awaiting_result());
    });

    // 同目标刷新仍更新可信内容，但不能再次启动入场。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.reload_history(cx));
    });
    wait_until(&runtime, visual, "same-target refresh", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 3
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert_eq!(
            logs.started_entrances_for_test, 1,
            "an ordinary same-target refresh must not replay the entrance"
        );
        assert!(!logs.data_entrance.awaiting_result());
    });
}

/// 仅展示 detail 变化不改变真实历史身份：不重读、不清可信快照，计数和行保持原样。
///
/// @author kongweiguang
#[gpui_kit::test]
fn detail_only_target_update_keeps_trusted_snapshot_without_reading(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::new(),
        replies: vec![ScriptedReply::EntryA],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let pane_id = PaneId::parse("pane-logs-detail").expect("valid pane identity");
    let target = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-detail",
        "pane-logs-detail",
    );
    let renamed = target
        .clone()
        .with_pane_detail(&pane_id, "Renamed pane title".to_owned());
    assert_eq!(
        renamed, target,
        "detail must not change the history identity"
    );
    assert_eq!(renamed.detail(), "Renamed pane title");

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "detail baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    assert_eq!(rendered_count_label(visual).as_deref(), Some("1-1 / 1"));
    let reads_before_detail = history_calls.load(Ordering::Acquire);

    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(renamed), cx));
    });
    // detail-only 不应发起新请求；给受控 runtime 一次真实推进机会，避免只靠 GPUI 泵误判“没有读取”。
    visual.run_until_parked();
    runtime.block_on(tokio::task::yield_now());
    visual.run_until_parked();
    visual.update(|_, app| {
        let logs = view.read(app);
        assert_eq!(
            history_calls.load(Ordering::Acquire),
            reads_before_detail,
            "a detail-only update must not issue another history read"
        );
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.displayed_entries().len(), 1);
        assert_eq!(
            logs.target.as_ref().map(LogsTarget::detail),
            Some("Renamed pane title")
        );
    });
    draw_logs(visual);
    assert_eq!(
        rendered_count_label(visual).as_deref(),
        Some("1-1 / 1"),
        "the trusted range must survive a detail-only target update"
    );
    assert!(
        visual.debug_bounds("rr-logs-row-history-entry-a").is_some(),
        "the trusted row must stay visible after a detail-only update"
    );
}

/// 真实异步 A 挂起、B 成功后再释放 A：旧回执必须被代次和目标校验拒绝，
/// B 的行、计数、目标与可信快照都不能被覆盖。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_previous_target_response_cannot_replace_current_rows_count_or_target(
    app: &mut TestAppContext,
) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let a_gate = Arc::new(Semaphore::new(0));
    let b_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([(0, a_gate.clone()), (1, b_gate.clone())]),
        replies: vec![ScriptedReply::EntriesA, ScriptedReply::EntriesB],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target_a = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-stale-a",
        "pane-logs-stale-a",
    );
    let target_b = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-stale-b.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-stale-b",
        "pane-logs-stale-b",
    );

    // A 请求已真实进入 repository 并挂起。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target_a), cx);
        });
    });
    wait_until(&runtime, visual, "A request gated", |_| {
        history_calls.load(Ordering::Acquire) >= 1
    });
    visual.update(|_, app| assert!(view.read(app).loading));

    // 切到 B 后 B 先成功；此时 A 仍挂在门控上。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_b.clone()), cx));
    });
    wait_until(&runtime, visual, "B request gated", |_| {
        history_calls.load(Ordering::Acquire) >= 2
    });
    b_gate.add_permits(1);
    wait_until(&runtime, visual, "B success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.target.as_ref(), Some(&target_b));
        assert_eq!(logs.displayed_entries().len(), 2);
        assert_eq!(
            logs.displayed_entries()
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            vec!["b-command-1", "b-command-2"]
        );
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some()
    );
    assert_eq!(rendered_count_label(visual).as_deref(), Some("1-2 / 2"));

    // 释放 A 的旧回执；必须等拒绝计数真实递增，证明旧回执已进入 `!response_is_current`
    // 分支，而不是固定泵次数下的假通过。超时仍明确失败，不无限等待。
    let reads_before_release = history_calls.load(Ordering::Acquire);
    a_gate.add_permits(1);
    wait_until(&runtime, visual, "stale A receipt rejected", |visual| {
        visual.update(|_, app| view.read(app).stale_responses_rejected_for_test >= 1)
    });
    // 拒绝分支之后再多推进几次，确认迟到的状态更新没有在下一次回调中回写 B。
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(!logs.loading);
        assert!(logs.error.is_none());
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.target.as_ref(), Some(&target_b));
        assert_eq!(logs.displayed_entries().len(), 2);
        assert_eq!(
            logs.displayed_entries()
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            vec!["b-command-1", "b-command-2"]
        );
        assert_eq!(
            history_calls.load(Ordering::Acquire),
            reads_before_release,
            "a stale receipt must not start another read"
        );
        assert_eq!(
            logs.stale_responses_rejected_for_test, 1,
            "exactly the released A receipt should be rejected; B stays current"
        );
    });
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-b1")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("rr-logs-row-history-entry-a1")
            .is_none()
    );
    assert_eq!(rendered_count_label(visual).as_deref(), Some("1-2 / 2"));
}

/// 应用内减少动效即使允许系统动效也不启动入场，且不会留下等待窗口。
///
/// @author kongweiguang
#[gpui_kit::test]
fn reduce_motion_target_success_does_not_start_entrance(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    app.update(|app| app.set_reduce_motion(true));
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let history_calls = Arc::new(AtomicUsize::new(0));
    let b_gate = Arc::new(Semaphore::new(0));
    let history = Arc::new(ScriptedHistory {
        calls: history_calls.clone(),
        gates: BTreeMap::from([(1, b_gate.clone())]),
        replies: vec![ScriptedReply::EntryA, ScriptedReply::Empty],
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let commands = Arc::new(commands);

    let mounted = Rc::new(RefCell::new(None));
    let mounted_view = mounted.clone();
    let commands_for_view = commands.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|cx| LogsView::new(commands_for_view, executor_for_view, window, cx));
        *mounted_view.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view = mounted
        .borrow_mut()
        .take()
        .expect("LogsView window fixture mounted");

    let target_a = logs_target(
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("sh"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        "session-logs-reduce-a",
        "pane-logs-reduce-a",
    );
    let target_b = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-reduce-b.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-reduce-b",
        "pane-logs-reduce-b",
    );

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_motion_allowed_for_test(true);
            view.set_active(true, cx);
            view.set_target(Some(target_a), cx);
        });
    });
    wait_until(&runtime, visual, "reduce baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_target(Some(target_b), cx));
    });
    wait_until(&runtime, visual, "reduce target read", |_| {
        history_calls.load(Ordering::Acquire) >= 2
    });
    // B 仍在等待真实结果：此时必须已 armed，且绑定时的缓存动效结论为不允许，
    // 这样成功后的“无启动”才证明 reduce_motion 门禁生效，而不是没有等待窗口。
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(
            logs.data_entrance.awaiting_result(),
            "B must be armed while its result is still pending"
        );
        assert!(
            !logs.data_entrance.allows_motion(),
            "reduce motion must be cached as disallowed at target-change time"
        );
    });
    b_gate.add_permits(1);
    wait_until(&runtime, visual, "reduce target success", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && history_calls.load(Ordering::Acquire) >= 2
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        let now = Instant::now();
        assert!(logs.error.is_none());
        assert!(!logs.data_entrance.awaiting_result());
        assert_eq!(
            logs.started_entrances_for_test, 0,
            "reduce motion must suppress the entrance even when the injected system preference allows it"
        );
        assert!(!logs.data_entrance.is_running(now));
        assert_eq!(logs.data_entrance.opacity(now), 1.0);
    });
}

/// 删除成功后紧接着的权威回读不能再清掉确认：成功和清理成功都必须保留提示，
/// 直到用户开启新交互或目标切换；后台回读成功只更新列表。
///
/// @author kongweiguang
#[gpui_kit::test]
fn mutation_success_confirmation_survives_authoritative_reload(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let list_calls = Arc::new(AtomicUsize::new(0));
    let delete_calls = Arc::new(AtomicUsize::new(0));
    let clear_calls = Arc::new(AtomicUsize::new(0));
    let reload_gate = Arc::new(Semaphore::new(0));
    let pane = PaneId::parse("pane-logs-mutation").expect("valid pane identity");
    let history = Arc::new(ScriptedMutationHistory {
        list_calls: list_calls.clone(),
        delete_calls: delete_calls.clone(),
        clear_calls: clear_calls.clone(),
        // 写操作成功后的第一次回读停在门控上，先验证等待期确认仍在；
        // 再释放门控验证成功落盘后也不会被清空。
        gate_list_call: Some(1),
        gate: Some(reload_gate.clone()),
        fail_list_call: None,
        delete_gate: None,
        entries: mutation_entries(&pane),
        delete_result: Ok(true),
        clear_result: Ok(2),
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let (view, visual) = mount_logs_view(app, Arc::new(commands), executor);
    let target = local_logs_target("session-logs-mutation", "pane-logs-mutation");

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "mutation baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.history_snapshot_is_current());
        assert_eq!(logs.displayed_entries().len(), 2);
        assert!(logs.status.is_none());
    });

    // 删除成功：确认立即写入，权威回读仍在门控上时提示必须保持可见。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.delete_entry("history-entry-a1", cx));
    });
    wait_until(&runtime, visual, "delete started", |_| {
        delete_calls.load(Ordering::Acquire) >= 1 && list_calls.load(Ordering::Acquire) >= 2
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.loading, "mutation reload should still be pending");
        assert_eq!(
            logs.status.as_deref(),
            Some(crate::locale::text("历史记录已删除", "History entry deleted").as_ref()),
            "the delete confirmation must survive the in-flight authoritative reload"
        );
    });

    // 回读成功落盘：列表被权威替换，但确认不能被同一结果抹除。
    reload_gate.add_permits(1);
    wait_until(&runtime, visual, "delete reload settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert_eq!(
            logs.status.as_deref(),
            Some(crate::locale::text("历史记录已删除", "History entry deleted").as_ref()),
            "a successful authoritative reload must not clear the delete confirmation"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_some());

    // 清空成功同样保留提示；这里不需要门控，验证成功后立即落盘的成功路径。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.clear_history(cx));
    });
    wait_until(&runtime, visual, "clear settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && clear_calls.load(Ordering::Acquire) >= 1
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert_eq!(
            logs.status.as_deref(),
            Some(crate::locale::text("已清理 2 条历史记录", "Cleared 2 history entries").as_ref()),
            "a successful clear confirmation must survive the authoritative reload"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_some());
}

/// 删除返回“已不存在”同样是权威结果，其确认不能被紧随其后的回读清掉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn missing_entry_confirmation_survives_authoritative_reload(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let list_calls = Arc::new(AtomicUsize::new(0));
    let delete_calls = Arc::new(AtomicUsize::new(0));
    let pane = PaneId::parse("pane-logs-missing").expect("valid pane identity");
    let history = Arc::new(ScriptedMutationHistory {
        list_calls: list_calls.clone(),
        delete_calls: delete_calls.clone(),
        clear_calls: Arc::new(AtomicUsize::new(0)),
        gate_list_call: None,
        gate: None,
        fail_list_call: None,
        delete_gate: None,
        entries: mutation_entries(&pane),
        delete_result: Ok(false),
        clear_result: Ok(0),
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let (view, visual) = mount_logs_view(app, Arc::new(commands), executor);
    let target = local_logs_target("session-logs-missing", "pane-logs-missing");

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "missing baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.delete_entry("history-entry-a1", cx));
    });
    wait_until(&runtime, visual, "missing delete reload", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading
                && delete_calls.load(Ordering::Acquire) >= 1
                && list_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert_eq!(
            logs.status.as_deref(),
            Some(
                crate::locale::text("历史记录已不存在", "History entry no longer exists").as_ref()
            ),
            "the already-missing confirmation must survive the authoritative reload"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_some());
}

/// 权威回读失败不能被旧确认掩盖：错误显示时确认必须作废，避免用户误以为列表已核对。
///
/// @author kongweiguang
#[gpui_kit::test]
fn mutation_confirmation_is_cleared_when_authoritative_reload_fails(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let list_calls = Arc::new(AtomicUsize::new(0));
    let delete_calls = Arc::new(AtomicUsize::new(0));
    let pane = PaneId::parse("pane-logs-reload-fail").expect("valid pane identity");
    let history = Arc::new(ScriptedMutationHistory {
        list_calls: list_calls.clone(),
        delete_calls: delete_calls.clone(),
        clear_calls: Arc::new(AtomicUsize::new(0)),
        gate_list_call: None,
        gate: None,
        // 写操作后的第一次回读失败，锁住“失败必须替换确认”的边界。
        fail_list_call: Some(1),
        delete_gate: None,
        entries: mutation_entries(&pane),
        delete_result: Ok(true),
        clear_result: Ok(0),
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let (view, visual) = mount_logs_view(app, Arc::new(commands), executor);
    let target = local_logs_target("session-logs-reload-fail", "pane-logs-reload-fail");

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "reload-fail baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.delete_entry("history-entry-a1", cx));
    });
    wait_until(&runtime, visual, "reload-fail settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading
                && logs.error.is_some()
                && delete_calls.load(Ordering::Acquire) >= 1
                && list_calls.load(Ordering::Acquire) >= 2
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_some());
        assert!(
            logs.status.is_none(),
            "a failed authoritative reload must not keep the stale confirmation"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_none());
}

/// 新搜索、来源筛选和目标切换都开启新的交互上下文：保留过的删除确认必须立即作废，
/// 不能搭新查询的等待窗口继续显示，也不能污染另一目标。
///
/// @author kongweiguang
#[gpui_kit::test]
fn mutation_confirmation_expires_on_new_interaction_and_target_change(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let list_calls = Arc::new(AtomicUsize::new(0));
    let delete_calls = Arc::new(AtomicUsize::new(0));
    let pane = PaneId::parse("pane-logs-expire").expect("valid pane identity");
    let history = Arc::new(ScriptedMutationHistory {
        list_calls: list_calls.clone(),
        delete_calls: delete_calls.clone(),
        clear_calls: Arc::new(AtomicUsize::new(0)),
        gate_list_call: None,
        gate: None,
        fail_list_call: None,
        delete_gate: None,
        entries: mutation_entries(&pane),
        delete_result: Ok(true),
        clear_result: Ok(0),
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let (view, visual) = mount_logs_view(app, Arc::new(commands), executor);
    let target = local_logs_target("session-logs-expire", "pane-logs-expire");
    let other_target = logs_target(
        SessionTarget::Telnet(TelnetSessionTarget {
            host: "logs-expire-other.example.test".to_owned(),
            port: 23,
        }),
        "session-logs-expire-other",
        "pane-logs-expire-other",
    );

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "expire baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 1
        })
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.delete_entry("history-entry-a1", cx));
    });
    wait_until(&runtime, visual, "expire delete settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && logs.status.is_some() && delete_calls.load(Ordering::Acquire) >= 1
        })
    });

    // 新搜索：旧确认立即失效，新查询等待期不能继续显示“已删除”。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("other".to_owned(), cx));
    });
    visual.update(|_, app| {
        assert!(
            view.read(app).status.is_none(),
            "a new query must invalidate the previous delete confirmation immediately"
        );
    });
    wait_until(&runtime, visual, "expire new query", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 3
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.error.is_none());
        assert!(
            logs.status.is_none(),
            "the completed new query must not resurrect the old confirmation"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_none());

    // 目标切换同样作废旧提示，旧目标确认不能出现在另一目标上。
    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_target(Some(other_target.clone()), cx)
        });
    });
    wait_until(&runtime, visual, "expire other target", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && logs.target.as_ref() == Some(&other_target)
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(logs.status.is_none());
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_none());
}

/// 用户在删除回执到达前换了搜索：迟到成功仍须刷新列表，但不能把旧确认写回新交互。
///
/// @author kongweiguang
#[gpui_kit::test]
fn late_mutation_receipt_does_not_write_back_into_new_interaction(app: &mut TestAppContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let list_calls = Arc::new(AtomicUsize::new(0));
    let delete_calls = Arc::new(AtomicUsize::new(0));
    let delete_gate = Arc::new(Semaphore::new(0));
    let pane = PaneId::parse("pane-logs-late").expect("valid pane identity");
    let history = Arc::new(ScriptedMutationHistory {
        list_calls: list_calls.clone(),
        delete_calls: delete_calls.clone(),
        clear_calls: Arc::new(AtomicUsize::new(0)),
        gate_list_call: None,
        gate: None,
        fail_list_call: None,
        delete_gate: Some(delete_gate.clone()),
        entries: mutation_entries(&pane),
        delete_result: Ok(true),
        clear_result: Ok(0),
    });
    let (commands, _catalog_calls) = root_commands_service::commands_service_with_history(
        history,
        Arc::new(EmptySessionService),
    );
    let (view, visual) = mount_logs_view(app, Arc::new(commands), executor);
    let target = local_logs_target("session-logs-late", "pane-logs-late");

    visual.update(|_, app| {
        view.update(app, |view, cx| {
            view.set_active(true, cx);
            view.set_target(Some(target), cx);
        });
    });
    wait_until(&runtime, visual, "late baseline", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 1
        })
    });

    // 删除请求进入仓储并挂起，用户随后换了搜索。
    visual.update(|_, app| {
        view.update(app, |view, cx| view.delete_entry("history-entry-a1", cx));
    });
    wait_until(&runtime, visual, "late delete gated", |_| {
        delete_calls.load(Ordering::Acquire) >= 1
    });
    visual.update(|_, app| {
        view.update(app, |view, cx| view.set_query("late".to_owned(), cx));
    });
    wait_until(&runtime, visual, "late new query settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) >= 2
        })
    });
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(
            logs.status.is_none(),
            "a new query must clear the in-progress delete status"
        );
    });
    let reads_before_receipt = list_calls.load(Ordering::Acquire);

    // 释放迟到回执：回执仍触发一次权威刷新，但旧“已删除”不能回到新交互界面。
    delete_gate.add_permits(1);
    wait_until(&runtime, visual, "late delete settled", |visual| {
        visual.update(|_, app| {
            let logs = view.read(app);
            !logs.loading && list_calls.load(Ordering::Acquire) > reads_before_receipt
        })
    });
    draw_logs(visual);
    visual.update(|_, app| {
        let logs = view.read(app);
        assert!(
            logs.status.is_none(),
            "a late delete receipt must not resurrect the confirmation in a new interaction"
        );
    });
    assert!(visual.debug_bounds("rr-logs-action-status").is_none());
}
