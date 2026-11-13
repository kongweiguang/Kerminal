//! Agent 连续性验收夹具的命令构造与 pane 回收存在性回归。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_application::{
    PublicErrorCode,
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionTarget, TerminalSize},
    workspace::TerminalLayoutNode,
};

use crate::root::workspace_runtime::WorkspaceRuntime;

use super::*;

/// 构造一个与真实 profile resolver 同形的本地 shell 描述，避免测试依赖宿主 PATH。
///
/// @author kongweiguang
fn local_shell(program: &str, args: &[&str]) -> LocalSessionTarget {
    LocalSessionTarget {
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
        working_directory: None,
        environment: BTreeMap::new(),
    }
}

/// 隔离的默认 profile：pwsh 7 且显式关闭 profile，外层只由 launch.rs 追加 `-Command`。
///
/// @author kongweiguang
fn isolated_pwsh() -> LocalSessionTarget {
    local_shell("pwsh.exe", &["-NoLogo", "-NoProfile"])
}

/// 连续性命令必须是可审计的内联脚本文本：不生成脚本文件、不带执行策略开关、不做编码转换。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_is_an_inline_pwsh7_script_without_policy_bypass() {
    let stage = PathBuf::from(r"C:\artifacts\agent continuity\trigger.stage");
    let fixture = AgentAcceptanceStagedFixture::new(stage);
    let marker = "rr-agent-123456789abc";

    let command = acceptance_staged_agent_command(&isolated_pwsh(), marker, &fixture)
        .expect("isolated pwsh profile should build a staged command");

    assert!(command.contains("$PSVersionTable.PSVersion.Major"));
    assert!(command.contains(&format!("{marker}-psversion:")));
    assert!(command.contains(&format!("{marker}-ready")));
    assert!(command.contains("$StageFile = 'C:\\artifacts\\agent continuity\\trigger.stage'"));
    assert!(command.contains("Start-Sleep -Milliseconds 50"));
    assert!(!command.contains("__MARKER__"));
    assert!(!command.contains("__STAGE__"));
    assert!(!command.contains("-ExecutionPolicy"));
    assert!(!command.contains("-EncodedCommand"));
    assert!(!command.contains("-File"));
    assert!(!command.contains("powershell.exe"));
    assert!(!command.contains("Start-Sleep -Seconds 180"));
}

/// stage 路径先由验收方放在独占运行目录中；PowerShell 单引号字面量要原样保留空格、`$`、`%`，
/// 单引号必须按既有规则写成两个单引号，避免路径被二次展开。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_quotes_spaces_dollar_percent_and_single_quotes() {
    let stage = PathBuf::from(r"C:\artifacts\a b\$env%PATH%\'quoted'\trigger.stage");
    let fixture = AgentAcceptanceStagedFixture::new(stage);

    let command =
        acceptance_staged_agent_command(&isolated_pwsh(), "rr-agent-123456789abc", &fixture)
            .expect("PowerShell literal quoting should accept the isolated stage path");

    assert!(
        command
            .contains("$StageFile = 'C:\\artifacts\\a b\\$env%PATH%\\''quoted''\\trigger.stage'")
    );
}

