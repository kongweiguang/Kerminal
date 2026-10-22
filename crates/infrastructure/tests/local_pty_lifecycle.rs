//! 使用平台原生 shell 验证 PTY 字节链路、resize、退出、重连与关闭收尾。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshotRequest, SessionFilter, SessionService,
    ShellCommandOutcome, ShellCommandRequest, ShellCommandUnknownReason,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::local_pty::LocalPtySessionTransportFactory;
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::runtime::Handle;

/// 选择平台交互 shell；Windows 禁用 AutoRun，避免隔离测试执行用户注册表中的启动脚本。
///
/// @author kongweiguang
fn local_shell() -> LocalSessionTarget {
    #[cfg(windows)]
    let (program, args) = ("cmd.exe", vec!["/D", "/Q"]);
    #[cfg(unix)]
    let (program, args) = ("/bin/sh", vec!["-i"]);

    LocalSessionTarget {
        program: OsString::from(program),
        args: args.into_iter().map(OsString::from).collect(),
        working_directory: None,
        environment: BTreeMap::new(),
    }
}

/// 为每个测试创建隔离的真实 local-PTY service，避免共享会话表导致时序相互影响。
///
/// @author kongweiguang
fn service() -> Arc<dyn SessionService> {
    Arc::new(SessionManager::new(
        Arc::new(LocalPtySessionTransportFactory::new()),
        Handle::current(),
        SessionRuntimeConfig::default(),
    ))
}

/// 轮询 bounded engine snapshot 直到标记真实输出或在有限时间内报告失败。
///
/// @author kongweiguang
async fn wait_for_text(
    service: &dyn SessionService,
    session_id: SessionId,
    marker: &str,
) -> String {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = service
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(16 * 1024),
                    },
                )
                .await
                .expect("session snapshot");
            if snapshot.text.contains(marker) {
                return snapshot.text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("terminal marker appeared before deadline")
}

/// 在有限时间内从真实终端输出中读取后代 PID；返回值让调用测试先清理会话再断言。
///
/// @author kongweiguang
async fn wait_for_descendant_pid(
    service: &dyn SessionService,
    session_id: &SessionId,
) -> Option<u32> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = service
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(16 * 1024),
                    },
                )
                .await
                .ok()?;
            let Some(marker) = snapshot.text.split("KERMINAL_DESCENDANT_PID=").nth(1) else {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            };
            let digits: String = marker
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let Ok(pid) = digits.parse() {
                return Some(pid);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok()
    .flatten()
}

/// 检查指定子进程仍可被系统枚举；调用方轮询到退出后才通过测试。
///
/// @author kongweiguang
#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    use std::process::Command;
    let pid = pid.to_string();
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", pid.as_str()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    !String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('Z')
}

/// 关闭 PTY 后确认 shell 的派生进程也退出，而不是只回收直接 child handle。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn local_pty_close_terminates_descendant_process_group() {
    let service = service();
    let info = service.create(CreateSessionRequest {
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from("sleep 60 & child=$!; printf 'KERMINAL_DESCENDANT_PID=%s\\n' \"$child\"; wait")],
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Mcp,
        size: TerminalSize::new(80, 24, 0, 0).expect("valid grid"),
    }).await.expect("spawn shell with descendant");
    let child_pid = wait_for_descendant_pid(service.as_ref(), &info.session_id).await;
    service
        .close(info.session_id)
        .await
        .expect("close session process group");
    let Some(child_pid) = child_pid else {
        panic!("child PID was not emitted before close");
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while process_is_running(child_pid) {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("PTY close terminated descendant process");
}

/// ConPTY 关闭必须结束同一 Job Object 内由 PowerShell 派生的长时子进程；先持有句柄防止 PID 复用。
/// 探测禁用用户 profile 且无窗口运行，测试后代也不得打断开发者的桌面操作。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn conpty_close_terminates_descendant_process_job() {
    use std::process::Command;
    if Command::new("pwsh")
        .creation_flags(0x0800_0000)
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .status()
        .is_err()
    {
        return;
    }
    let service = service();
    let script = "$child = Start-Process -WindowStyle Hidden -FilePath pwsh -ArgumentList @('-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 60') -PassThru; [Console]::WriteLine('KERMINAL_DESCENDANT_PID=' + $child.Id); Start-Sleep -Seconds 60";
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh"),
                args: vec![
                    OsString::from("-NoLogo"),
                    OsString::from("-NoProfile"),
                    OsString::from("-Command"),
                    OsString::from(script),
                ],
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
        })
        .await
        .expect("spawn PowerShell with descendant");
    let child_pid = wait_for_descendant_pid(service.as_ref(), &info.session_id).await;
    let child_handle = child_pid.map(open_native_child_handle);
    service
        .close(info.session_id)
        .await
        .expect("close ConPTY job");
    let Some(_child_pid) = child_pid else {
        panic!("child PID was not emitted before close");
    };
    let child_handle = child_handle
        .expect("child PID was emitted before close")
        .expect("open descendant process handle before closing ConPTY");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let exited = wait_for_native_exit_code(&child_handle, deadline)
        .await
        .expect("observe descendant process handle after ConPTY close");
    assert!(
        exited.is_some(),
        "ConPTY close terminated descendant process"
    );
}

/// 真实启动 shell、写入命令、读取 PTY 输出、调整尺寸并等待 child 清理。
///
/// @author kongweiguang
#[tokio::test]
async fn local_pty_create_write_read_resize_close() {
    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(local_shell()),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(80, 24, 0, 0).expect("valid grid"),
        })
        .await
        .expect("spawn real local PTY");
    assert!(
        service
            .runtime_metadata(info.session_id.clone())
            .await
            .expect("runtime metadata")
            .process_id
            .is_some()
    );

    #[cfg(windows)]
    let command = b"echo KERMINAL_PTY_LIFECYCLE_PROBE\r".to_vec();
    #[cfg(unix)]
    let command = b"echo KERMINAL_PTY_LIFECYCLE_PROBE\r".to_vec();
    service
        .write(info.session_id.clone(), command)
        .await
        .expect("write shell input");
    let output = wait_for_text(
        service.as_ref(),
        info.session_id.clone(),
        "KERMINAL_PTY_LIFECYCLE_PROBE",
    )
    .await;
    assert!(output.contains("KERMINAL_PTY_LIFECYCLE_PROBE"));

    let resized = TerminalSize::new(100, 30, 1000, 600).expect("valid resized grid");
    service
        .resize(info.session_id.clone(), resized)
        .await
        .expect("resize native PTY");
    let sessions = service
        .list(SessionFilter::default())
        .await
        .expect("list sessions");
    assert_eq!(sessions[0].size, resized);

    tokio::time::timeout(
        Duration::from_secs(4),
        service.close(info.session_id.clone()),
    )
    .await
    .expect("close releases PTY master and joins its workers before the test deadline")
    .expect("kill and reap child and PTY workers");
    assert!(
        service
            .list(SessionFilter::default())
            .await
            .expect("list after close")
            .is_empty()
    );
    service
        .close_all()
        .await
        .expect("shutdown has no remaining session");
}

/// 为受管命令等待真实 OSC prompt ready，而不是通过测试直接注入 parser 事件。
///
/// @author kongweiguang
async fn wait_for_shell_prompt(service: &dyn SessionService, session_id: &SessionId) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let metadata = service
                .runtime_metadata(session_id.clone())
                .await
                .expect("runtime metadata");
            if metadata.shell_integration_observed && metadata.prompt_ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real shell integration prompt arrived");
}

