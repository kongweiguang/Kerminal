// @author kongweiguang

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::{Value, json};

struct Host {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    data_dir: std::path::PathBuf,
    _temporary_data: Option<tempfile::TempDir>,
}

impl Host {
    /// 独立子进程真实经过 stdio，避免只验证与实现相同的内部辅助函数。
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("isolated plugin data");
        let mut host = Self::start_with_data(directory.path());
        host._temporary_data = Some(directory);
        host
    }

    /// 允许重启测试复用同一目录，同时由调用方控制该目录的生命周期。
    fn start_with_data(data_dir: &std::path::Path) -> Self {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/plugins/scripts/p0");
        Self::start_fixture(&fixture, data_dir, false)
    }

    /// 官方示例和测试夹具都经相同进程启动路径，来源只由测试组合根指定。
    fn start_fixture(
        fixture: &std::path::Path,
        data_dir: &std::path::Path,
        official: bool,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin-host"));
        command.arg(fixture);
        if official {
            command.arg("--official");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start plugin host");
        let input = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("read stdout");
                let value = serde_json::from_str(&line).expect("stdout must contain only JSON");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            data_dir: data_dir.to_path_buf(),
            _temporary_data: None,
        }
    }

    /// 帧立即刷新，测试才能观察真实的双向等待而非缓冲区偶然刷新。
    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").expect("write frame");
        self.input.flush().expect("flush frame");
    }

    /// 等待有上限，脚本死锁或遗漏回执会明确失败。
    fn receive(&self) -> Value {
        self.output
            .recv_timeout(Duration::from_secs(5))
            .expect("receive frame")
    }

    /// 测试请求逐次等待对应回执，避免把异步完成误认成另一个操作。
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":"h:case","method":method,"params":params}));
        let frame = self.receive();
        assert_eq!(frame["id"], "h:case", "{frame}");
        assert!(frame.get("error").is_none(), "{frame}");
        frame["result"].clone()
    }

    /// 按协议完成初始化，后续测试只关注各自的动作。
    fn initialize(&mut self) {
        self.initialize_identity("acme.notes", "1.2.0");
    }

    /// 身份由测试的真实清单决定，避免官方示例沿用第三方夹具身份。
    fn initialize_identity(&mut self, id: &str, version: &str) {
        self.send(
            json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":{
                "protocol":1,"host_api":"1.0","plugin":{"id":id,"version":version},
                "locale":"zh-CN","theme":"dark","data_dir":self.data_dir,"generation":1
            }}),
        );
        assert_eq!(
            self.receive(),
            json!({"jsonrpc":"2.0","id":"h:1","result":{
                "protocol":1,"plugin":{"id":id,"version":version}
            }})
        );
        self.send(json!({"jsonrpc":"2.0","method":"initialized"}));
    }
}

impl Drop for Host {
    /// 失败时也回收子进程，避免测试泄漏等待 stdin 的宿主。
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 验证真实 ESM 插件可在动作处理中发起请求并等待宿主回执。
#[test]
fn handshake_view_update_and_shutdown_cross_real_stdio() {
    let mut host = Host::start();
    host.initialize();
    host.send(
        json!({"jsonrpc":"2.0","id":"h:2","method":"view.action","params":{
            "instance":"tab-1","action":"increment","data":{"count":1}
        }}),
    );
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"p:1","method":"view.update",
        "params":{"instance":"tab-1","patch":{"count":2}}})
    );
    host.send(json!({"jsonrpc":"2.0","id":"p:1","result":{"accepted":true}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:2","result":{
            "patch":{"count":2},"acknowledgement":{"accepted":true}
        }})
    );
    host.send(json!({"jsonrpc":"2.0","id":"h:3","method":"shutdown","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:3","result":{}})
    );
    assert!(host.child.wait().expect("wait for shutdown").success());
}

/// 失败的身份校验不能被当作初始化成功，调用方可以用正确身份重试。
#[test]
fn handshake_rejects_identity_mismatch() {
    let mut host = Host::start();
    host.send(
        json!({"jsonrpc":"2.0","id":"h:wrong","method":"initialize","params":{
            "protocol":1,"plugin":{"id":"other.plugin","version":"1.2.0"}
        }}),
    );
    let response = host.receive();
    assert_eq!(response["id"], "h:wrong");
    assert_eq!(response["error"]["code"], -32602);
    host.initialize();
}

