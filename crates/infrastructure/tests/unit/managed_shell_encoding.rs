//! 受管脚本的编码不得改变 shell 语义或把控制键直接写入终端。
//!
//! @author kongweiguang

use super::{RemoteShellDialect, encode_managed_shell_command};

/// 普通代理脚本应保持可读且避免将 shell 的引号和替换提前执行。
///
/// @author kongweiguang
#[test]
fn posix_single_line_keeps_text_readable_and_quoted() {
    let command = "export ALL_PROXY='socks5h://127.0.0.1:1080'; x='$(unexpected)'";
    let encoded = encode_managed_shell_command(command, RemoteShellDialect::Posix)
        .expect("POSIX command is supported");
    assert!(encoded.starts_with("eval '"));
    assert!(encoded.contains("socks5h://127.0.0.1:1080"));
    assert!(encoded.contains("$(unexpected)"));
    assert!(!encoded.contains("\\145\\170\\160"));
    assert!(!encoded.bytes().any(|byte| byte.is_ascii_control()));
}

/// 测试脚本同时包含 shell 引号、命令替换和反斜线，多行样本另覆盖实际 tab。
///
/// @author kongweiguang
fn posix_probe_commands() -> [&'static str; 3] {
    [
        "KERMINAL_VALUE='世界\\quoted $(printf wrong)' ; false",
        "KERMINAL_VALUE='世界\\quoted $(printf wrong)'\nKERMINAL_OTHER='a\tb'\nfalse",
        "KERMINAL_VALUE='世界\\quoted $(printf wrong)'\nKERMINAL_OTHER='a\u{85}b'\nfalse",
    ]
}

/// 在同一 shell 内检查变量和原退出码，区分原始字符与首层错误展开。
///
/// @author kongweiguang
fn posix_probe(command: &str) -> String {
    let encoded = encode_managed_shell_command(command, RemoteShellDialect::Posix)
        .expect("POSIX command is supported");
    assert!(!encoded.bytes().any(|byte| byte.is_ascii_control()));
    format!(
        "{encoded}; result=$?; printf '%s|%s\\n' \"$KERMINAL_VALUE\" \"${{KERMINAL_OTHER-unset}}\"; exit \"$result\""
    )
}

/// 真实 POSIX shell 验证单行和多行的 Unicode、引号、反斜线、替换与退出码，避免只测试编码形状。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn posix_shell_preserves_scope_text_and_exit_status() {
    use std::process::Command;

    for shell in ["bash", "zsh"] {
        if Command::new(shell).arg("--version").output().is_err() {
            continue;
        }
        for (index, command) in posix_probe_commands().into_iter().enumerate() {
            let probe = posix_probe(command);
            let output = Command::new(shell)
                .args(["-c", &probe])
                .output()
                .expect("run isolated shell");
            assert_eq!(output.status.code(), Some(1), "{shell}");
            assert_eq!(
                String::from_utf8(output.stdout).expect("UTF-8 output"),
                match index {
                    0 => "世界\\quoted $(printf wrong)|unset\n",
                    1 => "世界\\quoted $(printf wrong)|a\tb\n",
                    _ => "世界\\quoted $(printf wrong)|a\u{85}b\n",
                },
                "{shell}"
            );
        }
    }
}

/// Windows 验收显式选择 WSL，在后台执行生产编码器生成的脚本，不依赖用户的交互终端。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
#[ignore = "requires an isolated WSL distribution with bash and zsh"]
fn posix_encoding_runs_in_wsl_shells() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use base64::Engine as _;
    for shell in ["bash", "zsh"] {
        for (index, command) in posix_probe_commands().into_iter().enumerate() {
            let probe = posix_probe(command);
            let payload = base64::engine::general_purpose::STANDARD.encode(probe.as_bytes());
            let launch = format!("echo {payload} | base64 -d | {shell}");
            let output = Command::new("wsl.exe")
                .creation_flags(0x0800_0000)
                .args(["--exec", "sh", "-c", &launch])
                .output()
                .expect("run isolated WSL shell");
            assert_eq!(output.status.code(), Some(1), "{shell}");
            assert_eq!(
                String::from_utf8(output.stdout).expect("UTF-8 output"),
                match index {
                    0 => "世界\\quoted $(printf wrong)|unset\n",
                    1 => "世界\\quoted $(printf wrong)|a\tb\n",
                    _ => "世界\\quoted $(printf wrong)|a\u{85}b\n",
                },
                "{shell}"
            );
        }
    }
}

/// 使用隔离的真实 Bash PTY 执行超过 4096 字节的生产编码，确认长输入不丢失且不会输出八进制洪泛。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
#[ignore = "requires WSL with bash and python3"]
fn posix_encoding_runs_in_wsl_bash_pty() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use base64::Engine as _;
    let command = format!(
        "KERMINAL_VALUE='世界\\quoted $(printf wrong)'; : '{}'; false",
        "x".repeat(5000)
    );
    let encoded = encode_managed_shell_command(&command, RemoteShellDialect::Posix)
        .expect("POSIX command is supported");
    assert!(encoded.len() > 4096);
    let harness = r#"
import base64, os, pty, select, signal, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.environ['PS1'] = 'KERMINAL_PTY_READY> '
    os.execvp('bash', ['bash', '--noprofile', '--norc', '-i'])
def read_until(marker):
    data = bytearray()
    deadline = time.monotonic() + 15
    while marker not in data and time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
        if ready:
            data.extend(os.read(fd, 65536))
    return bytes(data)
try:
    if b'KERMINAL_PTY_READY> ' not in read_until(b'KERMINAL_PTY_READY> '):
        raise RuntimeError('Bash prompt did not arrive')
    tail = b'; result=$?; printf "\\036KERMINAL_DONE:%s:%s\\037" "$result" "$KERMINAL_VALUE"'
    os.write(fd, base64.b64decode(sys.argv[1]) + tail + b'\r')
    expected = '\x1eKERMINAL_DONE:1:世界\\quoted $(printf wrong)\x1f'.encode()
    output = read_until(b'\x1f')
    if expected not in output or b'\\145\\170\\160' in output:
        raise RuntimeError('long command changed scope, status or output encoding')
    print('isolated Bash PTY passed')
finally:
    os.close(fd)
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    try: os.waitpid(pid, 0)
    except ChildProcessError: pass
"#;
    let engine = &base64::engine::general_purpose::STANDARD;
    let harness = engine.encode(harness.as_bytes());
    let command = engine.encode(encoded.as_bytes());
    let launch = format!("echo {harness} | base64 -d | python3 - {command}");
    let output = Command::new("wsl.exe")
        .creation_flags(0x0800_0000)
        .args(["--exec", "sh", "-c", &launch])
        .output()
        .expect("run isolated WSL PTY");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("isolated Bash PTY passed"));
}