/// 通过真实 PTY 比较子进程 PATH，并记录 PowerShell 启动身份；诊断只保留脱敏数值和布尔值。
///
/// @author kongweiguang
#[cfg(windows)]
async fn assert_pwsh_path_matches_expected(environment: BTreeMap<OsString, OsString>) {
    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh"),
                args: vec![OsString::from("-NoLogo"), OsString::from("-NoProfile")],
                working_directory: None,
                environment,
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
        })
        .await
        .unwrap_or_else(|error| {
            let mut source = std::error::Error::source(&error);
            let mut io_diagnostic = None;
            while let Some(cause) = source {
                if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
                    io_diagnostic = Some((io_error.kind(), io_error.raw_os_error()));
                    break;
                }
                source = cause.source();
            }
            let (kind, raw_os_error) = io_diagnostic
                .map(|(kind, raw_os_error)| (Some(kind), raw_os_error))
                .unwrap_or((None, None));
            panic!("start PowerShell PATH probe (io_kind={kind:?}, raw_os_error={raw_os_error:?})");
        });
    wait_for_shell_prompt(service.as_ref(), &info.session_id).await;

    let outcome = service
        .execute_shell_command(
            ShellCommandRequest {
                session_id: info.session_id.clone(),
                expected_ui_binding: None,
                command_id: "powershell-path-check".into(),
                command: powershell_path_probe_command(),
                timeout: Duration::from_secs(10),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let observation = wait_for_path_probe_result(service.as_ref(), info.session_id.clone()).await;
    service
        .close(info.session_id)
        .await
        .expect("close PowerShell PATH probe");

    assert!(matches!(
        outcome,
        Ok(ShellCommandOutcome::Finished {
            ref command_id,
            exit_code: 0,
        }) if command_id == "powershell-path-check"
    ));
    assert!(
        matches!(observation, PathProbeObservation::MatchTokenObserved { .. }),
        "PowerShell PATH probe result: {observation:?}"
    );
}

/// 按 PowerShell 官方启动规则计算完整期望 PATH，同时保留原始配置是否已有 PSHOME 前缀这一事实。
/// 版本字段为 Major、Minor、Patch/Build、Revision/哨兵；不输出任何路径内容。
/// 三段分别落在独立短行，避免宽度受限的 PTY 把 CSV 断开后无法解析。
///
/// @author kongweiguang
#[cfg(windows)]
fn powershell_path_probe_command() -> String {
    r#"
$configuredPath = [string]$env:KERMINAL_TEST_EXPECTED_PATH
$r = [string][Environment]::GetEnvironmentVariable('PATH', 'Process')
$p = [string]$env:PATH
$processBasename = [IO.Path]::GetFileName([Environment]::ProcessPath)
$processIsPwsh = [string]::Equals($processBasename, 'pwsh.exe', [StringComparison]::OrdinalIgnoreCase)
$psHome = [string]$PSHOME
$psHomePrefix = $psHome + [IO.Path]::PathSeparator
$configuredPathHasPsHomePrefix = $configuredPath.StartsWith($psHomePrefix, [StringComparison]::Ordinal)
$e = $configuredPath
if ($processIsPwsh -and -not $configuredPathHasPsHomePrefix) {
    $e = $psHomePrefix + $configuredPath
}
$ep = $e.Split([char[]]@(';'))
$rp = $r.Split([char[]]@(';'))
$pp = $p.Split([char[]]@(';'))
$n = [Math]::Min($ep.Length, $rp.Length)
$pre = 0
while (($pre -lt $n) -and ([string]::Equals($ep[$pre], $rp[$pre], [StringComparison]::OrdinalIgnoreCase))) { $pre++ }
$suf = 0
while (($suf -lt $n) -and ([string]::Equals($ep[$ep.Length - 1 - $suf], $rp[$rp.Length - 1 - $suf], [StringComparison]::OrdinalIgnoreCase))) { $suf++ }
$first = 0
for ($i = 0; $i -lt $n; $i++) { if (-not [string]::Equals($ep[$i], $rp[$i], [StringComparison]::OrdinalIgnoreCase)) { $first = $i + 1; break } }
$el = -1
$rl = -1
if ($first -gt 0) { $el = $ep[$first - 1].Length; $rl = $rp[$first - 1].Length }
$er = [string]::Equals($e, $r, [StringComparison]::Ordinal)
$rpEq = [string]::Equals($r, $p, [StringComparison]::Ordinal)
$erCi = [string]::Equals($e, $r, [StringComparison]::OrdinalIgnoreCase)
$firstSegment = if ($rp.Length -gt 0) { [string]$rp[0] } else { '' }
$trimChars = [char[]]@('\', '/')
$firstMatchesPsHome = [string]::Equals($firstSegment.TrimEnd($trimChars), $psHome.TrimEnd($trimChars), [StringComparison]::OrdinalIgnoreCase)
$dotnetStoreSegment = [IO.Path]::DirectorySeparatorChar + '.store' + [IO.Path]::DirectorySeparatorChar + 'powershell' + [IO.Path]::DirectorySeparatorChar
$psHomeIsDotnetTool = $psHome.IndexOf($dotnetStoreSegment, [StringComparison]::Ordinal) -gt 0
$version = $PSVersionTable.PSVersion
$versionPatchProperty = $version.PSObject.Properties['Patch']
$versionBuildProperty = $version.PSObject.Properties['Build']
$versionRevisionProperty = $version.PSObject.Properties['Revision']
$versionPatchOrBuild = -1
if ($null -ne $versionPatchProperty) {
    $versionPatchOrBuild = [int]$versionPatchProperty.Value
} elseif ($null -ne $versionBuildProperty) {
    $versionPatchOrBuild = [int]$versionBuildProperty.Value
}
$versionRevisionOrSentinel = -1
if ($null -ne $versionRevisionProperty) {
    $versionRevisionOrSentinel = [int]$versionRevisionProperty.Value
}
$diagnosticA = @(
    $e.Length, $r.Length, $p.Length, $ep.Length, $rp.Length, $pp.Length
) -join ','
$diagnosticB = @(
    [int]$er, [int]$rpEq, [int]$erCi, $pre, $suf, $first, $el, $rl
) -join ','
$diagnosticC = @(
    [int]$processIsPwsh, [int]$firstMatchesPsHome, [int]$configuredPathHasPsHomePrefix, [int]$psHomeIsDotnetTool,
    $version.Major, $version.Minor, $versionPatchOrBuild, $versionRevisionOrSentinel
) -join ','
[Console]::WriteLine(('KERMINAL_' + 'PATH_DIAG_A:' + $diagnosticA))
[Console]::WriteLine(('KERMINAL_' + 'PATH_DIAG_B:' + $diagnosticB))
[Console]::WriteLine(('KERMINAL_' + 'PATH_DIAG_C:' + $diagnosticC))
$ok = 'KERMINAL_PATH_' + 'CHECK_OK'
$bad = 'KERMINAL_PATH_' + 'CHECK_MISMATCH'
if ($er -and $rpEq) { [Console]::WriteLine($ok) } else { [Console]::WriteLine($bad) }
"#
    .trim()
    .to_owned()
}

/// 只认与固定 token 完全相等的独立输出行，避免命令回显、前缀或其他文本触发结果分类。
///
/// 诊断只保留环境字符串长度、分段数及段边界比较结果，不返回或打印任何 PATH 内容；短诊断行避免被 PTY 横向折行。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_path_probe_result(
    service: &dyn SessionService,
    session_id: SessionId,
) -> PathProbeObservation {
    const MATCH_TOKEN: &str = "KERMINAL_PATH_CHECK_OK";
    const MISMATCH_TOKEN: &str = "KERMINAL_PATH_CHECK_MISMATCH";

    let mut observed_snapshot_bytes = 0;
    let mut observed_diagnostics = None;
    let mut observed_diagnostic_rows = PathDiagnosticParseReport::default();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = service
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(16 * 1024),
                    },
                )
                .await
                .expect("PATH probe session snapshot");
            observed_snapshot_bytes = snapshot.text.len();
            observed_diagnostics = path_environment_diagnostics(&snapshot.text);
            observed_diagnostic_rows = path_diagnostic_parse_report(&snapshot.text);
            let matched = snapshot.text.lines().any(|line| line.trim() == MATCH_TOKEN);
            let mismatched = snapshot
                .text
                .lines()
                .any(|line| line.trim() == MISMATCH_TOKEN);
            match (matched, mismatched) {
                (true, false) => {
                    return PathProbeObservation::MatchTokenObserved {
                        snapshot_bytes: observed_snapshot_bytes,
                        diagnostics: observed_diagnostics,
                        diagnostic_rows: observed_diagnostic_rows,
                    };
                }
                (false, true) => {
                    return PathProbeObservation::MismatchTokenObserved {
                        snapshot_bytes: observed_snapshot_bytes,
                        diagnostics: observed_diagnostics,
                        diagnostic_rows: observed_diagnostic_rows,
                    };
                }
                (true, true) => {
                    return PathProbeObservation::BothTokensObserved {
                        snapshot_bytes: observed_snapshot_bytes,
                        diagnostics: observed_diagnostics,
                        diagnostic_rows: observed_diagnostic_rows,
                    };
                }
                (false, false) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap_or(PathProbeObservation::TimedOut {
        snapshot_bytes: observed_snapshot_bytes,
        diagnostics: observed_diagnostics,
        diagnostic_rows: observed_diagnostic_rows,
    })
}