/// 定时器取消和循环回调经过真实进程事件循环，而不是模拟时钟。
#[test]
fn timers_fire_and_cancel_without_blocking_rpc() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:timer","method":"test.timers","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:timer","result":{"ticks":3}})
    );
}

/// 死循环必须在限制时间内返回错误，随后仍能处理心跳。
#[test]
fn synchronous_execution_is_interrupted_and_heartbeat_recovers() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:spin","method":"test.spin","params":{}}));
    let result = host.receive();
    assert_eq!(result["id"], "h:spin");
    assert_eq!(result["error"]["code"], -32603);
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:ping","result":{}})
    );
}

/// 过大的 JS 堆分配必须失败，不能仅靠操作系统内存耗尽来终止。
#[test]
fn quickjs_heap_limit_rejects_excessive_allocation() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:heap","method":"test.heap","params":{}}));
    let result = host.receive();
    assert_eq!(result["id"], "h:heap");
    assert_eq!(result["error"]["code"], -32603);
}

/// 双向请求应传回宿主的权限错误，供插件区分拒绝与通信失败。
#[test]
fn host_rpc_preserves_permission_errors() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:rpc","method":"test.rpc","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"p:1","method":"clipboard.read","params":{}})
    );
    host.send(
        json!({"jsonrpc":"2.0","id":"p:1","error":{"code":-32001,"message":"permission denied"}}),
    );
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:rpc","result":{"code":-32001}})
    );
}

/// 取消只结束指定请求，迟到结果不得污染后续请求。
#[test]
fn cancellation_discards_late_result() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:hold","method":"test.hold","params":{}}));
    host.send(json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:hold"}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:hold","error":{"code":-32002,"message":"cancelled"}})
    );
    host.send(json!({"jsonrpc":"2.0","id":"h:release","method":"test.release","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:release","result":{}})
    );
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:ping","result":{}})
    );
}

/// 协议解析器拒绝未知字段和冒用官方命名空间，宿主不能另设宽松入口。
#[test]
fn invalid_manifest_is_rejected_before_handshake() {
    let template: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/plugins/scripts/p0/kerminal-plugin.json"
    ))
    .unwrap();
    for change in ["unknown", "official", "runtime"] {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = template.clone();
        match change {
            "unknown" => manifest["unrecognized"] = json!(true),
            "official" => {
                manifest["id"] = json!("kerminal.notes");
                manifest["publisher"] = json!("kerminal");
            }
            "runtime" => manifest["runtime"]["main"] = json!("../outside.js"),
            _ => unreachable!(),
        }
        std::fs::write(
            directory.path().join("kerminal-plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.path().join("main.js"), "").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin-host"))
            .arg(directory.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {change}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("plugin manifest"));
    }
}

/// ID 在取消后重用时，先前 Promise 的迟到结果不能结束新请求。
#[test]
fn cancelled_request_cannot_complete_a_reused_id() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:reuse","method":"test.hold","params":{}}));
    host.send(json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:reuse"}}));
    assert_eq!(host.receive()["error"]["code"], -32002);
    host.send(json!({"jsonrpc":"2.0","id":"h:reuse","method":"test.hold","params":{}}));
    host.send(json!({"jsonrpc":"2.0","id":"h:release1","method":"test.release","params":{}}));
    assert_eq!(host.receive()["id"], "h:release1");
    host.send(json!({"jsonrpc":"2.0","id":"h:release2","method":"test.release","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:reuse","result":{"released":true}})
    );
    assert_eq!(host.receive()["id"], "h:release2");
}

/// 插件声明的业务错误码保留给宿主，便于展示对应的处理信息。
#[test]
fn plugin_business_error_is_preserved() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:business","method":"test.business","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:business","error":{"code":-32101,"message":"invalid note"}})
    );
}

/// UTF-8 编码必须处理代理项，并保证有限缓冲区中不存在半个字符。
#[test]
fn text_encoder_handles_unicode_and_partial_output() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:text","method":"test.text","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:text","result":{
            "encoding":"utf-8",
            "bytes":[0xc3,0xa9,0xe4,0xb8,0xad,0xf0,0x9f,0x98,0x80,0xef,0xbf,0xbd],
            "partial":{"read":3,"written":5},
            "destination":[0x41,0xf0,0x9f,0x98,0x80,0]
        }})
    );
}

