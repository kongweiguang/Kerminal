//! 双向 RPC 的帧限制、背压、取消和来源身份回归。
//! @author kongweiguang

// process/supervisor 只服务于 Windows 的真实进程回收用例；Unix 上无条件导入会触发
// `-D warnings` 的 unused_imports，因此按平台收窄，rpc 仍供跨平台用例使用。
use kerminal_infrastructure::plugins::rpc;
#[cfg(windows)]
use kerminal_infrastructure::plugins::{process, supervisor};

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::plugin_ports::{
    PluginHostError, PluginHostRouter, plugin_business_error,
};
use kerminal_domain::plugins::PluginId;
use kerminal_plugin_protocol::{MAX_PENDING_REQUESTS, MAX_RPC_BYTES, error_code};
use rpc::RpcConnection;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split},
    sync::{Notify, Semaphore},
};
use tokio_util::sync::CancellationToken;

struct Router {
    entered: AtomicUsize,
    release: Notify,
}

#[async_trait]
impl PluginHostRouter for Router {
    /// 阻塞回调以观察双向请求配额，同时核对身份由宿主提供。
    async fn route(
        &self,
        plugin: &PluginId,
        generation: u64,
        _: &str,
        _: Value,
        token: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        assert_eq!(plugin.as_str(), "acme.notes");
        assert_eq!(generation, 7);
        self.entered.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            _ = self.release.notified() => Ok(json!({})),
            _ = token.cancelled() => Err(PluginHostError { code: error_code::CANCELLED, message: "cancelled".into() }),
        }
    }
}

/// 在内存双工管道运行真实连接，避免依赖本地桌面或用户配置。
fn connection(
    router: Option<Arc<dyn PluginHostRouter>>,
) -> (
    RpcConnection,
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
) {
    let (host, plugin) = tokio::io::duplex(MAX_RPC_BYTES + 1024);
    let (host_reader, host_writer) = split(host);
    let (plugin_reader, plugin_writer) = split(plugin);
    let connection = RpcConnection::from_streams(
        PluginId::parse("acme.notes").expect("id"),
        7,
        host_reader,
        host_writer,
        router.as_ref().map(Arc::downgrade),
    );
    (connection, plugin_reader, plugin_writer)
}

/// 接收完整一行协议帧，超时明确使测试失败。
async fn next_frame(reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .expect("read timeout")
        .expect("frame read");
    serde_json::from_str(&line).expect("JSON frame")
}