/// 只解析三条独立短诊断行中的长度、段数、布尔比较和规范化版本数字，不接收或返回任何路径字段值。
///
/// @author kongweiguang
#[cfg(windows)]
fn path_environment_diagnostics(text: &str) -> Option<PathEnvironmentDiagnostics> {
    let mut values = Vec::with_capacity(22);
    values.extend(parse_path_diagnostic_line(
        text,
        "KERMINAL_PATH_DIAG_A:",
        6,
    )?);
    values.extend(parse_path_diagnostic_line(
        text,
        "KERMINAL_PATH_DIAG_B:",
        8,
    )?);
    values.extend(parse_path_diagnostic_line(
        text,
        "KERMINAL_PATH_DIAG_C:",
        8,
    )?);
    if values.len() != 22
        || values[6..9].iter().any(|value| !matches!(value, 0 | 1))
        || values[14..18].iter().any(|value| !matches!(value, 0 | 1))
    {
        return None;
    }

    Some(PathEnvironmentDiagnostics {
        expected_chars: usize::try_from(values[0]).ok()?,
        process_chars: usize::try_from(values[1]).ok()?,
        provider_chars: usize::try_from(values[2]).ok()?,
        expected_segments: usize::try_from(values[3]).ok()?,
        process_segments: usize::try_from(values[4]).ok()?,
        provider_segments: usize::try_from(values[5]).ok()?,
        expected_process_exact: values[6] == 1,
        process_provider_exact: values[7] == 1,
        expected_process_ignore_case: values[8] == 1,
        common_prefix_segments: usize::try_from(values[9]).ok()?,
        common_suffix_segments: usize::try_from(values[10]).ok()?,
        first_differing_segment: usize::try_from(values[11])
            .ok()
            .and_then(|value| value.checked_sub(1)),
        expected_differing_segment_chars: usize::try_from(values[12]).ok(),
        process_differing_segment_chars: usize::try_from(values[13]).ok(),
        process_basename_is_pwsh: values[14] == 1,
        first_segment_matches_pshome: values[15] == 1,
        configured_path_has_pshome_prefix: values[16] == 1,
        pshome_uses_dotnet_tool_layout: values[17] == 1,
        powershell_version_components: [values[18], values[19], values[20], values[21]],
    })
}

/// 解析恰好一条完整诊断记录；拒绝PTY折行残片、重复记录和任何非数值尾缀。
///
/// @author kongweiguang
#[cfg(windows)]
fn parse_path_diagnostic_line(
    text: &str,
    marker: &str,
    expected_fields: usize,
) -> Option<Vec<i64>> {
    let mut matching_lines = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix(marker));
    let value = matching_lines.next()?;
    if matching_lines.next().is_some() {
        return None;
    }

    let values = value
        .split(',')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (values.len() == expected_fields).then_some(values)
}

/// 仅统计三个固定标记的出现、行形状和字符类别，定位解析失败但不暴露任意终端正文。
///
/// @author kongweiguang
#[cfg(windows)]
fn path_diagnostic_parse_report(text: &str) -> PathDiagnosticParseReport {
    PathDiagnosticParseReport {
        a: path_diagnostic_marker_report(text, "KERMINAL_PATH_DIAG_A:", 6),
        b: path_diagnostic_marker_report(text, "KERMINAL_PATH_DIAG_B:", 8),
        c: path_diagnostic_marker_report(text, "KERMINAL_PATH_DIAG_C:", 8),
    }
}

/// 汇总单个固定标记的安全形状指标；字符只归类为数字、分隔符、空白或其他类别。
///
/// @author kongweiguang
#[cfg(windows)]
fn path_diagnostic_marker_report(
    text: &str,
    marker: &str,
    expected_fields: usize,
) -> PathDiagnosticMarkerReport {
    let mut report = PathDiagnosticMarkerReport {
        marker_occurrences: text.matches(marker).count(),
        ..PathDiagnosticMarkerReport::default()
    };

    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix(marker) else {
            continue;
        };
        let fields = payload.split(',');
        let mut field_count = 0;
        let mut numeric_payload = true;
        for (field_index, field) in fields.enumerate() {
            field_count += 1;
            if field.parse::<i64>().is_err() {
                numeric_payload = false;
                if field_index < u16::BITS as usize {
                    report.non_numeric_field_mask |= 1_u16 << field_index;
                    if field.is_empty() {
                        report.empty_field_mask |= 1_u16 << field_index;
                    }
                }
            }
        }

        report.marker_prefixed_lines += 1;
        report.line_lengths_chars = report.line_lengths_chars.max(line.chars().count());
        report.payload_lengths_bytes = report.payload_lengths_bytes.max(payload.len());
        report.first_field_count.get_or_insert(field_count);
        report.last_field_count = Some(field_count);
        report.numeric_payload_lines += if numeric_payload { 1 } else { 0 };
        report.expected_shape_lines += if numeric_payload && field_count == expected_fields {
            1
        } else {
            0
        };
        report.payload_character_classes |= payload.chars().fold(0, |classes, character| {
            classes
                | match character {
                    '0'..='9' => 1 << 0,
                    ',' => 1 << 1,
                    '+' | '-' => 1 << 2,
                    value if value.is_ascii_whitespace() => 1 << 3,
                    value if value.is_ascii_control() => 1 << 4,
                    value if value.is_ascii() => 1 << 5,
                    _ => 1 << 6,
                }
        });
    }

    report
}

/// 仅保存不含路径内容的 PowerShell 环境比较摘要，供失败断言定位差异类别。
///
/// @author kongweiguang
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathEnvironmentDiagnostics {
    expected_chars: usize,
    process_chars: usize,
    provider_chars: usize,
    expected_segments: usize,
    process_segments: usize,
    provider_segments: usize,
    expected_process_exact: bool,
    process_provider_exact: bool,
    expected_process_ignore_case: bool,
    common_prefix_segments: usize,
    common_suffix_segments: usize,
    first_differing_segment: Option<usize>,
    expected_differing_segment_chars: Option<usize>,
    process_differing_segment_chars: Option<usize>,
    process_basename_is_pwsh: bool,
    first_segment_matches_pshome: bool,
    /// 记录 PowerShell 启动前传入的 PATH 是否已带精确 PSHOME 前缀。
    configured_path_has_pshome_prefix: bool,
    pshome_uses_dotnet_tool_layout: bool,
    /// 顺序为 Major、Minor、Patch/Build、Revision；缺失属性用 -1 表示。
    powershell_version_components: [i64; 4],
}