/// Promise 回调超时后请求应失败，事件循环仍须处理心跳。
#[test]
fn promise_execution_is_interrupted_and_heartbeat_recovers() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:promise","method":"test.promise-spin","params":{}}));
    let result = host.receive();
    assert_eq!(result["id"], "h:promise");
    assert_eq!(result["error"]["code"], -32603);
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(host.receive()["id"], "h:ping");
}

/// 单个计时器超时只结束该回调，不能拖垮进程和后续心跳。
#[test]
fn timer_execution_is_interrupted_and_heartbeat_recovers() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:timer-spin","method":"test.timer-spin","params":{}}));
    assert_eq!(host.receive()["id"], "h:timer-spin");
    thread::sleep(Duration::from_millis(100));
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(host.receive()["id"], "h:ping");
    thread::sleep(Duration::from_millis(100));
    host.send(json!({"jsonrpc":"2.0","id":"h:ping2","method":"$/ping","params":{}}));
    assert_eq!(
        host.output
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupted interval was not cancelled")["id"],
        "h:ping2"
    );
}

/// 微任务没有可靠的请求归属时，全部活动请求必须收到失败并丢弃迟到结果。
#[test]
fn interrupted_job_fails_all_active_requests_without_late_replies() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:hold","method":"test.hold","params":{}}));
    host.send(json!({"jsonrpc":"2.0","id":"h:promise","method":"test.promise-spin","params":{}}));
    let first = host.receive();
    let second = host.receive();
    assert_eq!(first["id"], "h:hold");
    assert_eq!(second["id"], "h:promise");
    assert_eq!(first["error"]["code"], -32603);
    assert_eq!(second["error"]["code"], -32603);
    host.send(json!({"jsonrpc":"2.0","id":"h:release","method":"test.release","params":{}}));
    assert_eq!(host.receive()["id"], "h:release");
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(host.receive()["id"], "h:ping");
}

/// 运行时没有文件、进程或动态标准模块入口，脚本只能使用受控桥接能力。
#[test]
fn script_cannot_access_host_process_or_standard_modules() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:sandbox","method":"test.sandbox","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:sandbox","result":{
            "require":"undefined","process":"undefined","fs":"undefined","moduleRejected":true
        }})
    );
}

/// 每个公开便捷方法都必须保留 RPC 方法名、参数和宿主返回值。
#[test]
fn host_api_wrappers_round_trip_through_real_stdio() {
    let mut host = Host::start();
    host.initialize();
    for (index, method) in [
        "ui.notify",
        "ui.confirm",
        "ui.prompt",
        "ui.pickFile",
        "settings.get",
        "state.get",
        "state.set",
        "secrets.get",
        "secrets.set",
        "secrets.delete",
        "clipboard.read",
        "clipboard.write",
        "shell.openExternal",
        "hosts.list",
        "terminal.paste",
        "events.subscribe",
    ]
    .into_iter()
    .enumerate()
    {
        let host_id = format!("h:api{index}");
        let plugin_id = format!("p:{}", index + 1);
        host.send(
            json!({"jsonrpc":"2.0","id":host_id,"method":"test.host-api","params":{
                "method":method,"payload":{"sample":index}
            }}),
        );
        assert_eq!(
            host.receive(),
            json!({"jsonrpc":"2.0","id":plugin_id,
            "method":method,"params":{"sample":index}})
        );
        host.send(json!({"jsonrpc":"2.0","id":plugin_id,"result":{"accepted":index}}));
        assert_eq!(
            host.receive(),
            json!({"jsonrpc":"2.0","id":host_id,
            "result":{"accepted":index}})
        );
    }
}

/// 取消信号应触发清理并阻止尚未执行的副作用，不能只丢弃回执。
#[test]
fn cancellation_signal_reaches_handler_and_stops_cooperative_work() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:work","method":"test.cancellable","params":{}}));
    host.send(json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:work"}}));
    assert_eq!(host.receive()["error"]["code"], -32002);
    host.send(json!({"jsonrpc":"2.0","id":"h:status","method":"test.cancel-status","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:status",
        "result":{"cancellableSideEffects":0,"observedAbort":true}})
    );
}