#[tokio::test]
/// 超额请求不得排队占用无限内存，取消已提交请求必须发对应 ID。
async fn host_backpressure_and_cancellation_are_bounded() {
    let (connection, reader, _writer) = connection(None);
    let mut reader = BufReader::new(reader);
    let mut requests = Vec::new();
    for _ in 0..MAX_PENDING_REQUESTS {
        let connection = connection.clone();
        requests.push(tokio::spawn(async move {
            connection
                .request(
                    "view.action",
                    json!({}),
                    Instant::now() + Duration::from_secs(10),
                    CancellationToken::new(),
                )
                .await
        }));
    }
    let mut ids = Vec::new();
    for _ in 0..MAX_PENDING_REQUESTS {
        ids.push(
            next_frame(&mut reader).await["id"]
                .as_str()
                .expect("request id")
                .to_owned(),
        );
    }
    let overloaded = connection
        .request(
            "view.action",
            json!({}),
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await;
    assert!(overloaded.is_err());
    requests.remove(0).abort();
    let cancel = next_frame(&mut reader).await;
    assert_eq!(cancel["method"], "$/cancel");
    assert!(
        ids.contains(
            &cancel["params"]["id"]
                .as_str()
                .expect("cancel id")
                .to_owned()
        )
    );
    connection.close();
    for request in requests {
        let _ = request.await;
    }
}

#[tokio::test]
/// 插件侧同样最多保留 64 个回调，多余请求应收到标准 overloaded。
async fn plugin_backpressure_returns_protocol_error() {
    let router = Arc::new(Router {
        entered: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let (connection, reader, mut writer) = connection(Some(router.clone()));
    let mut reader = BufReader::new(reader);
    for id in 0..=MAX_PENDING_REQUESTS {
        let mut frame = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":format!("p:{id}"),"method":"settings.get","params":{}}),
        )
        .expect("encode");
        frame.push(b'\n');
        writer.write_all(&frame).await.expect("write");
    }
    let response = next_frame(&mut reader).await;
    assert_eq!(response["id"], "p:64");
    assert_eq!(response["error"]["code"], error_code::OVERLOADED);
    connection.close();
}

#[tokio::test]
/// 超限无换行帧会关闭连接，不继续分配或接受后续消息。
async fn oversized_unterminated_frame_closes_connection() {
    let (connection, _reader, mut writer) = connection(None);
    writer
        .write_all(&vec![b'x'; MAX_RPC_BYTES + 1])
        .await
        .expect("write oversized frame");
    tokio::time::timeout(Duration::from_secs(3), connection.closed())
        .await
        .expect("connection closed");
}

#[tokio::test]
/// 前缀错误的回执不会匹配宿主请求，累计三次协议错误后连接关闭。
async fn invalid_response_identity_eventually_closes_connection() {
    let (connection, _reader, mut writer) = connection(None);
    for _ in 0..3 {
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"p:wrong\",\"result\":{}}\n")
            .await
            .expect("write invalid frame");
    }
    tokio::time::timeout(Duration::from_secs(3), connection.closed())
        .await
        .expect("connection closed");
}

#[tokio::test]
/// 绝对截止时间过后不得把带副作用的请求或通知放入发送队列。
async fn expired_deadlines_never_send_frames() {
    let (connection, reader, _writer) = connection(None);
    let mut reader = BufReader::new(reader);
    let deadline = Instant::now() - Duration::from_millis(1);
    assert!(
        connection
            .request("view.action", json!({}), deadline, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        connection
            .notify("view.visibility", json!({}), deadline)
            .await
            .is_err()
    );
    let mut line = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
            .await
            .is_err()
    );
    connection.close();
}

#[tokio::test]
/// 已取消的排队请求不能在优先取消帧之后被写出并意外执行。
async fn cancelled_queued_request_is_never_sent_later() {
    let (host, plugin) = tokio::io::duplex(64);
    let (host_reader, host_writer) = split(host);
    let (plugin_reader, _plugin_writer) = split(plugin);
    let connection = RpcConnection::from_streams(
        PluginId::parse("acme.notes").expect("id"),
        7,
        host_reader,
        host_writer,
        None,
    );
    let mut reader = BufReader::new(plugin_reader);
    let first_connection = connection.clone();
    let first = tokio::spawn(async move {
        first_connection
            .request(
                "view.action",
                json!({"payload":"x".repeat(256 * 1024)}),
                Instant::now() + Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let token = CancellationToken::new();
    let second_token = token.clone();
    let second_connection = connection.clone();
    let second = tokio::spawn(async move {
        second_connection
            .request(
                "view.action",
                json!({"side_effect":"must-not-run"}),
                Instant::now() + Duration::from_secs(10),
                second_token,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    token.cancel();
    assert!(second.await.expect("second task").is_err());
    let initial = next_frame(&mut reader).await;
    assert_eq!(initial["method"], "view.action");
    let cancel = next_frame(&mut reader).await;
    assert_eq!(cancel["method"], "$/cancel");
    assert_ne!(cancel["params"]["id"], initial["id"]);
    let mut extra = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut extra))
            .await
            .is_err()
    );
    connection.close();
    let _ = first.await;
}

#[cfg(windows)]
mod windows_process {
    use super::*;
    use crate::{process::PluginProcessSpec, supervisor::ProcessSupervisor};
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginProcessPort};
    use kerminal_domain::plugins::{PluginRuntimeState, PluginVersion};
    use std::{ffi::OsString, os::windows::process::CommandExt, path::PathBuf, process::Command};

    /// 使用测试宿主已安装的 pwsh 作为真实 Sidecar，避免假造进程回收结果。
    fn powershell() -> PathBuf {
        let output = Command::new("where.exe")
            .creation_flags(0x08000000)
            .arg("pwsh.exe")
            .output()
            .expect("locate pwsh");
        let path = String::from_utf8(output.stdout).expect("UTF-8 path");
        PathBuf::from(path.lines().next().expect("pwsh path"))
    }

    /// 完整握手后创建受同一 Job 管理的子进程，关闭请求会使直接进程正常退出。
    fn fixture_script() -> &'static str {
        r#"
$ErrorActionPreference = 'Stop'
$env:SystemRoot = [Environment]::GetFolderPath('Windows')
$child = Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep 60' -WindowStyle Hidden -PassThru
[IO.File]::WriteAllText((Join-Path $env:KERMINAL_PLUGIN_DATA 'child.pid'), $child.Id.ToString())
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $request = $line | ConvertFrom-Json
  if ($request.method -eq 'initialize') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{protocol=1;plugin=@{id='acme.notes';version='1.0.0'}}} | ConvertTo-Json -Compress -Depth 8))
  } elseif ($request.method -eq 'view.action') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{ok=$true}} | ConvertTo-Json -Compress -Depth 8))
  } elseif ($request.method -eq 'shutdown') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{}} | ConvertTo-Json -Compress -Depth 8))
    exit 0
  }
}
"#
    }

    /// 给真实监督器提供隔离数据与日志目录，不读取或覆盖用户目录。
    fn spec(root: &std::path::Path) -> PluginProcessSpec {
        PluginProcessSpec {
            id: PluginId::parse("acme.notes").expect("id"),
            version: PluginVersion::parse("1.0.0").expect("version"),
            executable: powershell(),
            arguments: vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(fixture_script()),
            ],
            data_directory: root.join("data"),
            log_directory: root.join("logs"),
            locale: "zh-CN".into(),
            theme: "dark".into(),
            host_api: "1.0".into(),
        }
    }

    #[tokio::test]
    /// 握手、调用和关闭经过实际 Windows Job，结束后子进程也必须消失。
    async fn shutdown_reaps_the_real_windows_process_tree() {
        let root = tempfile::tempdir().expect("temporary root");
        let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
        let supervisor =
            ProcessSupervisor::new(vec![spec(root.path())], executor).expect("supervisor");
        let id = PluginId::parse("acme.notes").expect("id");
        let generation = supervisor
            .activate(&id, Instant::now() + Duration::from_secs(10))
            .await
            .expect("handshake");
        let response = supervisor
            .call(
                &id,
                generation,
                "view.action",
                json!({}),
                Instant::now() + Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .expect("action");
        assert_eq!(response["ok"], true);
        let pid = std::fs::read_to_string(root.path().join("data/child.pid")).expect("child pid");
        supervisor
            .shutdown_until(Instant::now() + Duration::from_secs(6))
            .await
            .expect("shutdown and wait");
        assert_eq!(
            supervisor.state(&id).expect("state"),
            PluginRuntimeState::Idle
        );
        let check = format!(
            "if (Get-Process -Id {} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}",
            pid.trim()
        );
        let status = Command::new(powershell())
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-NonInteractive", "-Command", &check])
            .status()
            .expect("check child");
        assert!(status.success(), "child process remained after shutdown");
    }

    #[tokio::test]
    /// 开发重启必须回收旧进程、分配新代次，并拒绝旧代次的迟到调用。
    async fn restart_reaps_old_generation_before_starting_new_process() {
        let root = tempfile::tempdir().expect("temporary root");
        let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
        let supervisor =
            ProcessSupervisor::new(vec![spec(root.path())], executor).expect("supervisor");
        let id = PluginId::parse("acme.notes").expect("id");
        let first = supervisor
            .activate(&id, Instant::now() + Duration::from_secs(10))
            .await
            .expect("first handshake");
        let next = supervisor
            .restart(&id, Instant::now() + Duration::from_secs(15))
            .await
            .expect("restart");
        assert!(next > first);
        assert!(
            supervisor
                .call(
                    &id,
                    first,
                    "view.action",
                    json!({}),
                    Instant::now() + Duration::from_secs(3),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            supervisor
                .call(
                    &id,
                    next,
                    "view.action",
                    json!({}),
                    Instant::now() + Duration::from_secs(3),
                    CancellationToken::new()
                )
                .await
                .expect("new generation")["ok"],
            true
        );
        supervisor
            .shutdown_until(Instant::now() + Duration::from_secs(6))
            .await
            .expect("shutdown");
    }

    /// 替换隔离测试进程的脚本，仍通过相同监督器和系统管道执行握手。
    fn spec_with_script(root: &std::path::Path, script: &str) -> PluginProcessSpec {
        let mut spec = spec(root);
        spec.arguments[3] = OsString::from(script);
        spec
    }

    /// 只等待夹具主动写入的 PID 文件，避免用固定延时猜测握手是否开始。
    async fn wait_for_pid(root: &std::path::Path) -> String {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(pid) = tokio::fs::read_to_string(root.join("data/self.pid")).await {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fixture pid")
    }

    /// 按系统进程表确认直接进程已消失，关闭回执不能替代实际回收。
    fn assert_reaped(pid: &str) {
        let check = format!(
            "if (Get-Process -Id {} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}",
            pid.trim()
        );
        let status = Command::new(powershell())
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-NonInteractive", "-Command", &check])
            .status()
            .expect("check process");
        assert!(status.success(), "fixture process remained after shutdown");
    }

    #[tokio::test]
    /// 握手中的错误身份不能成为就绪进程，失败后仍须保留 owner 并实际回收。
    async fn handshake_identity_mismatch_is_rejected_and_reaped() {
        let root = tempfile::tempdir().expect("temporary root");
        let script = r#"
$ErrorActionPreference = 'Stop'
[IO.File]::WriteAllText((Join-Path $env:KERMINAL_PLUGIN_DATA 'self.pid'), $PID.ToString())
$line = [Console]::In.ReadLine() | ConvertFrom-Json
[Console]::Out.WriteLine((@{jsonrpc='2.0';id=$line.id;result=@{protocol=1;plugin=@{id='wrong.plugin';version='1.0.0'}}} | ConvertTo-Json -Compress -Depth 8))
Start-Sleep -Seconds 60
"#;
        let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
        let supervisor =
            ProcessSupervisor::new(vec![spec_with_script(root.path(), script)], executor)
                .expect("supervisor");
        let id = PluginId::parse("acme.notes").expect("id");
        assert!(
            supervisor
                .activate(&id, Instant::now() + Duration::from_secs(10))
                .await
                .is_err()
        );
        let pid = wait_for_pid(root.path()).await;
        supervisor
            .shutdown_until(Instant::now() + Duration::from_secs(6))
            .await
            .expect("reap failed handshake");
        assert_reaped(&pid);
    }

    #[tokio::test]
    /// 关闭期间可以打断尚未回复的握手，不能遗失已经创建的子进程所有权。
    async fn shutdown_interrupts_inflight_handshake_and_reaps_process() {
        let root = tempfile::tempdir().expect("temporary root");
        let script = r#"
$ErrorActionPreference = 'Stop'
[IO.File]::WriteAllText((Join-Path $env:KERMINAL_PLUGIN_DATA 'self.pid'), $PID.ToString())
$null = [Console]::In.ReadLine()
Start-Sleep -Seconds 60
"#;
        let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
        let supervisor = Arc::new(
            ProcessSupervisor::new(vec![spec_with_script(root.path(), script)], executor)
                .expect("supervisor"),
        );
        let id = PluginId::parse("acme.notes").expect("id");
        let pending_supervisor = supervisor.clone();
        let activation = tokio::spawn(async move {
            pending_supervisor
                .activate(&id, Instant::now() + Duration::from_secs(12))
                .await
        });
        let pid = wait_for_pid(root.path()).await;
        supervisor
            .shutdown_until(Instant::now() + Duration::from_secs(6))
            .await
            .expect("shutdown during handshake");
        assert!(activation.await.expect("activation task").is_err());
        assert_reaped(&pid);
    }

    #[tokio::test]
    /// 首次崩溃后的自动恢复必须产生新代次，旧代次请求不能被重放到新进程。
    async fn crashed_process_restarts_with_a_new_generation() {
        let root = tempfile::tempdir().expect("temporary root");
        let script = r#"
$ErrorActionPreference = 'Stop'
$marker = Join-Path $env:KERMINAL_PLUGIN_DATA 'restarted'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $request = $line | ConvertFrom-Json
  if ($request.method -eq 'initialize') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{protocol=1;plugin=@{id='acme.notes';version='1.0.0'}}} | ConvertTo-Json -Compress -Depth 8))
  } elseif ($request.method -eq 'initialized' -and -not (Test-Path $marker)) {
    [IO.File]::WriteAllText($marker, 'first')
    exit 1
  } elseif ($request.method -eq 'shutdown') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{}} | ConvertTo-Json -Compress -Depth 8))
    exit 0
  }
}
"#;
        let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
        let supervisor =
            ProcessSupervisor::new(vec![spec_with_script(root.path(), script)], executor)
                .expect("supervisor");
        let id = PluginId::parse("acme.notes").expect("id");
        let original = supervisor
            .activate(&id, Instant::now() + Duration::from_secs(10))
            .await
            .expect("first handshake");
        let recovered = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if let Ok(PluginRuntimeState::Ready { generation }) = supervisor.state(&id)
                    && generation > original
                {
                    break generation;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("automatic recovery");
        assert!(recovered > original);
        assert!(
            supervisor
                .call(
                    &id,
                    original,
                    "view.action",
                    json!({}),
                    Instant::now() + Duration::from_secs(2),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        supervisor
            .shutdown_until(Instant::now() + Duration::from_secs(6))
            .await
            .expect("shutdown");
    }
}

struct OrderedRouter {
    entries: Arc<Mutex<Vec<String>>>,
    started: Semaphore,
    release: Semaphore,
    finished: Semaphore,
}

#[async_trait]
impl PluginHostRouter for OrderedRouter {
    /// 同步补丁将标签写入共享序列，模拟应用层在读取器线程内提交。
    fn ordered_view_update(
        &self,
        _: &PluginId,
        _: u64,
        params: Value,
    ) -> Result<Value, PluginHostError> {
        let mark = params["mark"].as_str().expect("mark");
        self.entries.lock().expect("entries").push(mark.into());
        Ok(json!({}))
    }

    /// 第一条普通通知等待测试释放，验证后续通知不能抢先完成。
    async fn route(
        &self,
        _: &PluginId,
        _: u64,
        _: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        let mark = params["mark"].as_str().expect("mark");
        self.entries
            .lock()
            .expect("entries")
            .push(format!("{mark}-start"));
        if mark == "first" {
            self.started.add_permits(1);
            self.release.acquire().await.expect("release").forget();
        }
        self.entries
            .lock()
            .expect("entries")
            .push(format!("{mark}-end"));
        self.finished.add_permits(1);
        Ok(json!({}))
    }
}

/// 使用独立同步信号避免依赖调度时序或固定睡眠。
fn ordered_router() -> Arc<OrderedRouter> {
    Arc::new(OrderedRouter {
        entries: Arc::new(Mutex::new(Vec::new())),
        started: Semaphore::new(0),
        release: Semaphore::new(0),
        finished: Semaphore::new(0),
    })
}

/// 按真实换行分帧发送插件消息，测试不绕过连接的解码路径。
async fn send_frame(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, frame: Value) {
    let mut bytes = serde_json::to_vec(&frame).expect("frame");
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("send frame");
}

#[tokio::test]
/// 插件推送、请求回执与后续推送都按输入管道的顺序提交。
async fn view_updates_and_observed_reply_follow_frame_order() {
    let router = ordered_router();
    let (connection, reader, mut writer) = connection(Some(router.clone()));
    let mut reader = BufReader::new(reader);
    let waiting = connection.clone();
    let entries = router.entries.clone();
    let call = tokio::spawn(async move {
        waiting
            .request_observed(
                "view.open",
                json!({}),
                Instant::now() + Duration::from_secs(3),
                CancellationToken::new(),
                Box::new(move |_| {
                    entries.lock().expect("entries").push("reply".into());
                    Ok(())
                }),
            )
            .await
    });
    let id = next_frame(&mut reader).await["id"].clone();
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"view.update","params":{"mark":"before"}}),
    )
    .await;
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":"p:1","method":"view.update","params":{"mark":"request"}}),
    )
    .await;
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"result":{"data":{}}}),
    )
    .await;
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":"p:2","method":"view.update","params":{"mark":"after"}}),
    )
    .await;
    assert_eq!(next_frame(&mut reader).await["id"], "p:1");
    assert_eq!(next_frame(&mut reader).await["id"], "p:2");
    call.await.expect("join").expect("reply");
    assert_eq!(
        *router.entries.lock().expect("entries"),
        ["before", "request", "reply", "after"]
    );
    connection.close();
}

