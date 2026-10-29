//! Operations 到 Root 文件面板的目标转换测试。
//!
//! @author kongweiguang

use kerminal_application::workspace::WorkspaceTargetRef;
use kerminal_application::workspace::WorkspaceTmuxBinding;
use kerminal_domain::{
    connections::RemoteShellDialect,
    files::{ContainerRuntime, FileEndpoint, FilePathError},
    identity::HostId,
    operations::{ContainerTarget, TmuxTarget},
    sessions::{LocalSessionTarget, SessionTarget, SshSessionTarget},
};

use super::{
    WorkspaceTerminalOpenKind, container_exec_intent, container_file_endpoint,
    is_host_file_endpoint, tmux_attach_command, tmux_binding_matches_request,
    tmux_binding_target_ref, tmux_detach_key_events, tmux_local_shell_is_wsl,
    tmux_target_matches_terminal, valid_quickref_text,
};
use crate::features::operations::TmuxAttachTarget;

/// 容器文件路由保留显式 host/runtime/container，并仅规范化 POSIX 路径。
#[test]
fn container_endpoint_preserves_explicit_target_and_normalizes_path() {
    let target = ContainerTarget {
        host_id: Some(HostId::parse("host-a").expect("valid host id")),
        runtime: ContainerRuntime::Podman,
        container_id: "container-1".to_owned(),
    };

    let endpoint = container_file_endpoint(&target, "/workspace//./src/main.rs")
        .expect("valid container path");

    assert_eq!(
        endpoint,
        FileEndpoint::Container {
            host_id: target.host_id,
            runtime: ContainerRuntime::Podman,
            container_id: "container-1".to_owned(),
            path: kerminal_domain::files::RemotePath::parse("/workspace/src/main.rs")
                .expect("normalized path"),
        }
    );
}
/// 空路径与控制字符必须在进入 FilesPane 前拒绝，避免无效路径被当成成功浏览。
#[test]
fn container_endpoint_rejects_empty_and_control_paths() {
    let target = ContainerTarget {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "local-container".to_owned(),
    };

    assert_eq!(
        container_file_endpoint(&target, ""),
        Err(FilePathError::Empty)
    );
    assert_eq!(
        container_file_endpoint(&target, "/tmp/\u{7}/file"),
        Err(FilePathError::ControlCharacter)
    );
}

/// exec pane 保留完整 argv 和明确容器目标，并始终从工作区持久化恢复中排除。
#[test]
fn container_exec_intent_is_explicit_and_transient() {
    let target = ContainerTarget {
        host_id: Some(HostId::parse("host-a").expect("valid host id")),
        runtime: ContainerRuntime::Podman,
        container_id: "container-1".to_owned(),
    };
    let command = vec!["sh".to_owned(), "-lc".to_owned(), "printf hello".to_owned()];

    let intent = container_exec_intent(&target, &command);

    assert!(intent.transient);
    assert_eq!(
        intent.pane.mode,
        kerminal_application::workspace::WorkspacePaneMode::Container
    );
    assert_eq!(intent.pane.shell.as_deref(), Some("sh"));
    assert_eq!(intent.pane.args.as_deref(), Some(&command[1..]));
    assert_eq!(intent.pane.remote_host_id, target.host_id);
    assert_eq!(intent.pane.container_id.as_deref(), Some("container-1"));
    assert!(matches!(
        intent.kind,
        WorkspaceTerminalOpenKind::ContainerExec {
            target: actual_target,
            command: actual_command,
        } if actual_target == target && actual_command == command
    ));
}

/// tmux 请求必须同时匹配当前 SSH transport 和持久化 host 引用，防止面板选择与终端焦点错配。
///
/// @author kongweiguang
#[test]
fn tmux_target_requires_exact_focused_ssh_host() {
    let host = HostId::parse("host-a").expect("valid host id");
    let target = TmuxTarget {
        host_id: Some(host.clone()),
        ..TmuxTarget::default()
    };
    let actual = SessionTarget::Ssh(SshSessionTarget {
        host_id: host.clone(),
        working_directory: None,
        command: None,
    });
    let persisted = WorkspaceTargetRef::Ssh {
        host_id: host.clone(),
    };

    assert!(tmux_target_matches_terminal(&target, &actual, &persisted));
    assert!(!tmux_target_matches_terminal(
        &TmuxTarget {
            host_id: Some(HostId::parse("host-b").expect("valid host id")),
            ..TmuxTarget::default()
        },
        &actual,
        &persisted,
    ));
    assert!(!tmux_target_matches_terminal(
        &TmuxTarget::default(),
        &actual,
        &persisted,
    ));
}