/// 安全记录固定诊断行的计数、长度、字段形状和字符类别，不包含行文本或环境值。
///
/// @author kongweiguang
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PathDiagnosticMarkerReport {
    marker_occurrences: usize,
    marker_prefixed_lines: usize,
    numeric_payload_lines: usize,
    expected_shape_lines: usize,
    /// 位 i 表示第 i 个逗号分隔字段无法解析为整数。
    non_numeric_field_mask: u16,
    /// 位 i 表示第 i 个逗号分隔字段为空。
    empty_field_mask: u16,
    line_lengths_chars: usize,
    payload_lengths_bytes: usize,
    first_field_count: Option<usize>,
    last_field_count: Option<usize>,
    /// 位 0..6 依次代表数字、逗号、正负号、空白、控制字符、其他 ASCII、非 ASCII。
    payload_character_classes: u8,
}

/// 三组固定诊断标记的安全解析报告，仅附加在测试失败观测中。
///
/// @author kongweiguang
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PathDiagnosticParseReport {
    a: PathDiagnosticMarkerReport,
    b: PathDiagnosticMarkerReport,
    c: PathDiagnosticMarkerReport,
}

/// 仅暴露 PATH probe 的固定回执分类与快照长度，避免诊断包含机器路径。
///
/// @author kongweiguang
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathProbeObservation {
    MatchTokenObserved {
        snapshot_bytes: usize,
        diagnostics: Option<PathEnvironmentDiagnostics>,
        diagnostic_rows: PathDiagnosticParseReport,
    },
    MismatchTokenObserved {
        snapshot_bytes: usize,
        diagnostics: Option<PathEnvironmentDiagnostics>,
        diagnostic_rows: PathDiagnosticParseReport,
    },
    BothTokensObserved {
        snapshot_bytes: usize,
        diagnostics: Option<PathEnvironmentDiagnostics>,
        diagnostic_rows: PathDiagnosticParseReport,
    },
    TimedOut {
        snapshot_bytes: usize,
        diagnostics: Option<PathEnvironmentDiagnostics>,
        diagnostic_rows: PathDiagnosticParseReport,
    },
}

/// 用真实 PowerShell PTY 验证默认启动 hook、多行脚本、变量和 cwd 跨步骤保持及真实 C/D 结果；启动失败只显示系统错误种类和数值码，避免命令与路径进入测试日志。
/// 探测禁用用户 profile 且无窗口运行，测试后代也不得打断开发者的桌面操作。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn powershell_integration_runs_multiline_script_in_current_shell() {
    use std::process::Command;
    if Command::new("pwsh")
        .creation_flags(0x0800_0000)
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .status()
        .is_err()
    {
        return;
    }

    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh"),
                args: vec![OsString::from("-NoLogo"), OsString::from("-NoProfile")],
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
        })
        .await
        .unwrap_or_else(|error| {
            let mut source = std::error::Error::source(&error);
            let mut io_diagnostic = None;
            while let Some(cause) = source {
                if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
                    io_diagnostic = Some((io_error.kind(), io_error.raw_os_error()));
                    break;
                }
                source = cause.source();
            }
            let (kind, raw_os_error) = io_diagnostic
                .map(|(kind, raw_os_error)| (Some(kind), raw_os_error))
                .unwrap_or((None, None));
            panic!(
                "start interactive PowerShell with hook (io_kind={kind:?}, raw_os_error={raw_os_error:?})"
            );
        });
    wait_for_shell_prompt(service.as_ref(), &info.session_id).await;
    let metadata = service
        .runtime_metadata(info.session_id.clone())
        .await
        .expect("PowerShell runtime metadata");
    assert!(metadata.shell_integration_enabled);
    assert_eq!(metadata.shell_dialect, RemoteShellDialect::PowerShell);

    let first = service.execute_shell_command(ShellCommandRequest {
        session_id: info.session_id.clone(),
        expected_ui_binding: None,
        command_id: "powershell-multiline-1".into(),
        command: "$global:KerminalWorkflowValue = 'first ''quoted'' 世界'\nSet-Location -LiteralPath $env:TEMP\n[Console]::WriteLine(\"MULTILINE_FIRST:$global:KerminalWorkflowValue\")".into(),
        timeout: Duration::from_secs(20),
    }, tokio_util::sync::CancellationToken::new()).await.expect("execute first workflow step");
    assert_eq!(
        first,
        ShellCommandOutcome::Finished {
            command_id: "powershell-multiline-1".into(),
            exit_code: 0
        }
    );
    wait_for_text(
        service.as_ref(),
        info.session_id.clone(),
        "MULTILINE_FIRST:first 'quoted' 世界",
    )
    .await;

    let second = service.execute_shell_command(ShellCommandRequest {
        session_id: info.session_id.clone(),
        expected_ui_binding: None,
        command_id: "powershell-multiline-2".into(),
        command: "[Console]::WriteLine(\"MULTILINE_SECOND:$global:KerminalWorkflowValue\")\n[Console]::WriteLine(\"PERSISTED_CWD:$((Get-Location).Path)\")".into(),
        timeout: Duration::from_secs(20),
    }, tokio_util::sync::CancellationToken::new()).await.expect("execute second workflow step");
    assert_eq!(
        second,
        ShellCommandOutcome::Finished {
            command_id: "powershell-multiline-2".into(),
            exit_code: 0
        }
    );
    let output = wait_for_text(service.as_ref(), info.session_id.clone(), "PERSISTED_CWD:").await;
    assert!(output.contains("MULTILINE_SECOND:first 'quoted' 世界"));
    assert!(output.contains("PERSISTED_CWD:"));
    service
        .close(info.session_id)
        .await
        .expect("close PowerShell process tree");
}

/// 验证不带 target PATH 覆盖时，provider PATH 原样继承，PowerShell 仅按启动规则前置 PSHOME。
/// 探测禁用用户 profile 且无窗口运行，测试后代也不得打断开发者的桌面操作。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn powershell_bare_program_inherits_parent_path() {
    use std::process::Command;
    if Command::new("pwsh")
        .creation_flags(0x0800_0000)
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .status()
        .is_err()
    {
        return;
    }
    let Some(parent_path) = std::env::var_os("PATH") else {
        return;
    };
    let mut environment = BTreeMap::new();
    environment.insert(OsString::from("KERMINAL_TEST_EXPECTED_PATH"), parent_path);
    assert_pwsh_path_matches_expected(environment).await;
}

/// 验证 target 中混合大小写的 Path 覆盖继承值，随后仅应用 PowerShell 的 PSHOME 前置规则。
/// 可用性探测必须无窗口运行，测试后代也不得打断开发者的桌面操作。
///
/// 只在会话环境中追加哨兵目录，探测禁用用户 profile，不修改测试进程的全局 PATH。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn powershell_target_path_overlay_wins_case_insensitively() {
    use std::process::Command;
    if Command::new("pwsh")
        .creation_flags(0x0800_0000)
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .status()
        .is_err()
    {
        return;
    }
    let Some(parent_path) = std::env::var_os("PATH") else {
        return;
    };
    let mut path_entries: Vec<_> = std::env::split_paths(&parent_path).collect();
    path_entries.push(std::env::temp_dir().join(format!(
        "kerminal-path-overlay-sentinel-{}",
        std::process::id()
    )));
    let target_path = std::env::join_paths(path_entries).expect("build isolated target PATH");

    let mut environment = BTreeMap::new();
    environment.insert(OsString::from("Path"), target_path.clone());
    environment.insert(OsString::from("KERMINAL_TEST_EXPECTED_PATH"), target_path);
    assert_pwsh_path_matches_expected(environment).await;
}

/// 未安装或未观察到 OSC hook 时显式返回能力不足，不能用写入成功生成伪完成状态。
/// 探测和命令目标禁用用户 profile；探测无窗口运行，不借用户启动脚本制造副作用。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn powershell_startup_with_command_mode_does_not_claim_integration() {
    use std::process::Command;
    if Command::new("pwsh")
        .creation_flags(0x0800_0000)
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .status()
        .is_err()
    {
        return;
    }
    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh"),
                args: vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-Command"),
                    OsString::from("exit 0"),
                ],
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(80, 24, 0, 0).expect("valid grid"),
        })
        .await
        .expect("start explicit one-shot command target");
    let result = service
        .execute_shell_command(
            ShellCommandRequest {
                session_id: info.session_id.clone(),
                expected_ui_binding: None,
                command_id: "unsupported-shell-integration".into(),
                command: "Write-Output should-not-run".into(),
                timeout: Duration::from_secs(1),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err()
            | matches!(
                result,
                Ok(ShellCommandOutcome::Unknown {
                    reason: ShellCommandUnknownReason::Disconnected,
                    ..
                })
            )
    );
    service
        .close(info.session_id)
        .await
        .expect("close one-shot process session");
}

