//! 一致性检查使用隔离进程与有界帧；专用压力探针必须显式启用。
//! @author kongweiguang

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::Stdio,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};

use kerminal_plugin_protocol::{
    MAX_PENDING_REQUESTS, MAX_RPC_BYTES, PROTOCOL_VERSION, RpcMessage, RpcNotification, RpcPeer,
    RpcRequest, error_code,
};
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde_json::{Value, json};

use crate::Result;

/// 常规模式只验证所有插件都应支持的握手、帧、心跳和关闭。
pub fn run(executable: &Path, id: &str, version: &str, fixture: bool) -> Result<()> {
    let data = tempfile::tempdir()?;
    let mut command = CommandWrap::with_new(executable, |command| {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        command.env_clear();
        for name in ["PATH", "HOME", "USERPROFILE", "LANG", "SYSTEMROOT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("KERMINAL_PLUGIN_ID", id)
            .env("KERMINAL_PLUGIN_DATA", data.path())
            .current_dir(data.path());
    });
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    let child = command.spawn()?;
    let mut process = Process(child);
    let input = process.0.stdin().take().ok_or("Sidecar stdin 不可用")?;
    let mut input = FrameWriter::new(input);
    let output = process.0.stdout().take().ok_or("Sidecar stdout 不可用")?;
    let receiver = read_frames(output);
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":"h:initialize","method":"initialize","params":{
            "protocol":PROTOCOL_VERSION,"host_api":"1.0","plugin":{"id":id,"version":version},
            "locale":"en","theme":"dark","data_dir":data.path(),"generation":1
        }}),
    )?;
    let response = receive(
        &receiver,
        &mut input,
        Instant::now() + Duration::from_secs(10),
    )?;
    if response.get("id") != Some(&json!("h:initialize"))
        || response.get("result")
            != Some(&json!({"protocol":PROTOCOL_VERSION,"plugin":{"id":id,"version":version}}))
    {
        return Err("握手协议或插件身份不匹配".into());
    }
    send(
        &mut input,
        json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )?;
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":"h:ping","method":"$/ping","params":{}}),
    )?;
    expect_empty_result(&receiver, &mut input, "h:ping", Duration::from_secs(5))?;
    if fixture {
        probe_cancellation_and_backpressure(&receiver, &mut input)?;
    }
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":"h:shutdown","method":"shutdown","params":{}}),
    )?;
    expect_empty_result(&receiver, &mut input, "h:shutdown", Duration::from_secs(5))?;
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = process.0.try_wait()? {
            if !status.success() {
                return Err(format!("Sidecar 关闭后退出异常: {status}").into());
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err("Sidecar 关闭后未按时退出".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    if fixture {
        println!("通过：握手、身份、帧、心跳、取消、背压、关闭");
    } else {
        println!("通过：握手、身份、帧、心跳、关闭；取消和背压未测试（需 --fixture-mode）");
    }
    Ok(())
}

/// 只为测试插件启用阻塞方法；普通插件不因缺少测试方法被误判。
fn probe_cancellation_and_backpressure(
    receiver: &Receiver<Result<Value>>,
    input: &mut FrameWriter,
) -> Result<()> {
    for index in 0..=MAX_PENDING_REQUESTS {
        send(
            input,
            json!({"jsonrpc":"2.0","id":format!("h:probe{index}"),"method":"conformance.block","params":{}}),
        )?;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let overloaded = receive(receiver, input, deadline)?;
    if overloaded.get("id") != Some(&json!(format!("h:probe{MAX_PENDING_REQUESTS}")))
        || overloaded.pointer("/error/code").and_then(Value::as_i64)
            != Some(i64::from(error_code::OVERLOADED))
    {
        return Err("第 65 个挂起请求没有返回 overloaded".into());
    }
    for index in 0..MAX_PENDING_REQUESTS {
        let id = format!("h:probe{index}");
        send(
            input,
            json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":id}}),
        )?;
        let response = receive(receiver, input, deadline)?;
        if response.get("id").and_then(Value::as_str) != Some(id.as_str())
            || response.pointer("/error/code").and_then(Value::as_i64)
                != Some(i64::from(error_code::CANCELLED))
        {
            return Err("取消回执身份或错误码不符".into());
        }
    }
    Ok(())
}

/// 帧读线程限制单行大小；主线程的绝对截止时间不被无响应进程阻塞。
fn read_frames(output: impl Read + Send + 'static) -> Receiver<Result<Value>> {
    let (sender, receiver) = mpsc::sync_channel(8);
    thread::spawn(move || {
        let mut output = BufReader::new(output);
        loop {
            let mut bytes = Vec::new();
            let result = output
                .by_ref()
                .take((MAX_RPC_BYTES + 1) as u64)
                .read_until(b'\n', &mut bytes);
            let frame: Result<Value> = match result {
                Err(error) => Err(error.into()),
                Ok(0) => break,
                Ok(_) if bytes.len() > MAX_RPC_BYTES || !bytes.ends_with(b"\n") => {
                    Err("Sidecar 输出了超限或未完成的帧".into())
                }
                Ok(_) => serde_json::from_slice::<RpcMessage>(&bytes)
                    .and_then(serde_json::to_value)
                    .map_err(Into::into),
            };
            let failed = frame.is_err();
            if sender.send(frame).is_err() || failed {
                break;
            }
        }
    });
    receiver
}

/// 插件主动请求必须得到明确错误，避免测试宿主静默阻塞双向 RPC。
fn receive(
    receiver: &Receiver<Result<Value>>,
    input: &mut FrameWriter,
    deadline: Instant,
) -> Result<Value> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Sidecar 响应超时")?;
        let message = receiver.recv_timeout(remaining)??;
        if message.get("jsonrpc") != Some(&json!("2.0")) || !message.is_object() {
            return Err("Sidecar 输出无效 JSON-RPC 帧".into());
        }
        if message.get("method").is_some() {
            if message.get("id").is_some() {
                let request: RpcRequest = serde_json::from_value(message)?;
                request.validate(RpcPeer::Plugin)?;
                send(
                    input,
                    json!({"jsonrpc":"2.0","id":request.id,"error":{"code":error_code::METHOD_NOT_FOUND,"message":"not available in conformance"}}),
                )?;
            } else {
                let notification: RpcNotification = serde_json::from_value(message)?;
                notification.validate()?;
            }
            continue;
        }
        let response: kerminal_plugin_protocol::RpcResponse =
            serde_json::from_value(message.clone())?;
        response.validate()?;
        return Ok(message);
    }
}

/// 成功回复必须对应原请求且结果为空对象，不能把错误或其他回执当成成功。
fn expect_empty_result(
    receiver: &Receiver<Result<Value>>,
    input: &mut FrameWriter,
    id: &str,
    timeout: Duration,
) -> Result<()> {
    let response = receive(receiver, input, Instant::now() + timeout)?;
    if response.get("id") != Some(&json!(id)) || response.get("result") != Some(&json!({})) {
        return Err(format!("{id} 回复不符合契约").into());
    }
    Ok(())
}

/// 每次写入完整 NDJSON 帧并立即刷新，确保取消和关闭不受缓冲影响。
fn send(output: &mut FrameWriter, message: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&message)?;
    if bytes.len() + 1 > MAX_RPC_BYTES {
        return Err("请求帧超限".into());
    }
    bytes.push(b'\n');
    let (sender, receiver) = mpsc::channel();
    output.sender.send((bytes, sender))?;
    receiver.recv_timeout(Duration::from_secs(5))??;
    Ok(())
}

type WriteRequest = (Vec<u8>, mpsc::Sender<io::Result<()>>);

struct FrameWriter {
    sender: SyncSender<WriteRequest>,
}

impl FrameWriter {
    /// 单独的写线程防止拒读 stdin 的子进程永久阻塞一致性检查；超时后由进程守卫回收。
    fn new(mut output: impl Write + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<WriteRequest>(1);
        thread::spawn(move || {
            for (frame, completed) in receiver {
                let result = output.write_all(&frame).and_then(|_| output.flush());
                let failed = result.is_err();
                let _ = completed.send(result);
                if failed {
                    break;
                }
            }
        });
        Self { sender }
    }
}

struct Process(Box<dyn ChildWrapper>);

impl Drop for Process {
    /// 测试失败或超时时终止进程组/Job，避免留下 Sidecar 子进程。
    fn drop(&mut self) {
        let _ = self.0.start_kill();
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.wait();
        }
    }
}
