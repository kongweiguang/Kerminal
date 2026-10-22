//! tmux 固定格式输出、socket scope 与删除结果的契约测试。
//!
//! @author kongweiguang

use kerminal_domain::operations::{TmuxSession, TmuxTarget};

use crate::{
    PublicErrorCode,
    operations::{
        CommandExecutionResult,
        tmux::{
            FIELD_SEPARATOR, capability_from_output, classify_kill_result, no_server_running,
            pane_format, parse_pane, parse_session, parse_window, receipt_after_create_refresh,
            target_program_and_scope, target_reference, window_format,
        },
    },
};

/// 构造不接触外部 tmux server 的固定目标，供投影与 scope 用例共用。
fn target() -> TmuxTarget {
    TmuxTarget::default()
}

/// 每种 tmux server scope 都先强制 UTF-8 输出，同时保留 socket 选项的独立 argv 位置。
///
/// @author kongweiguang
#[test]
fn target_program_scope_enables_utf8_for_default_named_and_path_sockets() {
    let default = target_program_and_scope(&target());
    assert_eq!(default.0, "tmux");
    assert_eq!(default.1, vec!["-u".to_owned()]);

    let mut named_socket = target();
    named_socket.socket_name = Some("build".to_owned());
    let named = target_program_and_scope(&named_socket);
    assert_eq!(named.0, "tmux");
    assert_eq!(
        named.1,
        vec!["-u".to_owned(), "-L".to_owned(), "build".to_owned()]
    );

    let mut path_socket = target();
    path_socket.socket_path = Some("/tmp/tmux-kerminal.sock".to_owned());
    path_socket.tmux_path = Some("/usr/bin/tmux".to_owned());
    let path = target_program_and_scope(&path_socket);
    assert_eq!(path.0, "/usr/bin/tmux");
    assert_eq!(
        path.1,
        vec![
            "-u".to_owned(),
            "-S".to_owned(),
            "/tmp/tmux-kerminal.sock".to_owned()
        ]
    );
}

/// 构造完整的伪命令回执，所有判定都不需要启动本机或远端进程。
fn command_result(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    truncated: bool,
) -> CommandExecutionResult {
    CommandExecutionResult {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        exit_code,
        truncated,
        input_complete: true,
    }
}

/// 用生产 parser 生成固定 session 投影，测试可隔离回读身份和权威摘要选择。
///
/// @author kongweiguang
fn tmux_session(
    id: &str,
    name: &str,
    clients: u32,
    windows: u32,
    target: &TmuxTarget,
) -> TmuxSession {
    let line = [
        id.to_owned(),
        name.to_owned(),
        clients.to_string(),
        windows.to_string(),
        "/srv/kerminal".to_owned(),
        "100".to_owned(),
        "101".to_owned(),
    ]
    .join(&FIELD_SEPARATOR.to_string());
    parse_session(line.as_bytes(), target).expect("valid tmux session fixture")
}

/// 创建后必须用权威 session 列表更新摘要，不能继续展示命令输出中的旧计数。
///
/// @author kongweiguang
#[test]
fn create_receipt_uses_authoritative_session_summary() {
    let target = target();
    let created = tmux_session("$1", "rightui", 0, 1, &target);
    let refreshed = tmux_session("$1", "rightui", 1, 2, &target);

    let receipt = receipt_after_create_refresh(created, Ok(vec![refreshed.clone()]))
        .expect("the created session remains present");

    assert_eq!(receipt.session, refreshed);
    assert!(!receipt.refresh_required);
}

/// 空列表或同 ID 但不同 socket scope 都不能伪装成创建后仍存活。
///
/// @author kongweiguang
#[test]
fn create_receipt_rejects_missing_session_identity() {
    let created_target = target();
    let created = tmux_session("$1", "rightui", 0, 1, &created_target);

    let empty = receipt_after_create_refresh(created.clone(), Ok(Vec::new()))
        .expect_err("an empty authoritative list means the session disappeared");
    assert_eq!(empty.public_code(), PublicErrorCode::NotFound);

    let mut other_target = target();
    other_target.socket_name = Some("other-server".to_owned());
    let same_id_elsewhere = tmux_session("$1", "rightui", 0, 1, &other_target);
    let wrong_scope = receipt_after_create_refresh(created, Ok(vec![same_id_elsewhere]))
        .expect_err("an identically named ID from another server is not the created session");
    assert_eq!(wrong_scope.public_code(), PublicErrorCode::NotFound);
}