/// 用真实 Fish PTY 验证 Fish 方言的一行编码器能在当前交互 shell 执行多行脚本并保留状态。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn fish_integration_runs_multiline_script_in_current_shell() {
    use std::process::Command;
    if Command::new("fish").arg("--version").status().is_err() {
        return;
    }

    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("fish"),
                args: vec![OsString::from("-i")],
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
        })
        .await
        .expect("start interactive Fish with hook");
    wait_for_shell_prompt(service.as_ref(), &info.session_id).await;
    let metadata = service
        .runtime_metadata(info.session_id.clone())
        .await
        .expect("Fish runtime metadata");
    assert!(metadata.shell_integration_enabled);
    assert_eq!(metadata.shell_dialect, RemoteShellDialect::Fish);

    let first = service.execute_shell_command(ShellCommandRequest {
        session_id: info.session_id.clone(),
        expected_ui_binding: None,
        command_id: "fish-multiline-1".into(),
        command: "set -g KerminalWorkflowValue 'first \\'quoted\\' 世界'\ncd \"$TMPDIR\"\nprintf 'FISH_FIRST:%s\\n' \"$KerminalWorkflowValue\"".into(),
        timeout: Duration::from_secs(20),
    }, tokio_util::sync::CancellationToken::new()).await.expect("execute first Fish workflow step");
    assert_eq!(
        first,
        ShellCommandOutcome::Finished {
            command_id: "fish-multiline-1".into(),
            exit_code: 0
        }
    );
    wait_for_text(
        service.as_ref(),
        info.session_id.clone(),
        "FISH_FIRST:first 'quoted' 世界",
    )
    .await;

    let second = service.execute_shell_command(ShellCommandRequest {
        session_id: info.session_id.clone(),
        expected_ui_binding: None,
        command_id: "fish-multiline-2".into(),
        command: "printf 'FISH_SECOND:%s\\n' \"$KerminalWorkflowValue\"\nprintf 'FISH_CWD:%s\\n' \"$PWD\"".into(),
        timeout: Duration::from_secs(20),
    }, tokio_util::sync::CancellationToken::new()).await.expect("execute second Fish workflow step");
    assert_eq!(
        second,
        ShellCommandOutcome::Finished {
            command_id: "fish-multiline-2".into(),
            exit_code: 0
        }
    );
    let output = wait_for_text(service.as_ref(), info.session_id.clone(), "FISH_CWD:").await;
    assert!(output.contains("FISH_SECOND:first 'quoted' 世界"));
    service
        .close(info.session_id)
        .await
        .expect("close Fish process tree");
}

/// 验证同一 watch receiver 在退出后重连仍有事件，且旧屏幕内容不会串入新 VT generation。
///
/// @author kongweiguang
#[tokio::test]
async fn reconnect_preserves_session_watch_and_resets_terminal_generation() {
    let service = service();
    let info = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(local_shell()),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(80, 24, 0, 0).expect("valid grid"),
        })
        .await
        .expect("spawn initial shell");
    let mut changes = service
        .subscribe(info.session_id.clone())
        .await
        .expect("subscribe once");

    service
        .write(
            info.session_id.clone(),
            b"echo KERMINAL_OLD_GENERATION\r".to_vec(),
        )
        .await
        .expect("write first marker");
    wait_for_text(
        service.as_ref(),
        info.session_id.clone(),
        "KERMINAL_OLD_GENERATION",
    )
    .await;
    let prior_revision = service
        .render_snapshot(info.session_id.clone(), Default::default())
        .await
        .expect("snapshot")
        .revision;
    service
        .write(info.session_id.clone(), b"exit\r".to_vec())
        .await
        .expect("exit first shell");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let sessions = service
                .list(SessionFilter::default())
                .await
                .expect("list exited shell");
            if sessions
                .first()
                .is_some_and(|session| session.lifecycle == SessionLifecycle::Exited)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child exit observed");

    let reconnected = service
        .reconnect(info.session_id.clone())
        .await
        .expect("reopen same target");
    assert_eq!(reconnected.session_id, info.session_id);
    let subscription_cancel = tokio_util::sync::CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let change = changes
                .recv(&subscription_cancel)
                .await
                .expect("original subscription remains connected")
                .expect("session change channel remains open");
            if change.revision > prior_revision {
                break;
            }
        }
    })
    .await
    .expect("existing watch observed reconnect revision");
    let new_snapshot = service
        .render_snapshot(info.session_id.clone(), Default::default())
        .await
        .expect("new generation snapshot");
    assert!(
        new_snapshot.revision > prior_revision,
        "revision must remain monotonic across reconnect"
    );
    assert!(
        !new_snapshot
            .rows
            .iter()
            .flat_map(|row| row.runs.iter())
            .any(|run| run.text.contains("KERMINAL_OLD_GENERATION"))
    );

    service
        .write(
            info.session_id.clone(),
            b"echo KERMINAL_NEW_GENERATION\r".to_vec(),
        )
        .await
        .expect("write second marker");
    let output = wait_for_text(
        service.as_ref(),
        info.session_id.clone(),
        "KERMINAL_NEW_GENERATION",
    )
    .await;
    assert!(!output.contains("KERMINAL_OLD_GENERATION"));
    service
        .close(info.session_id)
        .await
        .expect("close reconnected child");
}

#[cfg(windows)]
const MAX_NATURAL_EXIT_TEST_OUTPUT_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const NATURAL_EXIT_GATE_ENV: &str = "KERMINAL_CONPTY_NATURAL_EXIT_GATE";
#[cfg(windows)]
const NATURAL_EXIT_DESCENDANT_GATE_ENV: &str = "KERMINAL_CONPTY_NATURAL_EXIT_DESCENDANT_GATE";
#[cfg(windows)]
const NATURAL_EXIT_DESCENDANT_READY_ENV: &str = "KERMINAL_CONPTY_NATURAL_EXIT_DESCENDANT_READY";
#[cfg(windows)]
const NATURAL_EXIT_DESCENDANT_PID_ENV: &str = "KERMINAL_CONPTY_NATURAL_EXIT_DESCENDANT_PID";
#[cfg(windows)]
const NATURAL_EXIT_DESCENDANT_EXIT_GATE_ENV: &str =
    "KERMINAL_CONPTY_NATURAL_EXIT_DESCENDANT_EXIT_GATE";

/// 测试失败时先从 PID 文件接管固定 descendant 句柄，再释放三个 gate 并有界等待它退出。
///
/// @author kongweiguang
#[cfg(windows)]
struct NaturalExitGateGuard {
    paths: [std::path::PathBuf; 3],
    descendant_pid_file: std::path::PathBuf,
    descendant_handle: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl Drop for NaturalExitGateGuard {
    /// 先固定 descendant 身份，再打开所有 gate 并有界等待，避免测试提前失败留下私有进程。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if self.descendant_handle.is_none()
            && let Some(pid) = std::fs::read_to_string(&self.descendant_pid_file)
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
        {
            // 根 helper 在 root gate 释放前仍活着，故此处按 PID 取句柄不会跨过退出/PID 复用边界。
            self.descendant_handle = open_native_child_handle(pid).ok();
        }
        for path in &self.paths {
            let _ = std::fs::write(path, b"cleanup");
        }
        if let Some(handle) = self.descendant_handle.as_ref()
            && !matches!(
                wait_for_native_process_exit(handle, Duration::from_secs(8)),
                Ok(true)
            )
        {
            eprintln!("自然退出回归的私有 descendant 未在失败清理期限内结束");
        }
    }
}

