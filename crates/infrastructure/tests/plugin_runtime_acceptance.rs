//! 真实 Windows Sidecar 验证重启预算和心跳；独立数据根不读取用户凭据或占用桌面。
//! @author kongweiguang
#![cfg(windows)]

use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginProcessPort};
use kerminal_domain::plugins::{PluginId, PluginRuntimeState, PluginVersion};
use kerminal_infrastructure::plugins::{process::PluginProcessSpec, supervisor::ProcessSupervisor};
use serde_json::json;
use std::{
    ffi::OsString,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// 显式定位 pwsh，避免 PATH 差异让测试错误调用 Windows PowerShell 或用户脚本。
fn powershell() -> PathBuf {
    let result = Command::new("where.exe")
        .creation_flags(0x08000000)
        .arg("pwsh.exe")
        .output()
        .expect("locate pwsh");
    let paths = String::from_utf8(result.stdout).expect("UTF-8 executable path");
    PathBuf::from(paths.lines().next().expect("PowerShell 7 installed"))
}

/// 测试进程使用生产监督器和受限环境；不通过 mock 缩短退避或心跳时间。
fn supervisor(root: &Path, script: &str) -> ProcessSupervisor {
    ProcessSupervisor::new(
        vec![PluginProcessSpec {
            id: PluginId::parse("acme.notes").expect("plugin id"),
            version: PluginVersion::parse("1.0.0").expect("plugin version"),
            executable: powershell(),
            arguments: vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(script),
            ],
            data_directory: root.join("data"),
            log_directory: root.join("logs"),
            locale: "en".into(),
            theme: "dark".into(),
            host_api: "1.0".into(),
        }],
        ApplicationTaskExecutor::new(tokio::runtime::Handle::current()),
    )
    .expect("supervisor")
}

/// 前四次握手后退出，只有人工重试产生的第五代正常服务，证明三次自动重启是有界的。
#[tokio::test]
async fn three_automatic_restarts_exhaust_budget_and_manual_retry_recovers() {
    let root = tempfile::tempdir().expect("isolated process root");
    let supervisor = supervisor(
        root.path(),
        r#"
$ErrorActionPreference = 'Stop'
$counter = Join-Path $env:KERMINAL_PLUGIN_DATA 'starts'
$count = 1
if (Test-Path $counter) { $count = [int]([IO.File]::ReadAllText($counter)) + 1 }
[IO.File]::WriteAllText($counter, $count.ToString())
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $request = $line | ConvertFrom-Json
  if ($request.method -eq 'initialize') {
    $result = @{protocol=1;plugin=@{id='acme.notes';version='1.0.0'}}
  } elseif ($request.method -eq 'initialized') {
    if ($count -le 4) { exit 1 }
    continue
  } elseif ($request.method -eq '$/ping' -or $request.method -eq 'shutdown') {
    $result = @{}
  } else { continue }
  [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=$result} | ConvertTo-Json -Compress -Depth 8))
  if ($request.method -eq 'shutdown') { exit 0 }
}
"#,
    );
    let id = PluginId::parse("acme.notes").expect("plugin id");
    supervisor
        .activate(&id, Instant::now() + Duration::from_secs(10))
        .await
        .expect("initial handshake");
    tokio::time::timeout(Duration::from_secs(45), async {
        while supervisor.state(&id).expect("runtime state") != PluginRuntimeState::Failed {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("fourth crash reaches Failed after three automatic retries");
    assert_eq!(
        std::fs::read_to_string(root.path().join("data/starts")).expect("start counter"),
        "4"
    );
    assert!(
        supervisor
            .activate(&id, Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    let generation = supervisor
        .retry(&id, Instant::now() + Duration::from_secs(10))
        .await
        .expect("explicit retry");
    assert_eq!(generation, 5);
    assert_eq!(
        supervisor
            .call(
                &id,
                generation,
                "$/ping",
                json!({}),
                Instant::now() + Duration::from_secs(5),
                CancellationToken::new()
            )
            .await
            .expect("recovered ping"),
        json!({})
    );
    supervisor
        .shutdown_until(Instant::now() + Duration::from_secs(6))
        .await
        .expect("reap retry process");
}

/// 第一个真实进程不响应心跳，监督器按生产时间回收它并启动能够回复的下一代。
#[tokio::test]
async fn missed_heartbeat_reaps_process_and_recovers_next_generation() {
    let root = tempfile::tempdir().expect("isolated heartbeat root");
    let supervisor = supervisor(
        root.path(),
        r#"
$ErrorActionPreference = 'Stop'
$marker = Join-Path $env:KERMINAL_PLUGIN_DATA 'heartbeat-started'
$first = -not (Test-Path $marker)
if ($first) { [IO.File]::WriteAllText((Join-Path $env:KERMINAL_PLUGIN_DATA 'heartbeat-first.pid'), $PID.ToString()) }
[IO.File]::WriteAllText($marker, 'started')
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $request = $line | ConvertFrom-Json
  if ($request.method -eq 'initialize') {
    $result = @{protocol=1;plugin=@{id='acme.notes';version='1.0.0'}}
  } elseif ($request.method -eq '$/ping') {
    if ($first) { continue }
    $result = @{}
  } elseif ($request.method -eq 'shutdown') { $result = @{} }
  else { continue }
  [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=$result} | ConvertTo-Json -Compress -Depth 8))
  if ($request.method -eq 'shutdown') { exit 0 }
}
"#,
    );
    let id = PluginId::parse("acme.notes").expect("plugin id");
    let original = supervisor
        .activate(&id, Instant::now() + Duration::from_secs(10))
        .await
        .expect("initial handshake");
    let recovered = tokio::time::timeout(Duration::from_secs(48), async {
        loop {
            if let PluginRuntimeState::Ready { generation } =
                supervisor.state(&id).expect("runtime state")
                && generation > original
            {
                break generation;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("heartbeat failure must cause recovery");
    let first_pid = std::fs::read_to_string(root.path().join("data/heartbeat-first.pid"))
        .expect("first heartbeat process identity")
        .parse::<u32>()
        .expect("process id");
    let check = format!(
        "if (Get-Process -Id {first_pid} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}"
    );
    assert!(
        Command::new(powershell())
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-NonInteractive", "-Command", &check])
            .status()
            .expect("check old process")
            .success(),
        "the unresponsive process remained alive"
    );
    assert!(
        supervisor
            .call(
                &id,
                original,
                "$/ping",
                json!({}),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    supervisor
        .call(
            &id,
            recovered,
            "$/ping",
            json!({}),
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .expect("new generation responds");
    supervisor
        .shutdown_until(Instant::now() + Duration::from_secs(6))
        .await
        .expect("reap heartbeat process");
}