/// 本机 tmux 只匹配同一 Local transport 与持久化 profile；缺省值不能替代显式 profile。
///
/// @author kongweiguang
#[test]
fn tmux_target_accepts_only_exact_focused_local_profile() {
    let target = TmuxTarget {
        profile_id: Some("profile-local".to_owned()),
        ..TmuxTarget::default()
    };
    let actual = SessionTarget::Local(LocalSessionTarget {
        program: "pwsh.exe".into(),
        args: Vec::new(),
        working_directory: None,
        environment: Default::default(),
    });
    let persisted = WorkspaceTargetRef::Local {
        profile_id: Some("profile-local".to_owned()),
    };

    assert!(tmux_target_matches_terminal(&target, &actual, &persisted));
    assert!(!tmux_target_matches_terminal(
        &TmuxTarget::default(),
        &actual,
        &persisted,
    ));
    assert!(!tmux_target_matches_terminal(
        &TmuxTarget {
            profile_id: Some("profile-other".to_owned()),
            ..TmuxTarget::default()
        },
        &actual,
        &persisted,
    ));
    assert!(!tmux_target_matches_terminal(
        &target,
        &actual,
        &WorkspaceTargetRef::Local { profile_id: None },
    ));
}

/// Attach typed binding 必须拒绝本机/SSH 互换及 Local profile 变化，避免把命令送入新目标。
///
/// @author kongweiguang
#[test]
fn tmux_attach_binding_rejects_cross_target_and_retargeted_profile() {
    let local = TmuxAttachTarget::Local {
        profile_id: Some("profile-local".to_owned()),
    };
    let local_tmux = local.to_tmux_target();

    assert_eq!(local.host_id(), None);
    assert!(local.matches_tmux_target(&local_tmux));
    assert!(!local.matches_tmux_target(&TmuxTarget::default()));
    assert!(!local.matches_tmux_target(&TmuxTarget {
        host_id: Some(HostId::parse("host-a").expect("valid host id")),
        ..TmuxTarget::default()
    }));

    let ssh = TmuxAttachTarget::Ssh {
        host_id: HostId::parse("host-a").expect("valid host id"),
    };
    assert!(!ssh.matches_tmux_target(&local_tmux));
}

/// Local target 使用宿主 tmux executor，因此直接启动 WSL 的 pane 不能承载宿主会话命令。
///
/// @author kongweiguang
#[test]
fn tmux_local_binding_does_not_claim_wsl_execution() {
    assert!(tmux_local_shell_is_wsl(std::ffi::OsStr::new("wsl.exe")));
    assert!(tmux_local_shell_is_wsl(std::ffi::OsStr::new(
        r"C:\Windows\System32\wsl.exe"
    )));
    assert!(!tmux_local_shell_is_wsl(std::ffi::OsStr::new("pwsh.exe")));
    assert!(!tmux_local_shell_is_wsl(std::ffi::OsStr::new(
        "powershell.exe"
    )));
}

/// POSIX attach 只接受服务返回的 `$N` ID，并分别编码 binary、socket 与 ID 参数。
///
/// @author kongweiguang
#[test]
fn tmux_attach_command_preserves_posix_encoding() {
    let target = TmuxTarget {
        host_id: Some(HostId::parse("host-a").expect("valid host id")),
        socket_name: Some("shared socket".to_owned()),
        tmux_path: Some("/opt/kerminal's tmux".to_owned()),
        ..TmuxTarget::default()
    };

    assert_eq!(
        tmux_attach_command(&target, "$17", RemoteShellDialect::Posix).as_deref(),
        Some("'/opt/kerminal'\\''s tmux' '-L' 'shared socket' 'attach-session' '-t' '$17'")
    );
    assert!(tmux_attach_command(&target, "'$17; rm -rf /", RemoteShellDialect::Posix).is_none());
}

/// PowerShell 必须通过调用运算符执行带空格/单引号的路径，并让 `$0` 保持字面值。
///
/// @author kongweiguang
#[test]
fn tmux_attach_command_encodes_powershell_and_keeps_session_id_literal() {
    let target = TmuxTarget {
        tmux_path: Some(r"C:\Program Files\Kerminal's\tmux.exe".to_owned()),
        ..TmuxTarget::default()
    };

    assert_eq!(
        tmux_attach_command(&target, "$0", RemoteShellDialect::PowerShell).as_deref(),
        Some("& 'C:\\Program Files\\Kerminal''s\\tmux.exe' 'attach-session' '-t' '$0'")
    );
}

/// 未知方言（包括无法区分的 cmd）不可安全引用，必须在排队前拒绝。
///
/// @author kongweiguang
#[test]
fn tmux_attach_rejects_unknown_shell_dialect() {
    assert!(
        tmux_attach_command(&TmuxTarget::default(), "$0", RemoteShellDialect::Unknown).is_none()
    );
}