/// AbortController 在脚本中可独立使用，重复取消保持幂等。
#[test]
fn abort_controller_retains_reason_and_detaches_listener() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:abort","method":"test.abort","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:abort",
        "result":{"aborted":true,"reason":"reason","called":1,"thrown":"reason"}})
    );
}

/// 已发出的插件请求取消时需向宿主传播通知，迟到回执不能再次完成请求。
#[test]
fn outgoing_cancellation_notifies_host_and_ignores_late_response() {
    let mut host = Host::start();
    host.initialize();
    host.send(json!({"jsonrpc":"2.0","id":"h:out","method":"test.cancel-host-call","params":{}}));
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"p:1","method":"clipboard.read","params":{}})
    );
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"p:1"}})
    );
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:out","result":{"code":-32002}})
    );
    host.send(json!({"jsonrpc":"2.0","id":"p:1","result":{"late":true}}));
    host.send(json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}));
    assert_eq!(host.receive()["id"], "h:ping");
}

/// 预先取消时不能发出任何宿主请求，也不能消耗插件请求编号。
#[test]
fn pre_cancelled_outgoing_request_is_not_sent() {
    let mut host = Host::start();
    host.initialize();
    host.send(
        json!({"jsonrpc":"2.0","id":"h:pre","method":"test.cancel-before-host-call","params":{}}),
    );
    assert_eq!(
        host.receive(),
        json!({"jsonrpc":"2.0","id":"h:pre","result":{"code":-32002}})
    );
    host.send(json!({"jsonrpc":"2.0","id":"h:rpc","method":"test.rpc","params":{}}));
    assert_eq!(host.receive()["id"], "p:1");
}

/// Windows 接受的套接字可能继承非阻塞状态，读取前切回阻塞并设置截止时间。
fn read_http_request(stream: &mut TcpStream) -> String {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut total = None;
    loop {
        let count = stream.read(&mut buffer).expect("read HTTP request");
        assert!(count > 0, "request closed before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if total.is_none()
            && let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            total = Some(end + 4 + length);
        }
        if total.is_some_and(|length| bytes.len() >= length) {
            break;
        }
        assert!(bytes.len() < 1024 * 1024, "unexpected request size");
    }
    String::from_utf8(bytes).expect("UTF-8 HTTP test request")
}

/// 实际本地服务器按顺序返回预定响应，并对缺失请求设置截止时间。
fn http_fixture(responses: Vec<String>) -> (u16, Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("HTTP fixture accept failed: {error}"),
                }
            };
            sender.send(read_http_request(&mut stream)).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (port, receiver, worker)
}

/// 持久化跨进程保留，同时不同数据根、键与嵌套文件互不混淆。
#[test]
fn storage_persists_across_restart_and_isolates_data_roots() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    {
        let mut host = Host::start_with_data(first.path());
        host.initialize();
        assert_eq!(
            host.call("test.storage", json!({"op":"get","key":"Case"})),
            json!({"value":null})
        );
        assert_eq!(
            host.call(
                "test.storage",
                json!({"op":"set","key":"Case","value":{"x":1}})
            ),
            json!({})
        );
        assert_eq!(
            host.call("test.storage", json!({"op":"set","key":"case","value":[2]})),
            json!({})
        );
        assert_eq!(
            host.call(
                "test.storage",
                json!({"op":"set","key":"Case","value":{"x":3}})
            ),
            json!({})
        );
        assert_eq!(
            host.call(
                "test.storage",
                json!({"op":"writeFile","path":"nested/note.txt","text":"中文"})
            ),
            json!({})
        );
        assert_eq!(
            host.call("test.storage", json!({"op":"list","path":"nested"})),
            json!({"names":["note.txt"]})
        );
    }
    let mut host = Host::start_with_data(first.path());
    host.initialize();
    assert_eq!(
        host.call("test.storage", json!({"op":"get","key":"Case"})),
        json!({"value":{"x":3}})
    );
    assert_eq!(
        host.call("test.storage", json!({"op":"get","key":"case"})),
        json!({"value":[2]})
    );
    assert_eq!(
        host.call(
            "test.storage",
            json!({"op":"readFile","path":"nested/note.txt"})
        ),
        json!({"bytes":"中文".as_bytes()})
    );
    assert_eq!(
        host.call("test.storage", json!({"op":"delete","key":"Case"})),
        json!({"deleted":true})
    );
    assert_eq!(
        host.call("test.storage", json!({"op":"delete","key":"Case"})),
        json!({"deleted":false})
    );
    assert_eq!(
        host.call(
            "test.storage",
            json!({"op":"removeFile","path":"nested/note.txt"})
        ),
        json!({"deleted":true})
    );
    assert_eq!(
        host.call(
            "test.storage",
            json!({"op":"removeFile","path":"nested/note.txt"})
        ),
        json!({"deleted":false})
    );
    let mut isolated = Host::start_with_data(second.path());
    isolated.initialize();
    assert_eq!(
        isolated.call("test.storage", json!({"op":"get","key":"case"})),
        json!({"value":null})
    );
    assert_eq!(
        isolated.call("test.storage", json!({"op":"list"})),
        json!({"names":[]})
    );
}