/// 回读本身失败时保留写入已确认但状态待核对的回执，禁止在不确定状态下自动重放创建。
///
/// @author kongweiguang
#[test]
fn create_receipt_marks_failed_refresh_for_explicit_retry() {
    let created = tmux_session("$1", "rightui", 0, 1, &target());

    let receipt = receipt_after_create_refresh(
        created.clone(),
        Err(crate::AppError::infrastructure("test refresh failure")),
    )
    .expect("the confirmed creation remains a receipt when refresh fails");

    assert_eq!(receipt.session, created);
    assert!(receipt.refresh_required);
}

/// 固定格式必须请求恢复 window 序号、布局和原始 flags，而非仅返回可见尺寸。
#[test]
fn window_format_requests_legacy_and_extended_metadata() {
    assert_eq!(
        window_format(),
        [
            "#{window_id}",
            "#{window_index}",
            "#{window_name}",
            "#{window_active}",
            "#{window_panes}",
            "#{window_width}",
            "#{window_height}",
            "#{window_layout}",
            "#{window_flags}",
            "#{pane_current_path}",
        ]
        .join(&FIELD_SEPARATOR.to_string())
    );
}

/// 固定格式保留 pane_index 和旧 pane 字段的原始顺序，避免序号错误地取 pane id。
#[test]
fn pane_format_requests_real_index_and_legacy_metadata() {
    assert_eq!(
        pane_format(),
        [
            "#{pane_id}",
            "#{session_id}",
            "#{window_id}",
            "#{pane_index}",
            "#{pane_title}",
            "#{pane_current_command}",
            "#{pane_current_path}",
            "#{pane_width}",
            "#{pane_height}",
            "#{pane_active}",
            "#{pane_dead}",
        ]
        .join(&FIELD_SEPARATOR.to_string())
    );
}

/// window 解析保持 tmux 原始序号、layout、flags 和空路径语义。
#[test]
fn parses_window_index_layout_flags_and_optional_path() {
    let mut target = target();
    target.socket_name = Some("ops-test".to_owned());
    let line = [
        "@3",
        "4",
        "editor",
        "1",
        "2",
        "132",
        "44",
        "main-vertical",
        "*Z",
        "/work/kerminal",
    ]
    .join(&FIELD_SEPARATOR.to_string());

    let window = parse_window(line.as_bytes(), &target).expect("parse tmux window row");
    assert_eq!(window.id, "@3");
    assert_eq!(window.index, 4);
    assert_eq!(window.name, "editor");
    assert!(window.active);
    assert_eq!(window.pane_count, 2);
    assert_eq!(window.layout.as_deref(), Some("main-vertical"));
    assert_eq!(window.flags.as_deref(), Some("*Z"));
    assert_eq!(window.current_path.as_deref(), Some("/work/kerminal"));
    assert_eq!(window.target_ref, "local|L:ops-test");
}

/// pane 序号独立于 pane ID；title 与 command 原样留空供 MCP 兼容层映射为 null。
#[test]
fn parses_pane_index_and_preserves_empty_legacy_text() {
    let line = [
        "%7",
        "$2",
        "@3",
        "6",
        "",
        "",
        "/work/kerminal",
        "80",
        "20",
        "1",
        "0",
    ]
    .join(&FIELD_SEPARATOR.to_string());

    let pane = parse_pane(line.as_bytes(), &target()).expect("parse tmux pane row");
    assert_eq!(pane.id, "%7");
    assert_eq!(pane.index, 6);
    assert_eq!(pane.session_id, "$2");
    assert_eq!(pane.window_id, "@3");
    assert!(pane.title.is_empty());
    assert!(pane.current_command.is_empty());
    assert_eq!(pane.current_path.as_deref(), Some("/work/kerminal"));
    assert!(pane.active);
    assert!(!pane.dead);
}

/// 字段数或数字格式变化时整行失败，不返回索引为零的猜测性 pane/window。
#[test]
fn rejects_incomplete_or_invalid_index_rows() {
    let incomplete = "@3\u{1f}4\u{1f}editor";
    assert!(parse_window(incomplete.as_bytes(), &target()).is_err());

    let invalid_index = [
        "@3",
        "x",
        "editor",
        "1",
        "2",
        "132",
        "44",
        "main-vertical",
        "*",
        "/work",
    ]
    .join(&FIELD_SEPARATOR.to_string());
    assert!(parse_window(invalid_index.as_bytes(), &target()).is_err());
}