/// 双重 socket 配置不能生成 attach 命令，避免把会话发到含混的 tmux server。
///
/// @author kongweiguang
#[test]
fn tmux_attach_rejects_conflicting_socket_target() {
    let target = TmuxTarget {
        socket_name: Some("named".to_owned()),
        socket_path: Some("/tmp/socket".to_owned()),
        ..TmuxTarget::default()
    };

    assert!(tmux_attach_command(&target, "$1", RemoteShellDialect::Posix).is_none());
    assert_eq!(tmux_binding_target_ref(&target), None);
}
/// Compose 文件桥接只接受 host 文件系统，容器路径必须保留在 Container 专用入口。
#[test]
fn compose_config_accepts_only_local_or_remote_endpoints() {
    assert!(is_host_file_endpoint(&FileEndpoint::Local {
        path: std::path::PathBuf::from("C:/compose.yaml"),
    }));
    assert!(is_host_file_endpoint(&FileEndpoint::Remote {
        host_id: HostId::parse("host-a").expect("valid host id"),
        path: kerminal_domain::files::RemotePath::parse("/srv/compose.yaml")
            .expect("valid remote path"),
    }));
    assert!(!is_host_file_endpoint(&FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "local-container".to_owned(),
        path: kerminal_domain::files::RemotePath::parse("/compose.yaml")
            .expect("valid container path"),
    }));
}

/// tmux quickref 只发送同一 attach 保存的 session、host/profile 和 socket 目标。
#[test]
fn tmux_quickref_requires_exact_persisted_target_and_session() {
    let target = TmuxTarget {
        host_id: Some(HostId::parse("host-a").expect("valid host id")),
        socket_name: Some("kerminal".to_owned()),
        ..TmuxTarget::default()
    };
    let binding = WorkspaceTmuxBinding {
        target_ref: "ssh:host-a|L:kerminal".to_owned(),
        session_id: "$1".to_owned(),
        session_name: "work".to_owned(),
        attached_at: "2026-10-17T00:00:00Z".to_owned(),
        socket_name: Some("kerminal".to_owned()),
        socket_path: None,
    };

    assert!(tmux_binding_matches_request(&binding, &target, "$1"));
    assert!(!tmux_binding_matches_request(&binding, &target, "$2"));
    assert!(!tmux_binding_matches_request(
        &binding,
        &TmuxTarget {
            host_id: Some(HostId::parse("host-b").expect("valid host id")),
            socket_name: Some("kerminal".to_owned()),
            ..TmuxTarget::default()
        },
        "$1"
    ));
}

/// Quickref payload 校验禁止多行 shell 注入，并将快捷键限制在 Ctrl-B 加一个可见按键。
#[test]
fn tmux_quickref_payload_is_single_action() {
    assert!(valid_quickref_text(
        "tmux ls",
        super::TmuxQuickrefKind::Command
    ));
    assert!(!valid_quickref_text(
        "tmux ls\nrm -rf /",
        super::TmuxQuickrefKind::Command
    ));
    assert!(valid_quickref_text(
        "\u{2}d",
        super::TmuxQuickrefKind::Shortcut
    ));
    assert!(!valid_quickref_text("d", super::TmuxQuickrefKind::Shortcut));
    assert!(!valid_quickref_text(
        "\u{2}dd",
        super::TmuxQuickrefKind::Shortcut
    ));
}

/// 验证快捷项最终能生成真实终端字节；裸 Character 曾在 legacy 模式静默丢失，只有前缀实际写入。
/// 字符事件也必须保留 Kitty 编码入口，并且绝不夹带提交换行。
///
/// @author kongweiguang
#[test]
fn tmux_shortcut_character_reaches_legacy_and_kitty_encoders() {
    use kerminal_terminal::{TerminalModes, input::encode_input};
    for key in ['d', '%', '"'] {
        let event = super::tmux_shortcut_key_event(key);
        assert_eq!(
            encode_input(&event, TerminalModes::default()),
            Some(key.to_string().into_bytes())
        );
        let kitty = TerminalModes {
            kitty_keyboard_flags: 0b01000,
            ..TerminalModes::default()
        };
        let encoded = encode_input(&event, kitty).expect("Kitty character must encode");
        assert!(encoded.starts_with(b"\x1b["));
        assert!(!encoded.contains(&b'\r'));
        assert!(!encoded.contains(&b'\n'));
    }
}

/// 分离动作编码后必须逐字节等于 main 发送的 Ctrl-B+d，且不会变成 detach-client 命令。
///
/// @author kongweiguang
#[test]
fn tmux_detach_sequence_encodes_ctrl_b_then_d() {
    use kerminal_terminal::{TerminalModes, input::encode_input};

    let events = tmux_detach_key_events();
    assert_eq!(
        encode_input(&events[0], TerminalModes::default()),
        Some(vec![0x02])
    );
    assert_eq!(
        encode_input(&events[1], TerminalModes::default()),
        Some(b"d".to_vec())
    );
}