/// 控制字符或非本轮 marker 不能进入脚本文本；无效输入必须在保存 launcher 前以 Invalid 失败。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_rejects_unsafe_paths_and_markers() {
    let shell = isolated_pwsh();
    let unsafe_path = AgentAcceptanceStagedFixture::new(PathBuf::from(
        "C:\\artifacts\\line\nbreak\\trigger.stage",
    ));
    let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &unsafe_path)
        .expect_err("control characters in the stage path must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);

    let safe_path = AgentAcceptanceStagedFixture::new(PathBuf::from(r"C:\safe\trigger.stage"));
    for marker in [
        "rr-agent-safe",
        "rr-agent-123456789ABC",
        "rr-agent-line\nbreak",
        "rr-agent-123456789abc-extra",
    ] {
        let error = acceptance_staged_agent_command(&shell, marker, &safe_path)
            .expect_err("non-canonical marker must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    }
}

/// 外层 shell 只接受 pwsh 7；5.1 与 cmd 的引号/环境语义未验证，验收必须拒绝。
/// profile 必须显式写入 `-NoProfile`，不能依赖生产 launcher 事后补参数，否则保存的配置
/// 在其他启动路径中仍可能加载用户 profile。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_requires_pwsh7_outer_shell() {
    let fixture = AgentAcceptanceStagedFixture::new(PathBuf::from(r"C:\safe\trigger.stage"));

    for shell in [
        local_shell("powershell.exe", &["-NoLogo", "-NoProfile"]),
        local_shell("cmd.exe", &["/d"]),
        local_shell("pwsh.exe", &["-NoLogo"]),
        local_shell("pwsh.exe", &["-NoProfileLoadTime"]),
    ] {
        let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &fixture)
            .expect_err("unsupported outer shell or missing -NoProfile must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
    }

    let accepted = acceptance_staged_agent_command(
        &local_shell("pwsh.exe", &["-NoLogo", "-NoProfile"]),
        "rr-agent-123456789abc",
        &fixture,
    );
    assert!(
        accepted.is_ok(),
        "pwsh 7 profile with explicit -NoProfile should remain supported"
    );
}

/// profile 已预置命令模式时，launch.rs 追加的 `-Command` 会破坏脚本语义，应在验收配置阶段失败。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_rejects_existing_profile_command_modes() {
    let fixture = AgentAcceptanceStagedFixture::new(PathBuf::from(r"C:\safe\trigger.stage"));

    for argument in ["-Command", "-File", "-EncodedCommand", "-C", "-E"] {
        let shell = local_shell("pwsh.exe", &["-NoProfile", argument]);
        let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &fixture)
            .expect_err("PowerShell command mode must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    }

    for argument in ["/c", "/k"] {
        let shell = local_shell("cmd.exe", &[argument]);
        let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &fixture)
            .expect_err("cmd command mode must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    }
}

/// 执行策略、窗口/工作目录和未知启动脚本参数都不属于纯交互允许列表，必须拒绝；
/// 这样 fixture 不会借验收默认 profile 绕过用户安全策略或执行额外脚本。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_rejects_policy_bypasses_and_unknown_profile_arguments() {
    let fixture = AgentAcceptanceStagedFixture::new(PathBuf::from(r"C:\safe\trigger.stage"));

    for args in [
        vec!["-NoProfile", "-ExecutionPolicy"],
        vec!["-NoProfile", "-ExecutionPolicy", "Bypass"],
        vec!["-NoProfile", "-WindowStyle", "Hidden"],
        vec!["-NoProfile", "-WorkingDirectory", r"C:\artifacts"],
        vec!["-NoProfile", "-NoExit:false"],
    ] {
        let shell = local_shell("pwsh.exe", &args);
        let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &fixture)
            .expect_err("unsafe outer profile arguments must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
    }
}

/// PowerShell 7 把 U+2018–U+201B 当作单引号终止符，而既有 helper 只转义 ASCII 单引号；
/// 验收命令必须在保存 launcher 前拒绝这些路径，避免路径后半段被解释成脚本代码。
///
/// @author kongweiguang
#[test]
fn staged_fixture_command_rejects_powershell_smart_single_quotes() {
    let shell = isolated_pwsh();
    for quote in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
        let fixture = AgentAcceptanceStagedFixture::new(PathBuf::from(format!(
            "C:\\artifacts\\a{quote}b\\trigger.stage"
        )));
        let error = acceptance_staged_agent_command(&shell, "rr-agent-123456789abc", &fixture)
            .expect_err("PowerShell smart single quote must be rejected");
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    }
}

