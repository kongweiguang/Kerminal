//! tmux MCP 兼容映射契约测试；不连接本机或远端 tmux server。
//!
//! @author kongweiguang

use kerminal_domain::operations::{
    TmuxAttachPlan, TmuxMutationReceipt, TmuxPane, TmuxPaneCapture, TmuxSession, TmuxTarget,
    TmuxWindow,
};
use serde_json::json;

use crate::operations_tools::tmux::{
    TmuxArgs, legacy_attach_value, legacy_capture_value, legacy_collection, legacy_kill_value,
    legacy_mutation_value, legacy_pane_value, legacy_session_value, legacy_target_ref,
    legacy_target_value, legacy_window_value,
};

/// 构造带 socket 的本地目标，验证兼容 ID 仍按旧版拼接规则区分 tmux server。
fn local_target_with_socket() -> TmuxTarget {
    TmuxTarget {
        host_id: None,
        profile_id: Some("work".to_owned()),
        socket_name: Some("dev".to_owned()),
        socket_path: None,
        tmux_path: None,
    }
}

/// 用最小稳定字段构造应用 session 投影，避免测试绑定 infrastructure 命令解析。
fn session() -> TmuxSession {
    TmuxSession {
        id: "$3".to_owned(),
        name: "build".to_owned(),
        attached: true,
        clients: 1,
        windows: 2,
        current_path: Some("/work".to_owned()),
        created_at: Some(10),
        activity_at: Some(20),
        target_ref: "application-ref".to_owned(),
    }
}

/// 旧版 wire shape 使用 sessions/status 和 socket-scoped targetRef，列表项只代表真实运行态。
#[test]
fn preserves_session_wire_fields_and_running_status() {
    let target_ref = legacy_target_ref(&local_target_with_socket());
    let mapped = legacy_session_value(session(), &target_ref);

    assert_eq!(target_ref, "local:work|L:dev");
    assert_eq!(mapped["id"], "$3");
    assert_eq!(mapped["targetRef"], "local:work|L:dev");
    assert_eq!(mapped["status"], "running");
    assert_eq!(mapped["currentPath"], "/work");
}

/// 每类查询继续使用旧 MCP data wrapper key，而不是只返回通用 items 数组。
#[test]
fn keeps_family_collection_wrapper_key() {
    let item = vec![json!({"id":"$3"})];

    assert_eq!(
        legacy_collection("sessions", item.clone()),
        json!({"sessions":[{"id":"$3"}]})
    );
    assert_eq!(
        legacy_collection("windows", item.clone()),
        json!({"windows":[{"id":"$3"}]})
    );
    assert_eq!(
        legacy_collection("panes", item),
        json!({"panes":[{"id":"$3"}]})
    );
}

/// window 以真实 query index/layout/flags 输出旧字段，并保留新 paneCount 等信息。
#[test]
fn maps_window_to_legacy_field_names_without_losing_new_data() {
    let mapped = legacy_window_value(
        TmuxWindow {
            id: "@4".to_owned(),
            name: "editor".to_owned(),
            index: 2,
            active: true,
            pane_count: 3,
            width: 120,
            height: 40,
            layout: Some("8e4a,120x40,0,0,0".to_owned()),
            flags: Some("*Z".to_owned()),
            current_path: Some("/work".to_owned()),
            target_ref: "application-internal-ref".to_owned(),
        },
        "$3",
        "ssh:saved:one|S:/run/tmux.sock",
    )
    .expect("window mapping should serialize all fields");

    assert_eq!(mapped["id"], "@4");
    assert_eq!(mapped["sessionId"], "$3");
    assert_eq!(mapped["index"], 2);
    assert_eq!(mapped["layout"], "8e4a,120x40,0,0,0");
    assert_eq!(mapped["flags"], "*Z");
    assert_eq!(mapped["panes"], 3);
    assert_eq!(mapped["paneCount"], 3);
    assert_eq!(mapped["width"], 120);
    assert_eq!(mapped["targetRef"], "ssh:saved:one|S:/run/tmux.sock");
}

/// pane index 保留且空 title/command 映射回旧的 nullable 字段。
#[test]
fn maps_empty_pane_title_and_command_to_null() {
    let mapped = legacy_pane_value(
        TmuxPane {
            id: "%2".to_owned(),
            session_id: "$3".to_owned(),
            window_id: "@4".to_owned(),
            index: 1,
            title: String::new(),
            current_command: String::new(),
            current_path: None,
            width: 80,
            height: 24,
            active: false,
            dead: true,
            target_ref: "application-internal-ref".to_owned(),
        },
        "local:work|L:dev",
    )
    .expect("pane mapping should serialize all fields");

    assert_eq!(mapped["id"], "%2");
    assert_eq!(mapped["sessionId"], "$3");
    assert_eq!(mapped["index"], 1);
    assert!(mapped["title"].is_null());
    assert!(mapped["currentCommand"].is_null());
    assert_eq!(mapped["dead"], true);
    assert_eq!(mapped["targetRef"], "local:work|L:dev");
}

/// kill 回执区分本次确实关闭和目标此前已不存在两种成功结果。
#[test]
fn preserves_kill_result_boolean() {
    assert_eq!(
        legacy_kill_value("$3", true),
        json!({"sessionId":"$3","killed":true})
    );
    assert_eq!(
        legacy_kill_value("$3", false),
        json!({"sessionId":"$3","killed":false})
    );
}