#[tokio::test]
/// 即使第一条通知执行缓慢，第二条也不能越过它；其他同步帧仍可被读取。
async fn ordinary_notifications_are_serialized() {
    let router = ordered_router();
    let (connection, reader, mut writer) = connection(Some(router.clone()));
    let mut reader = BufReader::new(reader);
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"log","params":{"mark":"first"}}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), router.started.acquire())
        .await
        .expect("first started")
        .unwrap()
        .forget();
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"log","params":{"mark":"second"}}),
    )
    .await;
    send_frame(&mut writer, json!({"jsonrpc":"2.0","id":"p:barrier","method":"view.update","params":{"mark":"barrier"}})).await;
    assert_eq!(next_frame(&mut reader).await["id"], "p:barrier");
    assert_eq!(
        *router.entries.lock().expect("entries"),
        ["first-start", "barrier"]
    );
    router.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), router.finished.acquire_many(2))
        .await
        .expect("finished")
        .unwrap()
        .forget();
    assert_eq!(
        *router.entries.lock().expect("entries"),
        [
            "first-start",
            "barrier",
            "first-end",
            "second-start",
            "second-end"
        ]
    );
    connection.close();
}

#[tokio::test]
/// 通知队列耗尽时关闭连接，避免无界累积或伪装成已处理。
async fn overflowing_notification_queue_closes_connection() {
    let router = ordered_router();
    let (connection, _reader, mut writer) = connection(Some(router.clone()));
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"log","params":{"mark":"first"}}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), router.started.acquire())
        .await
        .expect("first started")
        .unwrap()
        .forget();
    for _ in 0..=MAX_PENDING_REQUESTS {
        send_frame(
            &mut writer,
            json!({"jsonrpc":"2.0","method":"log","params":{"mark":"queued"}}),
        )
        .await;
    }
    tokio::time::timeout(Duration::from_secs(3), connection.closed())
        .await
        .expect("overflow closes");
}