/// 既有生命周期夹具仍保留单行 marker + 睡眠命令，连续性改动不能改变原退出闭环。
///
/// @author kongweiguang
#[test]
fn exiting_fixture_command_keeps_the_single_stage_marker() {
    let marker = "rr-agent-abcdef123456";
    let powershell = acceptance_agent_command(&local_shell("pwsh.exe", &[]), marker)
        .expect("PowerShell lifecycle fixture");
    assert!(powershell.contains(marker));
    assert!(powershell.contains("Start-Sleep"));
    assert!(!powershell.contains("powershell.exe"));

    let cmd = acceptance_agent_command(&local_shell("cmd.exe", &[]), marker)
        .expect("cmd lifecycle fixture");
    assert!(cmd.contains(marker));
    assert!(cmd.contains("timeout /t 8"));
}

/// 构造与真实 v3 快照同形的单 Tab 单 pane 工作区，供存在性判据直接读取已提交布局。
///
/// @author kongweiguang
fn workspace_snapshot(tab_id: TabId, pane_id: PaneId) -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id,
            tab_group_id: None,
            title: "Local".to_owned(),
            machine_id: "local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![WorkspaceSessionPane {
            id: pane_id,
            title: "shell".to_owned(),
            machine_id: "local".to_owned(),
            mode: WorkspacePaneMode::Local,
            target: Some(WorkspaceTargetRef::Local { profile_id: None }),
            remote_host_id: None,
            container_id: None,
            profile_id: None,
            shell: None,
            args: None,
            cwd: None,
            current_cwd: None,
            env: None,
            remote_command: None,
            tmux_binding: None,
            prompt: String::new(),
            status: WorkspaceMachineStatus::Online,
            latency_ms: None,
            local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
            output_history: None,
        }],
        tab_groups: BTreeMap::new(),
    }
}

/// 构造一个真实绑定到 pane 的本地运行态；只用于验证存在性判据，不创建会话或窗口。
///
/// @author kongweiguang
fn runtime_with_ready_pane(
    tab_id: &TabId,
    pane_id: &PaneId,
    session_id: &SessionId,
) -> WorkspaceRuntime {
    let mut runtime =
        WorkspaceRuntime::from_snapshot(&workspace_snapshot(tab_id.clone(), pane_id.clone()))
            .expect("valid workspace snapshot");
    runtime
        .install_session(
            pane_id,
            session_id.clone(),
            SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh.exe"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            TerminalSize::default(),
        )
        .expect("bind session to pane");
    runtime
}

/// 生产关闭路径先提交布局候选、后完成 close 回执：布局已不再引用 A2 时，运行态 pane 与
/// session 映射可能仍持有它。判据必须把这种“布局移除”与“资源回收”区分开，否则会在 close
/// 完成前把 Closing pane 当成已移除。
///
/// @author kongweiguang
#[test]
fn layout_removal_without_runtime_release_is_not_a_removed_pane() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let runtime = runtime_with_ready_pane(&tab_id, &pane_id, &session_id);
    let layout_removed = workspace_snapshot(tab_id.clone(), PaneId::new());

    let presence = continuity_pane_presence(
        Some(&layout_removed),
        &runtime,
        &tab_id,
        &pane_id,
        &session_id,
    );
    assert!(presence.pane_present);
    assert!(presence.session_bound);
    assert!(!presence.layout_present);
    assert!(!presence.fully_removed());
}

/// 运行态 pane 已移除但 session 映射或布局仍引用它时同样不算回收；判据必须同时检查三侧，
/// 不能只看 `pane()` 是否为空。
///
/// @author kongweiguang
#[test]
fn any_remaining_runtime_or_layout_reference_blocks_removal() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let runtime = runtime_with_ready_pane(&tab_id, &pane_id, &session_id);
    let committed = workspace_snapshot(tab_id.clone(), pane_id.clone());

    let presence =
        continuity_pane_presence(Some(&committed), &runtime, &tab_id, &pane_id, &session_id);
    assert!(!presence.fully_removed());
    assert!(presence.layout_present);

    let empty_runtime = WorkspaceRuntime::default();
    let stale_layout = continuity_pane_presence(
        Some(&committed),
        &empty_runtime,
        &tab_id,
        &pane_id,
        &session_id,
    );
    assert!(!stale_layout.pane_present);
    assert!(!stale_layout.session_bound);
    assert!(stale_layout.layout_present);
    assert!(!stale_layout.fully_removed());
}