/// 供自然退出回归子进程直写 ConPTY；后代 PID 交给外层测试持有句柄等待，避免等待后代阻止根进程先退出。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
#[ignore = "仅由 ConPTY 自然退出诊断用例作为子进程启动"]
fn conpty_natural_exit_native_stdout_helper() {
    use std::io::Write;
    use std::process::Command;
    use std::time::Duration;

    if let Some(gate) = std::env::var_os(NATURAL_EXIT_DESCENDANT_GATE_ENV) {
        let pid_file = std::path::PathBuf::from(
            std::env::var_os(NATURAL_EXIT_DESCENDANT_PID_ENV)
                .expect("parent provides isolated descendant PID path"),
        );
        let executable = std::env::current_exe().expect("current test executable");
        let mut descendant = Command::new(executable)
            .args([
                "--exact",
                "conpty_natural_exit_descendant_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(NATURAL_EXIT_DESCENDANT_GATE_ENV, gate)
            .env(
                NATURAL_EXIT_DESCENDANT_READY_ENV,
                std::env::var_os(NATURAL_EXIT_DESCENDANT_READY_ENV)
                    .expect("parent provides isolated descendant ready path"),
            )
            .env(NATURAL_EXIT_DESCENDANT_PID_ENV, pid_file.as_os_str())
            .env(
                NATURAL_EXIT_DESCENDANT_EXIT_GATE_ENV,
                std::env::var_os(NATURAL_EXIT_DESCENDANT_EXIT_GATE_ENV)
                    .expect("parent provides isolated descendant exit gate"),
            )
            .spawn()
            .expect("spawn controlled descendant in the same console");
        if let Err(error) = std::fs::write(&pid_file, descendant.id().to_string()) {
            let _ = descendant.kill();
            let _ = descendant.wait();
            panic!("publish descendant PID for the external process owner: {error}");
        }
        match descendant.try_wait() {
            Ok(None) => {}
            Ok(Some(_)) => {
                let _ = std::fs::remove_file(&pid_file);
                panic!("descendant exited before its gate was released");
            }
            Err(error) => {
                let _ = descendant.kill();
                let _ = descendant.wait();
                let _ = std::fs::remove_file(&pid_file);
                panic!("observe the gated descendant process: {error}");
            }
        }
    }

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER\n")
        .expect("write native stdout marker");
    stdout.flush().expect("flush native stdout marker");

    // 让测试先打开本子进程的原生句柄，再释放自然退出，避免 PID 重用影响退出观察。
    let gate = std::env::var_os(NATURAL_EXIT_GATE_ENV)
        .expect("parent provides an isolated natural-exit gate");
    let gate = std::path::PathBuf::from(gate);
    while !gate.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 发布 ready 状态后等待测试 gate；根 helper 已将 PID 交给主测试，由主测试持有最终 wait 句柄。
/// 之后仅在测试 gate 允许时输出尾部标记，验证 ConPTY 保留仍活动 client 的字节。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
#[ignore = "仅由 ConPTY 自然退出后代输出回归用例作为子进程启动"]
fn conpty_natural_exit_descendant_helper() {
    use std::io::Write;
    use std::time::Duration;

    let gate = std::env::var_os(NATURAL_EXIT_DESCENDANT_GATE_ENV)
        .expect("parent provides isolated descendant gate");
    let gate = std::path::PathBuf::from(gate);
    let ready = std::env::var_os(NATURAL_EXIT_DESCENDANT_READY_ENV)
        .expect("parent provides isolated descendant ready path");
    std::fs::write(ready, b"ready").expect("publish descendant readiness");
    while !gate.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"KERMINAL_CONPTY_DESCENDANT_AFTER_ROOT_EXIT\n")
        .expect("write output after direct child exit");
    stdout.flush().expect("flush descendant output marker");
    let exit_gate = std::env::var_os(NATURAL_EXIT_DESCENDANT_EXIT_GATE_ENV)
        .expect("parent provides isolated descendant exit gate");
    let exit_gate = std::path::PathBuf::from(exit_gate);
    while !exit_gate.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 在 ConPTY child 仍等待测试 gate 时打开同一进程对象，之后不再按 PID 重新查询。
///
/// @author kongweiguang
#[cfg(windows)]
fn open_native_child_handle(pid: u32) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    // 同时请求等待与读取退出码的权限；测试子进程由当前用户创建，不扩大访问范围。
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: OpenProcess 返回一个新拥有的句柄，此处接管并由 OwnedHandle 恰好关闭一次。
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// 以零超时检查固定进程对象，退出码只在内核确认对象已结束后读取。
///
/// @author kongweiguang
#[cfg(windows)]
fn observed_native_exit_code(
    handle: &std::os::windows::io::OwnedHandle,
) -> std::io::Result<Option<u32>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => {
            let mut exit_code = 0;
            if unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut exit_code) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Some(exit_code))
        }
        WAIT_TIMEOUT => Ok(None),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        status => Err(std::io::Error::other(format!(
            "unexpected process wait status {status}"
        ))),
    }
}

/// 在同一绝对截止时间内轮询已持有句柄，避免进程枚举工具超时或 PID 复用歧义。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_native_exit_code(
    handle: &std::os::windows::io::OwnedHandle,
    deadline: tokio::time::Instant,
) -> std::io::Result<Option<u32>> {
    loop {
        if let Some(exit_code) = observed_native_exit_code(handle)? {
            return Ok(Some(exit_code));
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        tokio::time::sleep_until((now + Duration::from_millis(20)).min(deadline)).await;
    }
}

/// 在 Drop 清理路径上使用 Windows 固定进程句柄做有限等待，避免仅释放 gate 后遗留私有 helper。
///
/// @author kongweiguang
#[cfg(windows)]
fn wait_for_native_process_exit(
    handle: &std::os::windows::io::OwnedHandle,
    timeout: Duration,
) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        status => Err(std::io::Error::other(format!(
            "unexpected process cleanup wait status {status}"
        ))),
    }
}

/// 持续读到 PTY EOF，同时把诊断输出限制在固定上限内，避免测试异常时无界缓存。
///
/// @author kongweiguang
#[cfg(windows)]
async fn read_pty_output_until_eof(
    reader: &mut std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + 'static>>,
    output: &mut Vec<u8>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut buffer = [0_u8; 4096];
    let mut exceeded_limit = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return if exceeded_limit {
                Err(std::io::Error::other(
                    "native PTY output exceeded the test capture bound",
                ))
            } else {
                Ok(())
            };
        }
        let available = MAX_NATURAL_EXIT_TEST_OUTPUT_BYTES.saturating_sub(output.len());
        let retained = count.min(available);
        output.extend_from_slice(&buffer[..retained]);
        exceeded_limit |= retained < count;
    }
}

/// 在自然退出 gate 释放前读取原生 marker，并回答 ConPTY 的光标位置查询以避免 helper 卡在启动握手。
///
/// @author kongweiguang
#[cfg(windows)]
async fn read_pty_until_marker(
    reader: &mut std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + 'static>>,
    writer: &mut std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send + 'static>>,
    output: &mut Vec<u8>,
    marker: &[u8],
) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    const CURSOR_RESPONSE: &[u8] = b"\x1b[1;1R";
    let mut buffer = [0_u8; 4096];
    let mut scanned_until = 0_usize;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(false);
        }

        let available = MAX_NATURAL_EXIT_TEST_OUTPUT_BYTES.saturating_sub(output.len());
        let retained = count.min(available);
        output.extend_from_slice(&buffer[..retained]);
        if retained < count {
            return Err(std::io::Error::other(
                "native PTY output exceeded the test capture bound",
            ));
        }

        // ConPTY 启用 INHERIT_CURSOR 时会向 child 发 DSR；纯 PTY 测试没有 TerminalEngine 代答，
        // 因此按累积输出扫描分片查询并原样返回有效光标位置，保留真实 ConPTY 启动路径。
        let scan_start = scanned_until.saturating_sub(CURSOR_QUERY.len().saturating_sub(1));
        let mut scan_position = scan_start;
        while let Some(offset) = output[scan_position..]
            .windows(CURSOR_QUERY.len())
            .position(|window| window == CURSOR_QUERY)
        {
            let query_end = scan_position + offset + CURSOR_QUERY.len();
            writer.write_all(CURSOR_RESPONSE).await?;
            writer.flush().await?;
            scan_position = query_end;
        }
        scanned_until = output.len();

        if output.windows(marker.len()).any(|window| window == marker) {
            return Ok(true);
        }
    }
}