/// 相对路径不能跨越数据根，超大已有文件也不能经读取绕过配额。
#[test]
fn storage_rejects_traversal_and_oversized_existing_files() {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start_with_data(root.path());
    host.initialize();
    for path in [
        "../escape",
        "nested/../../escape",
        "C:/outside",
        "nested\\outside",
        "/absolute",
        ".kerminal-tmp-x",
    ] {
        let result = host.call(
            "test.storage",
            json!({"op":"writeFile","path":path,"text":"bad"}),
        );
        assert!(
            result.get("error").is_some(),
            "accepted path {path}: {result}"
        );
    }
    std::fs::create_dir_all(root.path().join("files")).unwrap();
    std::fs::write(
        root.path().join("files/huge"),
        vec![0_u8; 4 * 1024 * 1024 + 1],
    )
    .unwrap();
    let result = host.call("test.storage", json!({"op":"readFile","path":"huge"}));
    assert!(result["error"].as_str().unwrap().contains("4 MiB"));
}

/// 能创建系统链接时验证目录句柄不会跟随越界目标；受系统权限限制则明确记录跳过。
#[test]
fn storage_rejects_symlink_escape_when_supported() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("files")).unwrap();
    std::fs::write(outside.path().join("secret"), b"outside").unwrap();
    #[cfg(windows)]
    let link = {
        let target = root.path().join("files").join("link");
        std::os::windows::fs::symlink_dir(outside.path(), &target).or_else(|_| {
            // 创建目录 junction 不需要符号链接特权，仍能覆盖 Windows 解析边界。
            let status = Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&target)
                .arg(outside.path())
                .output()?;
            if status.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "junction creation failed: {} {}",
                    String::from_utf8_lossy(&status.stdout),
                    String::from_utf8_lossy(&status.stderr)
                )))
            }
        })
    };
    #[cfg(unix)]
    let link = std::os::unix::fs::symlink(outside.path(), root.path().join("files").join("link"));
    if let Err(error) = link {
        eprintln!("symlink setup unavailable: {error}");
        return;
    }
    let mut host = Host::start_with_data(root.path());
    host.initialize();
    let result = host.call(
        "test.storage",
        json!({"op":"readFile","path":"link/secret"}),
    );
    assert!(
        result.get("error").is_some(),
        "escaped storage root: {result}"
    );
    let result = host.call(
        "test.storage",
        json!({"op":"writeFile","path":"link/new","text":"escape"}),
    );
    assert!(
        result.get("error").is_some(),
        "wrote outside storage root: {result}"
    );
    assert!(!outside.path().join("new").exists());
}

/// 本地服务确认同源跳转实际发送第二个请求，并验证 302 的方法转换。
#[test]
fn fetch_follows_same_origin_redirect_and_consumes_response_once() {
    let (port, requests, worker) = http_fixture(vec![
        "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".into(),
    ]);
    let mut host = Host::start();
    host.initialize();
    let result = host.call(
        "test.fetch",
        json!({"url":format!("http://localhost:{port}/start"),
        "options":{"method":"POST","headers":{"Authorization":"Bearer test"},"body":"hi"}}),
    );
    assert_eq!(result["status"], 200);
    assert_eq!(result["text"], "hello");
    assert_eq!(result["contentType"], "text/plain");
    assert_eq!(result["redirected"], true);
    assert_eq!(result["bodyUsed"], true);
    assert_eq!(result["repeated"], true);
    assert_eq!(result["responseType"], true);
    let first = requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .to_ascii_lowercase();
    let second = requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        first.starts_with("post /start ") && first.ends_with("hi"),
        "{first}"
    );
    assert!(
        second.starts_with("get /next ") && second.contains("authorization: bearer test"),
        "{second}"
    );
    worker.join().unwrap();
}