/// 三侧都释放后才算真实移除；缺少 controller 快照时运行态仍独立报告，避免把缺失快照
/// 当成布局已清空。
///
/// @author kongweiguang
#[test]
fn fully_removed_requires_runtime_mapping_and_layout_to_release() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let empty_runtime = WorkspaceRuntime::default();
    let released_layout = workspace_snapshot(tab_id.clone(), PaneId::new());

    let presence = continuity_pane_presence(
        Some(&released_layout),
        &empty_runtime,
        &tab_id,
        &pane_id,
        &session_id,
    );
    assert!(presence.fully_removed());

    let runtime = runtime_with_ready_pane(&tab_id, &pane_id, &session_id);
    let without_snapshot = continuity_pane_presence(None, &runtime, &tab_id, &pane_id, &session_id);
    assert!(without_snapshot.pane_present);
    assert!(!without_snapshot.layout_present);
    assert!(!without_snapshot.fully_removed());
}

/// 判据只按精确 Tab/Pane/Session 读取：同一 Tab 的另一个 pane 或另一 Tab 的同名布局不能被
/// 当成目标残留，避免验收因无关资源而失败。
///
/// @author kongweiguang
#[test]
fn presence_only_reports_the_exact_tab_pane_and_session() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let other_tab = TabId::new();
    let other_pane = PaneId::new();
    let other_session = SessionId::new();
    let runtime = runtime_with_ready_pane(&other_tab, &other_pane, &other_session);
    let layout = workspace_snapshot(other_tab, other_pane);

    let presence =
        continuity_pane_presence(Some(&layout), &runtime, &tab_id, &pane_id, &session_id);
    assert_eq!(
        presence,
        ContinuityPanePresence {
            pane_present: false,
            session_bound: false,
            layout_present: false,
        }
    );
    assert!(presence.fully_removed());
}

/// `Closing` 与 `CloseFailed` 都不是回收证据：关闭请求或失败回执不能等同资源已释放。
///
/// @author kongweiguang
#[test]
fn closing_and_close_failed_panes_are_still_present() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let mut runtime = runtime_with_ready_pane(&tab_id, &pane_id, &session_id);
    runtime.begin_close(&pane_id).expect("begin close");
    let layout = workspace_snapshot(tab_id.clone(), PaneId::new());

    let closing = continuity_pane_presence(Some(&layout), &runtime, &tab_id, &pane_id, &session_id);
    assert!(closing.pane_present);
    assert!(closing.session_bound);
    assert!(!closing.fully_removed());

    runtime
        .mark_close_failed(&pane_id, &session_id)
        .expect("mark close failed");
    let failed = continuity_pane_presence(Some(&layout), &runtime, &tab_id, &pane_id, &session_id);
    assert!(failed.pane_present);
    assert!(failed.session_bound);
    assert!(!failed.fully_removed());

    // 只有生产 close ACK 后的 remove_closed_session 才释放运行态与 session 映射。
    // CloseFailed 状态不允许直接移除，必须先经生产保留/重试路径回到可关闭状态。
    assert!(
        !runtime
            .remove_closed_session(&pane_id, &session_id)
            .expect("close failed pane still maps the session")
    );
    assert!(runtime.keep_close_failed_pane(&pane_id));
    runtime.begin_close(&pane_id).expect("retry close");
    assert!(
        runtime
            .remove_closed_session(&pane_id, &session_id)
            .expect("remove closed session")
    );
    let removed = continuity_pane_presence(Some(&layout), &runtime, &tab_id, &pane_id, &session_id);
    assert!(removed.fully_removed());
    assert!(runtime.pane(&pane_id).is_none());
}