/// 持有原生 child handle 与 PTY writer，分别观察 child wait、marker 和 reader EOF，再有界回收 owner。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn conpty_natural_child_exit_and_reader_eof_are_separately_observed() {
    use std::ffi::OsString;
    use std::io;

    use kerminal_application::sessions::SessionTransportFactory;
    use kerminal_domain::sessions::{LocalSessionTarget, SessionTarget, TerminalSize};
    use tokio::time::{Instant, timeout_at};
    use tokio_util::sync::CancellationToken;

    let gate_directory = tempfile::tempdir().expect("isolated ConPTY gate directory");
    let gate_path = gate_directory.path().join("release-natural-exit");
    let helper_exe = std::env::current_exe().expect("current integration test executable");
    let target = SessionTarget::Local(LocalSessionTarget {
        program: helper_exe.into_os_string(),
        args: [
            "--exact",
            "conpty_natural_exit_native_stdout_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        working_directory: None,
        environment: BTreeMap::from([(
            OsString::from(NATURAL_EXIT_GATE_ENV),
            gate_path.as_os_str().to_owned(),
        )]),
    });
    let factory = LocalPtySessionTransportFactory::new();
    let transport = match factory
        .open(
            &target,
            TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
            CancellationToken::new(),
        )
        .await
    {
        Ok(transport) => transport,
        Err(open_error) => {
            let (error, cleanup_owner) = open_error.into_parts();
            if let Some(cleanup_owner) = cleanup_owner {
                let deadline = Instant::now() + Duration::from_secs(8);
                let cleanup = timeout_at(deadline, cleanup_owner.close_until(deadline)).await;
                assert!(
                    matches!(cleanup, Ok(Ok(()))),
                    "failed PTY open retained a cleanup owner that did not close before its deadline"
                );
            }
            panic!("open native stdout PTY child: {error}");
        }
    };

    let process_id = transport.process_id;
    let control = Arc::clone(&transport.control);
    let mut reader = transport.reader;
    let mut output = Vec::new();
    // SessionManager 持有输入 writer 与 reader/child monitor 同期；自然退出测量也保留它，避免把提前关闭误当成 EOF 证据。
    let mut writer = Some(transport.writer);

    let observation_deadline = Instant::now() + Duration::from_secs(10);
    // helper 在自身 gate 上等待父进程；先打开返回的原生 PID，不能让 marker 到达与否决定句柄是否存在。
    let process_handle = process_id
        .ok_or_else(|| io::Error::other("PTY transport did not return a process id"))
        .and_then(open_native_child_handle);
    let marker_deadline = (Instant::now() + Duration::from_secs(2)).min(observation_deadline);
    let native_marker_observed = timeout_at(
        marker_deadline,
        read_pty_until_marker(
            &mut reader,
            writer
                .as_mut()
                .expect("test retains the PTY writer during observation"),
            &mut output,
            b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER",
        ),
    )
    .await;
    let marker_seen = match &native_marker_observed {
        Ok(Ok(seen)) => *seen,
        _ => false,
    };
    let gate_release = std::fs::write(&gate_path, b"exit");
    let child_wait = timeout_at(observation_deadline, control.observe_child_exit());
    let native_child_exit = async {
        match process_handle.as_ref() {
            Ok(handle) => wait_for_native_exit_code(handle, observation_deadline).await,
            Err(error) => Err(io::Error::other(error.to_string())),
        }
    };
    let (child_wait_observed, native_exit_observed) = tokio::join!(child_wait, native_child_exit);
    let child_status = match &child_wait_observed {
        Ok(Ok(Some(status))) => Some(status.clone()),
        _ => None,
    };
    let process_exit_code_before_cleanup = match &native_exit_observed {
        Ok(exit_code) => *exit_code,
        _ => None,
    };
    let writer_held_open_during_observation = writer.is_some();
    let marker_before_cleanup = output
        .windows(b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER".len())
        .any(|window| window == b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER");

    // native child 状态一经观察就启动统一 drain 预算；reader EOF 与 worker join 不重置它。
    let natural_deadline = child_status
        .as_ref()
        .map(|_| Instant::now() + Duration::from_secs(10));
    let full_wait_before_release = if let Some(deadline) = natural_deadline {
        let short_observation = (Instant::now() + Duration::from_millis(150)).min(deadline);
        Some(timeout_at(short_observation, control.wait_exit()).await)
    } else {
        None
    };
    let full_wait_pending_with_writer =
        matches!(full_wait_before_release.as_ref(), Some(Err(_))) && writer.is_some();

    let release_result = if let Some(deadline) = natural_deadline {
        Some(timeout_at(deadline, control.release_after_child_exit_until(deadline)).await)
    } else {
        None
    };
    let release_supported = matches!(&release_result, Some(Ok(Ok(()))));
    let release_unsupported = matches!(
        &release_result,
        Some(Ok(Err(error))) if error.public_code() == kerminal_application::PublicErrorCode::Unsupported
    );

    // 只有成功的非破坏性 Release 才等待自然 EOF；不支持的旧 Windows 留待显式 close 回收。
    let reader_eof_before_close = if release_supported {
        if let Some(deadline) = natural_deadline {
            matches!(
                timeout_at(
                    deadline,
                    read_pty_output_until_eof(&mut reader, &mut output),
                )
                .await,
                Ok(Ok(()))
            )
        } else {
            false
        }
    } else {
        false
    };
    let output_before_cleanup = output.clone();

    let mut joined_exit = None;
    if reader_eof_before_close {
        // EOF 已由 reader 证明后才关闭应用输入半部；native writer 在 EOF 前一直保持。
        drop(writer.take());
        if let Some(deadline) = natural_deadline {
            joined_exit = Some(timeout_at(deadline, control.wait_exit()).await);
        }
    }
    let natural_wait_completed = joined_exit
        .as_ref()
        .is_some_and(|result| matches!(result, Ok(Ok(_))));

    let mut close_result = None;
    let mut drained_reader = None;
    if !natural_wait_completed {
        // fallback 是一次明确的破坏性关闭；它使用调用方的新预算，不伪装为自然 drain 成功。
        drop(writer.take());
        let cleanup_deadline = Instant::now() + Duration::from_secs(8);
        let (close, wait, drain) = tokio::join!(
            timeout_at(cleanup_deadline, control.close_until(cleanup_deadline)),
            timeout_at(cleanup_deadline, control.wait_exit()),
            timeout_at(
                cleanup_deadline,
                read_pty_output_until_eof(&mut reader, &mut output),
            ),
        );
        close_result = Some(close);
        joined_exit = Some(wait);
        drained_reader = Some(drain);
    }

    // 清理结束后仍从原进程对象读取状态；不按 PID 重新打开，避免句柄缺失或 PID 复用误报。
    let process_exit_code_after_cleanup = process_handle
        .as_ref()
        .ok()
        .and_then(|handle| observed_native_exit_code(handle).ok().flatten());
    let cleanup_completed = natural_wait_completed
        || (close_result
            .as_ref()
            .is_some_and(|result| matches!(result, Ok(Ok(()))))
            && joined_exit
                .as_ref()
                .is_some_and(|result| matches!(result, Ok(Ok(_))))
            && drained_reader
                .as_ref()
                .is_some_and(|result| matches!(result, Ok(Ok(())))));
    let marker_after_cleanup = output
        .windows(b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER".len())
        .any(|window| window == b"KERMINAL_CONPTY_NATURAL_EXIT_MARKER");
    let child_wait_before_cleanup = match &child_wait_observed {
        Ok(Ok(Some(status))) => {
            format!("status(success={}, code={:?})", status.success, status.code)
        }
        Ok(Ok(None)) => "no-native-child".to_owned(),
        Ok(Err(_)) => "error".to_owned(),
        Err(_) => "deadline".to_owned(),
    };
    let native_exit_before_cleanup = match &native_exit_observed {
        Ok(Some(exit_code)) => format!("signaled({exit_code})"),
        Ok(None) => "running-at-deadline".to_owned(),
        Err(_) => "deadline".to_owned(),
    };
    let release_state = if release_supported {
        "released"
    } else if release_unsupported {
        "unsupported"
    } else if natural_deadline.is_none() {
        "not-observed"
    } else {
        "failed-or-timeout"
    };
    let observations = format!(
        "gate_released={}, writer_held_open={}, marker_seen={}, child_wait={child_wait_before_cleanup}, \
         native_exit={native_exit_before_cleanup}, full_wait_pending={}, release={release_state}, \
         reader_eof_before_close={}, bytes_before_cleanup={}, bytes_after_cleanup={}, cleanup_completed={}, \
         native_exit_after_cleanup={process_exit_code_after_cleanup:?}",
        gate_release.is_ok(),
        writer_held_open_during_observation,
        marker_seen && marker_before_cleanup && marker_after_cleanup,
        full_wait_pending_with_writer,
        reader_eof_before_close,
        output_before_cleanup.len(),
        output.len(),
        cleanup_completed,
    );
    eprintln!("ConPTY natural-exit observations after cleanup: {observations}");

    assert!(
        gate_release.is_ok(),
        "could not release isolated child: {observations}"
    );
    assert!(
        writer_held_open_during_observation,
        "native exit observation must retain the PTY input writer: {observations}"
    );
    assert!(
        cleanup_completed,
        "natural drain or explicit cleanup must complete within its absolute deadline: {observations}"
    );
    assert!(
        process_exit_code_before_cleanup == Some(0) && process_exit_code_after_cleanup == Some(0),
        "the retained native process handle must confirm the same child exited successfully: {observations}; handle_error={:?}",
        process_handle.as_ref().err()
    );
    assert!(
        child_status.is_some_and(|status| status.success && status.code == Some(0)),
        "the independent child observer did not report natural exit: {observations}"
    );
    assert!(
        full_wait_pending_with_writer,
        "full wait_exit must not complete while its writer is retained: {observations}"
    );
    assert!(
        release_supported || release_unsupported,
        "a supported backend must release or report capability unavailable: {observations}"
    );
    if release_supported {
        assert!(
            reader_eof_before_close && natural_wait_completed,
            "successful Release must preserve natural reader EOF and complete worker drain: {observations}"
        );
    }
    assert!(
        marker_seen && marker_before_cleanup && marker_after_cleanup,
        "native output marker must remain available across exit/drain: {observations}"
    );
}

/// 确认 native root child 已退出后，SessionManager 仍转发同一 ConPTY descendant 尾字节。
/// service.close 后用测试持有的固定进程句柄等待 descendant，保留失败分支的 gate 清理。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn session_manager_preserves_descendant_output_after_native_child_exit() {
    use std::ffi::OsString;

    let gates = tempfile::tempdir().expect("isolated native-exit helper gates");
    let root_gate = gates.path().join("release-root");
    let descendant_gate = gates.path().join("release-descendant");
    let descendant_exit_gate = gates.path().join("release-descendant-exit");
    let descendant_ready = gates.path().join("descendant-ready");
    let descendant_pid_file = gates.path().join("descendant-pid");
    let mut gate_guard = NaturalExitGateGuard {
        paths: [
            root_gate.clone(),
            descendant_gate.clone(),
            descendant_exit_gate.clone(),
        ],
        descendant_pid_file: descendant_pid_file.clone(),
        descendant_handle: None,
    };
    let executable = std::env::current_exe().expect("current integration test executable");
    let target = LocalSessionTarget {
        program: executable.into_os_string(),
        args: [
            "--exact",
            "conpty_natural_exit_native_stdout_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        working_directory: None,
        environment: BTreeMap::from([
            (
                OsString::from(NATURAL_EXIT_GATE_ENV),
                root_gate.as_os_str().to_owned(),
            ),
            (
                OsString::from(NATURAL_EXIT_DESCENDANT_GATE_ENV),
                descendant_gate.as_os_str().to_owned(),
            ),
            (
                OsString::from(NATURAL_EXIT_DESCENDANT_READY_ENV),
                descendant_ready.as_os_str().to_owned(),
            ),
            (
                OsString::from(NATURAL_EXIT_DESCENDANT_PID_ENV),
                descendant_pid_file.as_os_str().to_owned(),
            ),
            (
                OsString::from(NATURAL_EXIT_DESCENDANT_EXIT_GATE_ENV),
                descendant_exit_gate.as_os_str().to_owned(),
            ),
        ]),
    };
    let service = service();
    let session = service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(target),
            owner: SessionOwner::Mcp,
            size: TerminalSize::new(100, 30, 0, 0).expect("valid grid"),
        })
        .await
        .expect("start controlled ConPTY root child");

    let before_exit = wait_for_text(
        service.as_ref(),
        session.session_id.clone(),
        "KERMINAL_CONPTY_NATURAL_EXIT_MARKER",
    )
    .await;
    assert!(before_exit.contains("KERMINAL_CONPTY_NATURAL_EXIT_MARKER"));
    tokio::time::timeout(Duration::from_secs(8), async {
        while !descendant_ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant attached before the root child is released");
    let descendant_pid = std::fs::read_to_string(&descendant_pid_file)
        .expect("read isolated descendant PID")
        .parse::<u32>()
        .expect("descendant publishes a numeric process ID");
    gate_guard.descendant_handle = Some(
        open_native_child_handle(descendant_pid)
            .expect("open the descendant process object before releasing gates"),
    );
    assert_eq!(
        observed_native_exit_code(gate_guard.descendant_handle.as_ref().unwrap())
            .expect("query descendant before root exit"),
        None,
        "gated descendant must still be running before the root exits"
    );

    std::fs::write(&root_gate, b"exit").expect("release direct child exit");
    let exited = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let sessions = service
                .list(SessionFilter {
                    owner: Some(SessionOwner::Mcp),
                    target: None,
                })
                .await
                .expect("list isolated session");
            if let Some(info) = sessions
                .into_iter()
                .find(|info| info.session_id == session.session_id)
                && info.lifecycle == SessionLifecycle::Exited
            {
                break info;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SessionManager observed native root child exit");
    assert_eq!(
        exited.exit_status.as_ref().and_then(|status| status.code),
        Some(0)
    );

    std::fs::write(&descendant_gate, b"write-tail").expect("release descendant tail output");
    let after_exit = wait_for_text(
        service.as_ref(),
        session.session_id.clone(),
        "KERMINAL_CONPTY_DESCENDANT_AFTER_ROOT_EXIT",
    )
    .await;
    assert!(
        after_exit.contains("KERMINAL_CONPTY_DESCENDANT_AFTER_ROOT_EXIT"),
        "SessionManager dropped output after publishing the direct child's Exited state"
    );

    // descendant 仍持有 ConPTY 时显式 close 必须先终止 Job 才能 join 可能等待 detach 的 Release。
    service
        .close(session.session_id)
        .await
        .expect("close the descendant while natural Release may be waiting for detach");
    let descendant_exit_code = wait_for_native_exit_code(
        gate_guard.descendant_handle.as_ref().unwrap(),
        tokio::time::Instant::now() + Duration::from_secs(8),
    )
    .await
    .expect("wait on the same descendant process object after close");
    assert!(
        descendant_exit_code.is_some(),
        "close must terminate the gated descendant within the bounded wait"
    );
}