/// 白名单在跳转之前逐跳检查，禁止目标端口实际接到越权请求。
#[test]
fn fetch_rejects_disallowed_host_and_redirect_before_connection() {
    let forbidden = TcpListener::bind("127.0.0.1:0").unwrap();
    forbidden.set_nonblocking(true).unwrap();
    let target = forbidden.local_addr().unwrap().port();
    let (port, requests, worker) = http_fixture(vec![format!(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )]);
    let mut host = Host::start();
    host.initialize();
    for url in [
        format!("http://127.0.0.1:{target}/direct"),
        format!("http://localhost:{port}/redirect"),
    ] {
        let result = host.call("test.fetch", json!({"url":url}));
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("permission denied"),
            "{result}"
        );
    }
    assert!(
        requests
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .starts_with("GET /redirect ")
    );
    worker.join().unwrap();
    assert_eq!(
        forbidden.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

/// 不同来源即使都获准访问，也不能继承上一个来源的认证与 Cookie。
#[test]
fn fetch_strips_sensitive_headers_on_cross_origin_redirect() {
    let (target, target_requests, target_worker) = http_fixture(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
    ]);
    let (port, requests, worker) = http_fixture(vec![format!(
        "HTTP/1.1 302 Found\r\nLocation: http://localhost:{target}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )]);
    let mut host = Host::start();
    host.initialize();
    let result = host.call(
        "test.fetch",
        json!({"url":format!("http://localhost:{port}/start"),
        "options":{"headers":{"Authorization":"Bearer secret","Cookie":"session=secret"}}}),
    );
    assert_eq!(result["text"], "ok", "{result}");
    let first = requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .to_ascii_lowercase();
    let next = target_requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        first.contains("authorization: bearer secret") && first.contains("cookie: session=secret")
    );
    assert!(
        !next.contains("authorization:") && !next.contains("cookie:"),
        "{next}"
    );
    worker.join().unwrap();
    target_worker.join().unwrap();
}

/// 超限响应在读取实体之前拒绝，不能将 Content-Length 当作成功结果。
#[test]
fn fetch_rejects_oversized_response() {
    let (port, requests, worker) = http_fixture(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 4194305\r\nConnection: close\r\n\r\n".into(),
    ]);
    let mut host = Host::start();
    host.initialize();
    let result = host.call(
        "test.fetch",
        json!({"url":format!("http://localhost:{port}/huge")}),
    );
    assert!(
        result["error"].as_str().unwrap().contains("4 MiB"),
        "{result}"
    );
    requests.recv_timeout(Duration::from_secs(3)).unwrap();
    worker.join().unwrap();
}

/// 取消处理器后等待服务器观察到连接关闭，证明底层 HTTP 任务也停止。
#[test]
fn fetch_cancellation_closes_inflight_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_sender, accepted) = mpsc::channel();
    let (closed_sender, closed) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_http_request(&mut stream);
        accepted_sender.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut byte = [0_u8; 1];
        let stopped = matches!(stream.read(&mut byte), Ok(0));
        closed_sender.send(stopped).unwrap();
    });
    let mut host = Host::start();
    host.initialize();
    host.send(
        json!({"jsonrpc":"2.0","id":"h:slow","method":"test.fetch","params":{
        "url":format!("http://localhost:{port}/slow"),"cancellable":true}}),
    );
    accepted.recv_timeout(Duration::from_secs(5)).unwrap();
    host.send(json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:slow"}}));
    assert_eq!(host.receive()["error"]["code"], -32002);
    assert!(
        closed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "server did not observe socket close"
    );
    worker.join().unwrap();
}