#[tokio::test]
/// 仅业务码允许显式提取限长清理文案，普通错误及诊断展示保持脱敏。
async fn remote_business_errors_preserve_only_sanitized_explicit_text() {
    let (connection, reader, mut writer) = connection(None);
    let mut reader = BufReader::new(reader);
    let waiting = connection.clone();
    let call = tokio::spawn(async move {
        waiting
            .request(
                "view.action",
                json!({}),
                Instant::now() + Duration::from_secs(3),
                CancellationToken::new(),
            )
            .await
    });
    let id = next_frame(&mut reader).await["id"].clone();
    let text = format!("提示\n\u{202e}\u{061c}\u{200f}{}", "字".repeat(250));
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32100,"message":text}}),
    )
    .await;
    let error = call.await.expect("join").expect_err("business error");
    let business = plugin_business_error(&error).expect("business source");
    assert_eq!(business.code(), -32100);
    assert!(business.message().starts_with("提示 "));
    assert_eq!(business.message().chars().count(), 200);
    assert!(
        !business
            .message()
            .contains(['\u{202e}', '\u{061c}', '\u{200f}'])
    );
    assert!(!business.message().chars().any(char::is_control));
    assert!(!format!("{error}").contains("提示"));
    assert!(!format!("{error:?}").contains("提示"));

    let waiting = connection.clone();
    let call = tokio::spawn(async move {
        waiting
            .request(
                "view.action",
                json!({}),
                Instant::now() + Duration::from_secs(3),
                CancellationToken::new(),
            )
            .await
    });
    let id = next_frame(&mut reader).await["id"].clone();
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"sensitive detail"}}),
    )
    .await;
    let error = call.await.expect("join").expect_err("generic error");
    assert!(plugin_business_error(&error).is_none());
    assert!(!format!("{error:?}").contains("sensitive"));
    connection.close();
}