/// `target_ref` 的旧基址后追加明确的 `|L:` 或 `|S:`，相同 host 不混淆不同 server。
#[test]
fn target_reference_includes_socket_kind_and_identity() {
    let mut local_socket = target();
    local_socket.socket_name = Some("build".to_owned());
    local_socket.profile_id = Some("work".to_owned());
    let mut path_socket = target();
    path_socket.socket_path = Some("/tmp/tmux-kerminal.sock".to_owned());

    assert_eq!(target_reference(&target()), "local");
    assert_eq!(target_reference(&local_socket), "local:work|L:build");
    assert_eq!(
        target_reference(&path_socket),
        "local|S:/tmp/tmux-kerminal.sock"
    );
}

/// capability status 只接受 tmux 自报版本；执行端连接错误在 probe 调用处保持 Err。
#[test]
fn capability_status_requires_successful_tmux_version() {
    let available =
        capability_from_output(&command_result("tmux 3.4\n", "", Some(0), false), &target());
    assert!(available.available);
    assert_eq!(available.version.as_deref(), Some("tmux 3.4"));
    assert_eq!(available.target_ref, "local");

    let unavailable = capability_from_output(
        &command_result("", "tmux: command not found\n", Some(127), false),
        &target(),
    );
    assert!(!unavailable.available);
    assert_eq!(unavailable.version, None);
    assert!(unavailable.reason.is_some());
}

/// kill 返回 true 仅代表命令成功退出；明确不存在或无 server 才返回 false。
/// @author kongweiguang
#[test]
fn kill_result_distinguishes_confirmed_kill_from_absence() {
    assert!(classify_kill_result(&command_result("", "", Some(0), false)).unwrap());
    assert!(
        !classify_kill_result(&command_result(
            "",
            "can't find session: missing\n",
            Some(1),
            false,
        ))
        .unwrap()
    );
    assert!(
        !classify_kill_result(&command_result(
            "",
            "no server running on /tmp/tmux-test/default\n",
            Some(1),
            false,
        ))
        .unwrap()
    );
}

/// permission、未知退出码和被截断的诊断不能被幂等缺失规则误吞。
#[test]
fn kill_result_preserves_permission_and_unknown_failures() {
    let denied = classify_kill_result(&command_result(
        "",
        "failed to connect to server: Permission denied\n",
        Some(1),
        false,
    ))
    .expect_err("permission errors must stay visible");
    assert_eq!(denied.public_code(), PublicErrorCode::RemoteExit);

    let unknown = classify_kill_result(&command_result("", "", None, false))
        .expect_err("unknown exit status must stay visible");
    assert_eq!(unknown.public_code(), PublicErrorCode::RemoteExit);

    assert!(
        classify_kill_result(&command_result(
            "",
            "no server running\nadditional output",
            Some(1),
            true,
        ))
        .is_err()
    );
}

/// no-server 分类只吞确定不存在的 socket 错误，拒绝把 permission denied 归为空 server。
#[test]
fn no_server_classifier_does_not_hide_permission_errors() {
    assert!(no_server_running(
        b"failed to connect to server: Connection refused"
    ));
    assert!(no_server_running(
        b"failed to connect to server: No such file or directory"
    ));
    assert!(!no_server_running(
        b"failed to connect to server: Permission denied"
    ));
    assert!(!no_server_running(b"unexpected server error"));
}

/// 只把 tmux 3.2a 的明确缺失默认 socket 诊断视为空 server，不吞同前缀的权限、拒绝或未知错误。
///
/// @author kongweiguang
#[test]
fn no_server_classifier_recognizes_tmux_missing_default_socket() {
    assert!(no_server_running(
        b"error connecting to /tmp/tmux-1000/default (No such file or directory)"
    ));
    assert!(no_server_running(
        b"error connecting to /tmp/tmux-1000/default (No such file or directory)\n"
    ));
    assert!(!no_server_running(
        b"error connecting to /tmp/tmux-1000/default (Permission denied)"
    ));
    assert!(!no_server_running(
        b"error connecting to /tmp/tmux-1000/default (Connection refused)"
    ));
    assert!(!no_server_running(
        b"error connecting to /tmp/tmux-1000/default (unexpected error)"
    ));
}