/// 主机目标在旧 MCP 中使用 tagged `kind/hostId`，targetRef 仍编码 socket 作用域。
#[test]
fn keeps_target_kind_and_host_fields_in_legacy_shape() {
    let args = TmuxArgs {
        target_kind: Some("ssh".to_owned()),
        host_id: Some("saved:one".to_owned()),
        socket_path: Some("/run/tmux.sock".to_owned()),
        ..TmuxArgs::default()
    };
    let target = TmuxTarget {
        host_id: Some(kerminal_domain::identity::HostId::parse("saved:one").unwrap()),
        profile_id: None,
        socket_name: None,
        socket_path: Some("/run/tmux.sock".to_owned()),
        tmux_path: None,
    };

    assert_eq!(
        legacy_target_value(&args),
        json!({"kind":"ssh","hostId":"saved:one"})
    );
    assert_eq!(legacy_target_ref(&target), "ssh:saved:one|S:/run/tmux.sock");
}

/// socket scope 必须保留 tmux 实际收到的空格，避免显示的 targetRef 指向另一 server。
#[test]
fn target_ref_preserves_valid_socket_whitespace() {
    let socket_name = TmuxTarget {
        host_id: None,
        profile_id: None,
        socket_name: Some(" dev ".to_owned()),
        socket_path: None,
        tmux_path: None,
    };
    let socket_path = TmuxTarget {
        host_id: Some(kerminal_domain::identity::HostId::parse("saved:one").unwrap()),
        profile_id: None,
        socket_name: None,
        socket_path: Some(" /run/tmux.sock ".to_owned()),
        tmux_path: None,
    };

    assert_eq!(legacy_target_ref(&socket_name), "local|L: dev ");
    assert_eq!(
        legacy_target_ref(&socket_path),
        "ssh:saved:one|S: /run/tmux.sock "
    );
}

/// 创建和重命名保留旧 session wrapper，并在其旁边提供应用层刷新回执。
#[test]
fn preserves_mutation_session_wrapper_and_refresh_receipt() {
    let mapped = legacy_mutation_value(
        TmuxMutationReceipt {
            session: session(),
            refresh_required: true,
        },
        "local:work|L:dev",
    );

    assert_eq!(mapped["session"]["id"], "$3");
    assert_eq!(mapped["session"]["status"], "running");
    assert_eq!(mapped["session"]["targetRef"], "local:work|L:dev");
    assert_eq!(mapped["refreshRequired"], true);
}

/// 旧 `capture.lines` 是请求上限；新增 capturedLines 表示本次响应中实际取得的行数。
#[test]
fn distinguishes_capture_limit_from_actual_line_count() {
    let mapped = legacy_capture_value(
        TmuxPaneCapture {
            pane_id: "%2".to_owned(),
            text: "one\ntwo".to_owned(),
            captured_lines: 2,
            truncated: true,
        },
        40,
    );

    assert_eq!(mapped["capture"]["paneId"], "%2");
    assert_eq!(mapped["capture"]["lines"], 40);
    assert_eq!(mapped["capture"]["capturedLines"], 2);
    assert_eq!(mapped["capture"]["truncated"], true);
}

/// 本地 attach 计划回到旧 local tagged union，且绑定时间保持 Unix 秒字符串。
#[test]
fn maps_local_attach_plan_to_legacy_launch_union() {
    let mapped = legacy_attach_value(TmuxAttachPlan {
        target: local_target_with_socket(),
        session_id: "$3".to_owned(),
        session_name: "build".to_owned(),
        program: "tmux".to_owned(),
        arguments: vec![
            "attach-session".to_owned(),
            "-t".to_owned(),
            "$3".to_owned(),
        ],
        working_directory: Some("/work".to_owned()),
    });

    assert_eq!(mapped["launch"]["mode"], "local");
    assert_eq!(mapped["launch"]["terminal"]["shell"], "tmux");
    assert_eq!(mapped["launch"]["terminal"]["args"][2], "$3");
    assert_eq!(mapped["launch"]["terminal"]["cwd"], "/work");
    assert_eq!(mapped["launch"]["binding"]["socketName"], "dev");
    assert!(
        mapped["launch"]["binding"]["attachedAt"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some()
    );
}

/// SSH attach 保留 SSH tagged union，并把 argv 分别按 POSIX 单引号规则编码。
#[test]
fn maps_ssh_attach_plan_without_losing_argument_boundaries() {
    let mapped = legacy_attach_value(TmuxAttachPlan {
        target: TmuxTarget {
            host_id: Some(kerminal_domain::identity::HostId::parse("saved:one").unwrap()),
            profile_id: None,
            socket_name: None,
            socket_path: Some("/run/tmux.sock".to_owned()),
            tmux_path: None,
        },
        session_id: "$3".to_owned(),
        session_name: "build".to_owned(),
        program: "tmux".to_owned(),
        arguments: vec![
            "attach-session".to_owned(),
            "-t".to_owned(),
            "a b".to_owned(),
            "O'Hare".to_owned(),
        ],
        working_directory: Some("/work".to_owned()),
    });

    assert_eq!(mapped["launch"]["mode"], "ssh");
    assert_eq!(mapped["launch"]["hostId"], "saved:one");
    assert_eq!(
        mapped["launch"]["remoteCommand"],
        "'tmux' 'attach-session' '-t' 'a b' 'O'\\''Hare'"
    );
    assert_eq!(mapped["launch"]["binding"]["socketPath"], "/run/tmux.sock");
}