#[tokio::test]
/// 取消后的迟到回执不得触发观察器，即使连接随后仍可接收视图更新。
async fn cancelled_reply_never_runs_observer() {
    let router = ordered_router();
    let (connection, reader, mut writer) = connection(Some(router.clone()));
    let mut reader = BufReader::new(reader);
    let token = CancellationToken::new();
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    let waiting = connection.clone();
    let call_token = token.clone();
    let call = tokio::spawn(async move {
        waiting
            .request_observed(
                "view.action",
                json!({}),
                Instant::now() + Duration::from_secs(3),
                call_token,
                Box::new(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            )
            .await
    });
    let id = next_frame(&mut reader).await["id"].clone();
    token.cancel();
    assert!(call.await.expect("join").is_err());
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"result":{"patch":{}}}),
    )
    .await;
    send_frame(&mut writer, json!({"jsonrpc":"2.0","id":"p:barrier","method":"view.update","params":{"mark":"barrier"}})).await;
    loop {
        let frame = next_frame(&mut reader).await;
        if frame["id"] == "p:barrier" {
            assert_eq!(frame["result"], json!({}));
            break;
        }
    }
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    connection.close();
}

#[tokio::test]
/// 每个脚本执行中断计入同一协议错误预算，第三次错误关闭连接。
async fn script_execution_interruptions_share_protocol_error_budget() {
    let (connection, _reader, mut writer) = connection(None);
    for _ in 0..2 {
        send_frame(
            &mut writer,
            json!({"jsonrpc":"2.0","method":"$/executionTimeout","params":{}}),
        )
        .await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), connection.closed())
            .await
            .is_err()
    );
    writer.write_all(b"not-json\n").await.expect("bad frame");
    tokio::time::timeout(Duration::from_secs(3), connection.closed())
        .await
        .expect("third protocol error closes");
}