/// 官方命名空间只由组合根显式传入的参数启用，不能从清单自己推导。
#[test]
fn official_manifest_requires_explicit_trusted_flag() {
    let directory = tempfile::tempdir().unwrap();
    let mut manifest: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/plugins/scripts/p0/kerminal-plugin.json"
    ))
    .unwrap();
    manifest["id"] = json!("kerminal.notes");
    manifest["publisher"] = json!("kerminal");
    std::fs::write(
        directory.path().join("kerminal-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.path().join("main.js"), "").unwrap();
    let executable = env!("CARGO_BIN_EXE_kerminal-plugin-host");
    let ordinary = Command::new(executable)
        .arg(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!ordinary.status.success());
    let official = Command::new(executable)
        .arg("--official")
        .arg(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        official.status.success(),
        "{}",
        String::from_utf8_lossy(&official.stderr)
    );
}

/// URL、严格解码与不同摘要均用确定结果验证，随机值验证格式和独立性。
#[test]
fn web_primitives_match_known_vectors() {
    let mut host = Host::start();
    host.initialize();
    let result = host.call("test.web", json!({}));
    assert_eq!(
        result["addresses"],
        json!({"href":"https://example.com/a?x=1&x=2&q=%E4%B8%AD+%E6%96%87#part",
        "values":["1","2"],"same":true,"origin":"https://example.com","ipv6":"[::1]:8080","question":"%3Fa=1"})
    );
    assert_eq!(
        result["hashes"]["SHA-256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        result["hashes"]["SHA-384"],
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    );
    assert_eq!(
        result["hashes"]["SHA-512"],
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(result["fatal"], true);
    assert_eq!(result["replacement"], "�");
    let uuid = result["uuid"].as_str().unwrap();
    let parsed = uuid::Uuid::parse_str(uuid).unwrap();
    assert_eq!(parsed.get_version_num(), 4);
    assert_ne!(result["uuid"], result["uuid2"]);
}

/// 允许的最大文件需要真正经过 JS 编解码和磁盘往返，不能只验证拒绝超限。
#[test]
fn storage_accepts_four_mib_file() {
    let mut host = Host::start();
    host.initialize();
    let result = host.call("test.large-storage", json!({"size":4 * 1024 * 1024}));
    assert_eq!(
        result,
        json!({"length":4 * 1024 * 1024,"first":97,"last":97})
    );
}

/// 已取消请求和不支持的选项不得建立连接或静默降级。
#[test]
fn fetch_preaborted_and_unsupported_options_do_not_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://localhost:{}/never",
        listener.local_addr().unwrap().port()
    );
    let mut host = Host::start();
    host.initialize();
    let aborted = host.call("test.fetch", json!({"url":url,"preAborted":true}));
    assert_eq!(aborted["name"], "AbortError", "{aborted}");
    let unsupported = host.call(
        "test.fetch",
        json!({"url":url,"options":{"credentials":"include"}}),
    );
    assert!(
        unsupported["error"]
            .as_str()
            .unwrap()
            .contains("unsupported fetch option")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

/// 官方示例实际入口经过受信标志握手并完成无网络的视图生命周期。
#[test]
fn official_http_example_loads_and_closes_a_view() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/http-check");
    let data = tempfile::tempdir().unwrap();
    let mut host = Host::start_fixture(&fixture, data.path(), true);
    host.initialize_identity("kerminal.http-check", "1.0.0");
    let opened = host.call("view.open", json!({"instance":"sample", "state":null}));
    assert_eq!(opened["data"]["url"], "https://example.com/");
    let closed = host.call("view.close", json!({"instance":"sample"}));
    assert_eq!(closed["state"]["url"], "https://example.com/");
}

/// 目标不可替换时保留已有目录并清除本次创建的临时文件。
#[test]
fn storage_failed_replacement_cleans_its_temporary_file() {
    let root = tempfile::tempdir().unwrap();
    let files = root.path().join("files");
    std::fs::create_dir_all(files.join("occupied")).unwrap();
    let mut host = Host::start_with_data(root.path());
    host.initialize();
    let result = host.call(
        "test.storage",
        json!({"op":"writeFile","path":"occupied","text":"value"}),
    );
    assert!(result.get("error").is_some(), "{result}");
    assert!(files.join("occupied").is_dir());
    let names: Vec<_> = std::fs::read_dir(files)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("occupied")]);
}