struct DelayedCleanupRouter {
    entered: Semaphore,
    cancelled: Semaphore,
    release: Semaphore,
}

#[async_trait]
impl PluginHostRouter for DelayedCleanupRouter {
    /// 模拟取消后仍需异步清理的宿主调用，以检验关闭回执不会抢在资源释放之前。
    async fn route(
        &self,
        _: &PluginId,
        _: u64,
        _: &str,
        _: Value,
        token: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        self.entered.add_permits(1);
        token.cancelled().await;
        self.cancelled.add_permits(1);
        self.release.acquire().await.expect("release open").forget();
        Err(PluginHostError {
            code: error_code::CANCELLED,
            message: "cancelled".into(),
        })
    }
}

#[tokio::test]
/// 超时不得声称宿主回调已经回收；释放资源后可用新的截止时间重试。
async fn close_waits_for_cancelled_host_request_cleanup() {
    let router = Arc::new(DelayedCleanupRouter {
        entered: Semaphore::new(0),
        cancelled: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let (connection, _reader, mut writer) = connection(Some(router.clone()));
    send_frame(
        &mut writer,
        json!({"jsonrpc":"2.0","id":"p:cleanup","method":"settings.get","params":{}}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), router.entered.acquire())
        .await
        .expect("request entered")
        .expect("entered open")
        .forget();
    assert!(
        connection
            .close_until(Instant::now() + Duration::from_millis(50))
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(3), router.cancelled.acquire())
        .await
        .expect("cancellation delivered")
        .expect("cancelled open")
        .forget();
    router.release.add_permits(1);
    connection
        .close_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("cleanup completed");
}
