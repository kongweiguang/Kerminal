//! 验证本机结构化命令在大输入和取消时能同时 drain 输出并回收子进程。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions, TryLockError},
    future::pending,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, CommandInputHandle, CommandStreamHandle, ConnectionIntent, ExecRequest,
        HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest, OpenSftpRequest,
        RemoteCommandInput, RemoteCommandStream, RemoteConnectionHandle, RemoteConnectionLease,
        RemoteConnectionService, RemoteForwardHandle, RemoteForwardRequest, RemoteShellDialect,
        SftpChannelHandle, ShellScriptRequest, TerminalOpenRequest, TunnelChannelHandle,
        TunnelRequest,
    },
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, CommandStreamEvent,
        ContainerStageCleanupRequest, OperationsCommandExecutor,
    },
    sessions::SessionTransport,
};
use kerminal_domain::{
    connections::{ConnectionCapabilities, RemoteProtocol},
    identity::{HostId, TransferId},
};
use kerminal_infrastructure::operations::StructuredOperationsCommandExecutor;
use tokio_util::sync::CancellationToken;

const ECHO_CHILD_ENV: &str = "KERMINAL_TEST_COMMAND_EXECUTOR_ECHO_CHILD";
const HOLD_CHILD_ENV: &str = "KERMINAL_TEST_COMMAND_EXECUTOR_HOLD_CHILD";
const EMIT_CHILD_ENV: &str = "KERMINAL_TEST_COMMAND_EXECUTOR_EMIT_CHILD";
const EMIT_READY_ENV: &str = "KERMINAL_TEST_COMMAND_EXECUTOR_EMIT_READY";
const EMIT_LOCK_ENV: &str = "KERMINAL_TEST_COMMAND_EXECUTOR_EMIT_LOCK";

struct UnusedRemoteConnections;

/// 预留 capability 的创建必须越过同步状态锁并到达命令连接边界，旧缺陷的卡线程不能挂住整个测试进程。
///
/// @author kongweiguang
#[test]
fn stage_creation_releases_phase_lock_before_connecting() {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create isolated runtime");
        let code = runtime.block_on(async {
            let executor =
                StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
            let capability = executor
                .reserve_container_stage_cleanup(ContainerStageCleanupRequest {
                    transfer_id: TransferId::new(),
                    staging_endpoint: kerminal_domain::files::FileEndpoint::Container {
                        host_id: Some(HostId::new()),
                        runtime: kerminal_domain::files::ContainerRuntime::Docker,
                        container_id: "isolated-fixture".to_owned(),
                        path: kerminal_domain::files::RemotePath::parse_container(
                            "/tmp/.kerminal-stage",
                        )
                        .expect("valid stage path"),
                    },
                })
                .expect("reserve isolated staging capability");
            capability
                .create_stage_until(tokio::time::Instant::now() + Duration::from_secs(3))
                .await
                .expect_err("the unused connector must reject after the phase transition")
                .public_code()
        });
        sender.send(code).expect("deliver executor result");
    });
    let code = receiver
        .recv_timeout(Duration::from_secs(8))
        .expect("stage creation must not block on its own phase lock");
    thread.join().expect("isolated runtime exits");
    assert_eq!(code, kerminal_application::PublicErrorCode::Unsupported);
}

#[async_trait]
impl RemoteConnectionService for UnusedRemoteConnections {
    /// 远端测试请求到达连接边界时用固定 Unsupported 哨兵终止，以验证前置 argv 校验行为。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("测试未配置远端连接"))
    }

    /// 非本组测试关注的指纹查询保持明确不支持，避免伪造安全回执。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("测试未配置主机指纹查询"))
    }

    /// 非本组测试不持久化信任状态，调用表示测试越界。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("测试未配置主机信任"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

/// 子进程测试入口通过环境变量区分父测试和被启动的回显进程。
#[test]
fn helper_echo_child() {
    if std::env::var_os(ECHO_CHILD_ENV).is_none() {
        return;
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    io::copy(&mut input, &mut output).expect("回显测试输入失败");
    output.flush().expect("刷新回显测试输出失败");
}

/// 以与 SSH acceptance 相同的 argv 和读取预算验证请求能通过入口校验并到达连接边界。
///
/// @author kongweiguang
#[tokio::test]
async fn python_acceptance_probe_passes_invocation_validation() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let error = executor
        .execute(
            CommandExecutionTarget::Host(HostId::new()),
            CommandInvocation::new(
                "python3",
                vec![
                    "-c".to_owned(),
                    "import sys; print(sys.version_info[0])".to_owned(),
                ],
            ),
            CommandExecutionLimits {
                timeout: Duration::from_secs(12),
                max_output_bytes: 16 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("fake connector must reject only after invocation validation");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Unsupported,
        "request should reach the fake SSH connection boundary: {error:?}"
    );
}

/// 多行 Python 源码作为单个 argv 传递时应通过校验，并到达明确拒绝测试连接的 fake 端口。
///
/// @author kongweiguang
#[tokio::test]
async fn structured_argv_multiline_argument_reaches_remote_connection() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let error = executor
        .execute(
            CommandExecutionTarget::Host(HostId::new()),
            CommandInvocation::new(
                "python3",
                vec![
                    "-c".to_owned(),
                    "import sys\nprint(sys.version_info[0])".to_owned(),
                ],
            ),
            CommandExecutionLimits {
                timeout: Duration::from_secs(12),
                max_output_bytes: 16 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("fake connector must reject after validating the multiline argv");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Unsupported,
        "multiline argv should reach the fake SSH connection boundary"
    );
}

/// NUL 不能表示为操作系统 argv，也不能编码进 SSH exec，须在触达连接端口前拒绝。
///
/// @author kongweiguang
#[tokio::test]
async fn structured_argv_rejects_nul_before_remote_connection() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let error = executor
        .execute(
            CommandExecutionTarget::Host(HostId::new()),
            CommandInvocation::new("python3", vec!["-c".to_owned(), "print('a\0b')".to_owned()]),
            CommandExecutionLimits {
                timeout: Duration::from_secs(12),
                max_output_bytes: 16 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("NUL must be rejected before the fake connector can accept the request");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        error.public_message(),
        "运维命令参数或读取预算无效 [argument_nul_byte]"
    );
}

/// 子进程持有独占文件锁并写出就绪标记，再向 stdout 写入超过 pipe 容量的内容供真实流式测试使用。
///
/// @author kongweiguang
#[test]
fn helper_emit_child() {
    if std::env::var_os(EMIT_CHILD_ENV).is_none() {
        return;
    }
    let child_lock = std::env::var_os(EMIT_LOCK_ENV).map(|path| {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("打开流式子进程锁文件失败");
        file.lock().expect("锁定流式子进程存活标记失败");
        file
    });
    if let Some(marker) = std::env::var_os(EMIT_READY_ENV) {
        fs::write(marker, b"ready").expect("写入流式子进程就绪标记失败");
    }
    let mut output = io::stdout().lock();
    let block = vec![b'Z'; 256 * 1024];
    for _ in 0..32 {
        output.write_all(&block).expect("写出流式测试数据失败");
    }
    output.flush().expect("刷新流式测试输出失败");
    drop(child_lock);
}

/// 子进程写入启动标记后持续运行，供父测试验证取消会等待它退出。
#[test]
fn helper_hold_child() {
    let Some(marker) = std::env::var_os(HOLD_CHILD_ENV) else {
        return;
    };
    fs::write(marker, std::process::id().to_string()).expect("写入测试进程标记失败");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// 大于操作系统管道容量的 stdin 和 stdout 同时推进，避免读写互等造成死锁。
#[tokio::test]
async fn local_input_and_output_make_progress_together() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let input = large_binary_payload(8 * 1024 * 1024);
    let mut invocation = helper_invocation("helper_echo_child");
    invocation
        .environment
        .insert(ECHO_CHILD_ENV.to_owned(), "1".to_owned());

    let result = executor
        .execute_with_input(
            CommandExecutionTarget::Local,
            invocation,
            input.clone(),
            limits(input.len() + 128 * 1024),
            CancellationToken::new(),
        )
        .await
        .expect("本机回显命令应完成");

    assert_eq!(result.exit_code, Some(0));
    assert!(!result.truncated);
    assert!(result.input_complete);
    let start = result
        .stdout
        .windows(b"KERM-INPUT-BEGIN".len())
        .position(|window| window == b"KERM-INPUT-BEGIN")
        .expect("输出缺少输入起始标记");
    let end = result
        .stdout
        .windows(b"KERM-INPUT-END".len())
        .position(|window| window == b"KERM-INPUT-END")
        .expect("输出缺少输入结束标记");
    assert_payload_matches(&result.stdout[start..end + b"KERM-INPUT-END".len()], &input);
}

/// 子进程复用 Rust test harness，因此按 payload 标记提取回显，避免把 harness 文本当成命令输出。
#[tokio::test]
async fn local_stream_round_trips_large_binary_payload() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let input = large_binary_payload(2 * 1024 * 1024);
    let mut invocation = helper_invocation("helper_echo_child");
    invocation
        .environment
        .insert(ECHO_CHILD_ENV.to_owned(), "1".to_owned());
    let mut stream = executor
        .execute_stream(
            CommandExecutionTarget::Local,
            invocation,
            limits(64 * 1024),
            CancellationToken::new(),
        )
        .await
        .expect("应启动本机双向命令流");
    let writer = stream.input();
    let payload = input.clone();
    let producer = tokio::spawn(async move {
        for chunk in payload.chunks(64 * 1024) {
            writer
                .write(chunk.to_vec())
                .await
                .expect("写入流式测试块失败");
        }
        writer.close().await.expect("关闭流式 stdin 失败");
    });
    let mut output = Vec::new();
    while let Some(event) = stream.next_event().await.expect("读取流式输出失败") {
        if let CommandStreamEvent::Stdout(bytes) = event {
            output.extend_from_slice(&bytes);
        }
    }
    producer.await.expect("输入生产任务 panic");
    let exit = stream.finish().await.expect("流式命令应正常完成");

    assert_eq!(exit.exit_code, Some(0));
    let start = output
        .windows(b"KERM-INPUT-BEGIN".len())
        .position(|window| window == b"KERM-INPUT-BEGIN")
        .expect("输出缺少输入起始标记");
    let end = output
        .windows(b"KERM-INPUT-END".len())
        .position(|window| window == b"KERM-INPUT-END")
        .expect("输出缺少输入结束标记");
    assert_payload_matches(&output[start..end + b"KERM-INPUT-END".len()], &input);
}

/// finish 在未消费事件时仍并发 drain pipe，确保大 stdout 不会卡住 child wait。
#[tokio::test]
async fn stream_finish_drains_unread_output() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let mut invocation = helper_invocation("helper_emit_child");
    invocation
        .environment
        .insert(EMIT_CHILD_ENV.to_owned(), "1".to_owned());
    let mut stream = executor
        .execute_stream(
            CommandExecutionTarget::Local,
            invocation,
            limits(64 * 1024),
            CancellationToken::new(),
        )
        .await
        .expect("应启动大输出测试命令");
    let exit = tokio::time::timeout(Duration::from_secs(5), stream.finish())
        .await
        .expect("finish 未 drain 输出并卡住")
        .expect("大输出命令应正常完成");

    assert_eq!(exit.exit_code, Some(0));
    assert!(exit.input_complete);
}

/// 验证未消费的有界输出队列不能阻止真实本机命令按 deadline 终止并回收。
///
/// @author kongweiguang
#[tokio::test]
async fn stream_output_backpressure_obeys_command_deadline() {
    let executor = StructuredOperationsCommandExecutor::new(Arc::new(UnusedRemoteConnections));
    let marker_suffix = HostId::new();
    let marker = std::env::temp_dir().join(format!(
        "kerminal-gpui-command-backpressure-{}.marker",
        marker_suffix
    ));
    let lock_path = std::env::temp_dir().join(format!(
        "kerminal-gpui-command-backpressure-{}.lock",
        marker_suffix
    ));
    let _ = fs::remove_file(&marker);
    let _ = fs::remove_file(&lock_path);
    drop(
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("创建流式子进程锁文件失败"),
    );
    let mut invocation = helper_invocation("helper_emit_child");
    invocation
        .environment
        .insert(EMIT_CHILD_ENV.to_owned(), "1".to_owned());
    invocation.environment.insert(
        EMIT_READY_ENV.to_owned(),
        marker.to_string_lossy().into_owned(),
    );
    invocation.environment.insert(
        EMIT_LOCK_ENV.to_owned(),
        lock_path.to_string_lossy().into_owned(),
    );
    let mut stream = executor
        .execute_stream(
            CommandExecutionTarget::Local,
            invocation,
            CommandExecutionLimits {
                timeout: Duration::from_secs(2),
                max_output_bytes: 64 * 1024,
            },
            CancellationToken::new(),
        )
        .await
        .expect("应启动本机背压测试命令");

    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if started.is_err() {
        let _ = stream
            .close_until(tokio::time::Instant::now() + Duration::from_secs(3))
            .await;
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&lock_path);
        panic!("背压测试子进程未能启动");
    }

    let lock_probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("打开流式子进程存活锁失败");
    // 就绪标记之后 child 会连续写 8 MiB；输出队列仅能缓存少量 chunk，故此处不读取。
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let lock_released = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match lock_probe.try_lock() {
                Ok(()) => {
                    lock_probe.unlock().expect("释放已确认的子进程退出锁失败");
                    return;
                }
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(TryLockError::Error(_)) => panic!("查询流式子进程存活锁失败"),
            }
        }
    })
    .await;
    if lock_released.is_err() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let _ = stream.close_until(deadline).await;
        let _ = executor.shutdown_until(deadline).await;
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&lock_path);
        panic!("receiver 仍未读取时子进程未按 command deadline 退出");
    }
    // 文件锁释放证明命令子进程已退出；此断言发生在 finish 排空事件之前。
    let finish_result = tokio::time::timeout(Duration::from_secs(8), stream.finish()).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let cleanup_result = stream.close_until(deadline).await;
    let shutdown_result = executor.shutdown_until(deadline).await;
    let _ = fs::remove_file(&marker);
    let _ = fs::remove_file(&lock_path);
    let timeout_error = match finish_result {
        Ok(Err(error)) => error,
        Ok(Ok(_)) => panic!("命令 deadline 已过，不能把背压中的命令报为成功"),
        Err(_) => panic!("队列背压下命令 deadline 未及时终止 worker"),
    };
    assert_eq!(
        timeout_error.public_code(),
        kerminal_application::PublicErrorCode::Timeout
    );
    cleanup_result.expect("超时命令的进程树与 writer 应可在有界 deadline 内回收");
    shutdown_result.expect("回收后执行器应无遗留资源");
}

/// 流句柄关闭后 worker 必须 kill 并 wait 子进程，且 shutdown tracker 要观察到登记归零。
#[tokio::test]
async fn stream_close_waits_for_child_reaping() {
    let executor = Arc::new(StructuredOperationsCommandExecutor::new(Arc::new(
        UnusedRemoteConnections,
    )));
    let marker = std::env::temp_dir().join(format!(
        "kerminal-gpui-command-stream-child-{}.marker",
        std::process::id()
    ));
    let _ = fs::remove_file(&marker);
    let mut invocation = helper_invocation("helper_hold_child");
    invocation.environment.insert(
        HOLD_CHILD_ENV.to_owned(),
        marker.to_string_lossy().into_owned(),
    );
    let mut stream = executor
        .execute_stream(
            CommandExecutionTarget::Local,
            invocation,
            limits(16 * 1024),
            CancellationToken::new(),
        )
        .await
        .expect("应启动保持运行的本机命令");
    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if started.is_err() {
        let _ = stream
            .close_until(tokio::time::Instant::now() + Duration::from_secs(3))
            .await;
        let _ = fs::remove_file(&marker);
        panic!("流式测试子进程未能启动");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    stream
        .close_until(deadline)
        .await
        .expect("close_until 应等待子进程回收");
    executor
        .shutdown_until(deadline)
        .await
        .expect("worker 回收后 shutdown 应立即完成");
    let _ = fs::remove_file(&marker);
}

/// 主动取消后本机命令必须先终止并 wait 子进程，再把取消结果交回调用方。
#[tokio::test]
async fn local_cancellation_waits_for_child_reaping() {
    let executor = Arc::new(StructuredOperationsCommandExecutor::new(Arc::new(
        UnusedRemoteConnections,
    )));
    let marker = std::env::temp_dir().join(format!(
        "kerminal-gpui-command-child-{}.marker",
        std::process::id()
    ));
    let _ = fs::remove_file(&marker);
    let mut invocation = helper_invocation("helper_hold_child");
    invocation.environment.insert(
        HOLD_CHILD_ENV.to_owned(),
        marker.to_string_lossy().into_owned(),
    );
    let cancel = CancellationToken::new();
    let operation = {
        let executor = executor.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            executor
                .execute(
                    CommandExecutionTarget::Local,
                    invocation,
                    limits(16 * 1024),
                    cancel,
                )
                .await
        })
    };

    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if started.is_err() {
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), operation).await;
        let _ = fs::remove_file(&marker);
        panic!("测试子进程未能启动");
    }

    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(3), operation)
        .await
        .expect("取消后命令任务未及时返回")
        .expect("命令任务 panic");
    let _ = fs::remove_file(&marker);
    assert_eq!(
        result.expect_err("取消命令不能伪报成功").public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
}

/// 验证统一退出会拒绝新命令、取消在途子进程并等执行器登记归零。
#[tokio::test]
async fn shutdown_cancels_in_flight_command_and_stops_intake() {
    let executor = Arc::new(StructuredOperationsCommandExecutor::new(Arc::new(
        UnusedRemoteConnections,
    )));
    let marker = std::env::temp_dir().join(format!(
        "kerminal-gpui-command-shutdown-{}.marker",
        std::process::id()
    ));
    let _ = fs::remove_file(&marker);
    let mut invocation = helper_invocation("helper_hold_child");
    invocation.environment.insert(
        HOLD_CHILD_ENV.to_owned(),
        marker.to_string_lossy().into_owned(),
    );
    let command = {
        let executor = executor.clone();
        tokio::spawn(async move {
            executor
                .execute(
                    CommandExecutionTarget::Local,
                    invocation,
                    limits(16 * 1024),
                    CancellationToken::new(),
                )
                .await
        })
    };
    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if started.is_err() {
        let _ = tokio::time::timeout(Duration::from_secs(3), command).await;
        let _ = fs::remove_file(&marker);
        panic!("退出测试子进程未能启动");
    }

    executor
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .expect("退出应等待本机子进程完成回收");
    let result = tokio::time::timeout(Duration::from_secs(1), command)
        .await
        .expect("执行任务应在 shutdown 返回前完成")
        .expect("命令任务 panic");
    let _ = fs::remove_file(&marker);
    assert_eq!(
        result.expect_err("退出取消不能伪报命令成功").public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );

    let rejected = executor
        .execute(
            CommandExecutionTarget::Local,
            helper_invocation("helper_echo_child"),
            limits(1024),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        rejected
            .expect_err("shutdown 后不能接收新命令")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
}

/// 假远端先等完整 stdin EOF 再发送退出和尾部字节，模拟命令确实消费了请求输入的连接顺序。
#[tokio::test]
async fn remote_exit_status_is_drained_before_eof() {
    let input_bytes = large_binary_payload(2 * 1024 * 1024);
    let input_state = Arc::new(GatedInput::new());
    let scripted = ScriptedCommandStream::new(
        vec![
            Some(CommandEvent::Exited {
                code: Some(7),
                signal: None,
            }),
            Some(CommandEvent::Stdout(b"stdout-tail".to_vec())),
            Some(CommandEvent::Stderr(b"stderr-tail".to_vec())),
            None,
        ],
        input_state.clone(),
    )
    .gate_events_until_input_complete(input_bytes.len());
    let stream = CommandStreamHandle::new(Box::new(scripted));
    let host_id = HostId::new();
    let handle = Arc::new(FakeRemoteHandle::new(host_id.clone(), stream));
    let connections = Arc::new(FakeRemoteConnections { handle });
    let executor = StructuredOperationsCommandExecutor::new(connections);

    let result = executor
        .execute_with_input(
            CommandExecutionTarget::Host(host_id),
            CommandInvocation::new("cat", Vec::new()),
            input_bytes.clone(),
            limits(1024),
            CancellationToken::new(),
        )
        .await
        .expect("远端命令应消费退出状态后的尾部数据");

    assert_eq!(result.exit_code, Some(7));
    assert_eq!(result.stdout, b"stdout-tail");
    assert_eq!(result.stderr, b"stderr-tail");
    assert!(result.input_complete);
    assert_eq!(input_state.bytes(), input_bytes);
    assert!(input_state.is_closed());
}

/// 远端提前退出时保留退出与尾部输出，并将未 ACK 的输入标成未知而非用户取消。
///
/// @author kongweiguang
#[tokio::test]
async fn remote_early_exit_preserves_output_and_marks_input_unconfirmed() {
    let input_state = Arc::new(EarlyExitInput::new());
    let scripted = EarlyExitCommandStream::new(
        input_state.clone(),
        vec![
            CommandEvent::Exited {
                code: Some(23),
                signal: None,
            },
            CommandEvent::Stdout(b"stdout-tail".to_vec()),
            CommandEvent::Stderr(b"stderr-tail".to_vec()),
        ],
    );
    let stream = CommandStreamHandle::new(Box::new(scripted));
    let host_id = HostId::new();
    let handle = Arc::new(FakeRemoteHandle::new(host_id.clone(), stream));
    let executor =
        StructuredOperationsCommandExecutor::new(Arc::new(FakeRemoteConnections { handle }));
    let cancellation = CancellationToken::new();

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        executor.execute_with_input(
            CommandExecutionTarget::Host(host_id),
            CommandInvocation::new("consume-then-exit", Vec::new()),
            large_binary_payload(64 * 1024),
            limits(1024),
            cancellation.clone(),
        ),
    )
    .await
    .expect("远端提前退出后应有界完成输出排空")
    .expect("内部停止 stdin writer 不应伪装成用户取消");

    assert_eq!(
        input_state.attempted_bytes.load(Ordering::Acquire),
        32 * 1024
    );
    assert_eq!(input_state.accepted_bytes.load(Ordering::Acquire), 0);
    assert_eq!(result.exit_code, Some(23));
    assert_eq!(result.stdout, b"stdout-tail");
    assert_eq!(result.stderr, b"stderr-tail");
    assert!(!result.input_complete);
    assert!(!cancellation.is_cancelled());
}

/// 命令运行超过普通清理预算后再取消时，首次重试仍须获得新鲜而非过期的绝对 deadline。
#[tokio::test]
async fn long_running_remote_command_gets_fresh_cleanup_deadline() {
    let input_state = Arc::new(GatedInput::new());
    let close_attempts = Arc::new(AtomicUsize::new(0));
    let retry_deadline_was_future = Arc::new(AtomicBool::new(false));
    let stream = CommandStreamHandle::new(Box::new(PendingCleanupCommandStream {
        input: input_state,
        close_attempts: close_attempts.clone(),
        retry_deadline_was_future: retry_deadline_was_future.clone(),
    }));
    let host_id = HostId::new();
    let handle = Arc::new(FakeRemoteHandle::new(host_id.clone(), stream));
    let executor = Arc::new(StructuredOperationsCommandExecutor::new(Arc::new(
        FakeRemoteConnections { handle },
    )));
    let cancellation = CancellationToken::new();
    let command_executor = executor.clone();
    let command_cancel = cancellation.clone();
    let command = tokio::spawn(async move {
        command_executor
            .execute(
                CommandExecutionTarget::Host(host_id),
                CommandInvocation::new("wait-for-cancel", Vec::new()),
                limits(1024),
                command_cancel,
            )
            .await
    });

    // 执行时间跨过普通命令的五秒回收预算，复现“启动时计算 deadline”的过期问题。
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    cancellation.cancel();
    let command_result = tokio::time::timeout(Duration::from_secs(3), command)
        .await
        .expect("取消后的命令收集不应无限等待")
        .expect("执行任务不应 panic");
    let attempts_before_shutdown = close_attempts.load(Ordering::Acquire);
    let retry_deadline_valid = retry_deadline_was_future.load(Ordering::Acquire);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let shutdown = executor.shutdown_until(deadline).await;
    assert!(
        command_result.is_err(),
        "首个 close 故意失败，调用方应看到清理错误"
    );
    assert_eq!(
        attempts_before_shutdown, 2,
        "取消后应立即用新 deadline 重试一次"
    );
    assert!(
        retry_deadline_valid,
        "长命令的清理重试 deadline 必须晚于当前时刻"
    );
    shutdown.expect("shutdown deadline 应允许 worker 用新时限完成资源清理");
}

/// Worker 清理首次失败时 finish 应可见报错，后续 close 必须驱动同一个资源 owner 重试。
#[tokio::test]
async fn stream_finish_reports_cleanup_failure_and_close_retries_same_worker() {
    let input_state = Arc::new(GatedInput::new());
    let (scripted, close_attempts) = ScriptedCommandStream::with_close_failures(
        vec![
            Some(CommandEvent::Exited {
                code: Some(0),
                signal: None,
            }),
            None,
        ],
        input_state,
        1,
    );
    let stream = CommandStreamHandle::new(Box::new(scripted));
    let host_id = HostId::new();
    let handle = Arc::new(FakeRemoteHandle::new(host_id.clone(), stream));
    let executor =
        StructuredOperationsCommandExecutor::new(Arc::new(FakeRemoteConnections { handle }));
    let mut command = executor
        .execute_stream(
            CommandExecutionTarget::Host(host_id),
            CommandInvocation::new("true", Vec::new()),
            limits(1024),
            CancellationToken::new(),
        )
        .await
        .expect("应创建受控远端流");

    let first_finish = tokio::time::timeout(Duration::from_secs(3), command.finish())
        .await
        .expect("清理失败应及时返回，不能等输出 sender 被 worker 丢弃")
        .expect_err("首次远端 channel close 故意失败，应向调用方报告")
        .public_code();
    assert_eq!(
        first_finish,
        kerminal_application::PublicErrorCode::Infrastructure
    );
    assert_eq!(close_attempts.load(Ordering::Acquire), 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    command
        .close_until(deadline)
        .await
        .expect("同一个 worker 应用新 deadline 重试并完成清理");
    assert_eq!(close_attempts.load(Ordering::Acquire), 2);
    executor
        .shutdown_until(deadline)
        .await
        .expect("完成资源关闭后 executor 应无遗留 worker");
}

/// 已完成 worker 会 drop 状态 sender；finish 仍须在队列有待读输出时 drain 并返回成功回执。
#[tokio::test]
async fn stream_finish_drains_queued_output_after_completion_sender_drop() {
    let input_state = Arc::new(GatedInput::new());
    let (scripted, close_attempts) = ScriptedCommandStream::with_close_failures(
        vec![
            Some(CommandEvent::Stdout(b"queued-first".to_vec())),
            Some(CommandEvent::Stdout(b"queued-second".to_vec())),
            Some(CommandEvent::Exited {
                code: Some(0),
                signal: None,
            }),
            None,
        ],
        input_state,
        0,
    );
    let stream = CommandStreamHandle::new(Box::new(scripted));
    let host_id = HostId::new();
    let handle = Arc::new(FakeRemoteHandle::new(host_id.clone(), stream));
    let executor =
        StructuredOperationsCommandExecutor::new(Arc::new(FakeRemoteConnections { handle }));
    let mut command = executor
        .execute_stream(
            CommandExecutionTarget::Host(host_id),
            CommandInvocation::new("true", Vec::new()),
            limits(1024),
            CancellationToken::new(),
        )
        .await
        .expect("应创建受控远端流");

    tokio::time::timeout(Duration::from_secs(3), async {
        while close_attempts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("远端 worker 应先读完事件并进入 close");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    executor
        .shutdown_until(deadline)
        .await
        .expect("已完成的 worker 应在 shutdown 前释放 permit");

    let exit = command
        .finish()
        .await
        .expect("sender 已释放且队列有输出时 finish 仍应完成 drain");
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(close_attempts.load(Ordering::Acquire), 1);
}

struct GatedInput {
    reader_started: tokio::sync::Notify,
    first_write: AtomicBool,
    written: Mutex<Vec<u8>>,
    stdin_closed: AtomicBool,
    progress: tokio::sync::watch::Sender<(usize, bool)>,
}

struct EarlyExitInput {
    write_attempted: tokio::sync::Notify,
    accept_write: tokio::sync::Notify,
    attempted_bytes: AtomicUsize,
    accepted_bytes: AtomicUsize,
}

impl EarlyExitInput {
    /// 暂停真实 fake writer 的回执，稳定复现“请求已发起、远端未确认”状态。
    fn new() -> Self {
        Self {
            write_attempted: tokio::sync::Notify::new(),
            accept_write: tokio::sync::Notify::new(),
            attempted_bytes: AtomicUsize::new(0),
            accepted_bytes: AtomicUsize::new(0),
        }
    }

    /// 让退出事件只在 writer 已收到输入请求后释放，避免依赖调度时序。
    async fn wait_for_write_attempt(&self) {
        self.write_attempted.notified().await;
    }
}

#[async_trait]
impl RemoteCommandInput for EarlyExitInput {
    /// Writer 收到输入请求后等待确认门闩；worker 自然退出应中断等待且不得 ACK。
    async fn write_stdin(&self, bytes: Vec<u8>) -> AppResult<()> {
        self.attempted_bytes
            .fetch_add(bytes.len(), Ordering::AcqRel);
        self.write_attempted.notify_one();
        self.accept_write.notified().await;
        self.accepted_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
        Ok(())
    }

    /// 该场景在数据回执前退出，不会有正常的 stdin 半关闭。
    async fn close_stdin(&self) -> AppResult<()> {
        Ok(())
    }
}

struct EarlyExitCommandStream {
    input: Arc<EarlyExitInput>,
    events: std::collections::VecDeque<CommandEvent>,
    waited_for_input: bool,
}

impl EarlyExitCommandStream {
    /// 构造等待真实 stdin 尝试后立即退出、并保留最后输出块的脚本流。
    fn new(input: Arc<EarlyExitInput>, events: Vec<CommandEvent>) -> Self {
        Self {
            input,
            events: events.into(),
            waited_for_input: false,
        }
    }
}

#[async_trait]
impl RemoteCommandStream for EarlyExitCommandStream {
    /// 第一条退出事件等待 writer 入场，确保回归覆盖远端提前完成的并发边界。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        if !self.waited_for_input {
            self.input.wait_for_write_attempt().await;
            self.waited_for_input = true;
        }
        Ok(self.events.pop_front())
    }

    /// 暴露受控 writer，使远端退出可中断未确认的写入。
    fn input_handle(&self) -> CommandInputHandle {
        let input: Arc<dyn RemoteCommandInput> = self.input.clone();
        CommandInputHandle::new(input)
    }

    /// fake channel 没有外部资源，关闭回执立即确认。
    async fn close_until(&mut self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

impl GatedInput {
    /// 第一次写入等待 reader 启动，以确定测试覆盖真实的双向背压窗口。
    fn new() -> Self {
        let (progress, _) = tokio::sync::watch::channel((0, false));
        Self {
            reader_started: tokio::sync::Notify::new(),
            first_write: AtomicBool::new(false),
            written: Mutex::new(Vec::new()),
            stdin_closed: AtomicBool::new(false),
            progress,
        }
    }

    /// 返回测试 writer 实际收到的完整 payload 副本。
    fn bytes(&self) -> Vec<u8> {
        self.written.lock().expect("测试输入锁中毒").clone()
    }

    /// 报告 stdin 是否在结果读取期间完成半关闭。
    fn is_closed(&self) -> bool {
        self.stdin_closed.load(Ordering::Acquire)
    }

    /// 让脚本流等待远端真实收齐输入并收到 EOF，避免测试的立即 EOF 制造假的取消竞争。
    async fn wait_for_input_completion(&self, expected_bytes: usize) {
        let mut progress = self.progress.subscribe();
        loop {
            let (written_bytes, closed) = *progress.borrow_and_update();
            if written_bytes >= expected_bytes && closed {
                return;
            }
            progress.changed().await.expect("测试输入进度通道提前关闭");
        }
    }
}

#[async_trait]
impl RemoteCommandInput for GatedInput {
    /// 首次输入必须等 reader 开始读取，复现“先完整写再读”会卡住的远端流场景。
    async fn write_stdin(&self, bytes: Vec<u8>) -> AppResult<()> {
        if !self.first_write.swap(true, Ordering::AcqRel) {
            tokio::time::timeout(Duration::from_millis(250), self.reader_started.notified())
                .await
                .map_err(|_| AppError::timeout("测试 writer 等待 reader"))?;
        }
        let written_bytes = {
            let mut written = self
                .written
                .lock()
                .map_err(|_| AppError::infrastructure("测试输入锁中毒"))?;
            written.extend_from_slice(&bytes);
            written.len()
        };
        self.progress
            .send_replace((written_bytes, self.is_closed()));
        Ok(())
    }

    /// 记录半关闭，保证 writer 完成后 reader 仍可 drain 剩余事件。
    async fn close_stdin(&self) -> AppResult<()> {
        self.stdin_closed.store(true, Ordering::Release);
        let written_bytes = self
            .written
            .lock()
            .map_err(|_| AppError::infrastructure("测试输入锁中毒"))?
            .len();
        self.progress.send_replace((written_bytes, true));
        Ok(())
    }
}

struct ScriptedCommandStream {
    events: Mutex<std::collections::VecDeque<Option<CommandEvent>>>,
    input: Arc<GatedInput>,
    close_failures_remaining: usize,
    close_attempts: Arc<AtomicUsize>,
    input_completion_gate: Option<usize>,
}

struct PendingCleanupCommandStream {
    input: Arc<GatedInput>,
    close_attempts: Arc<AtomicUsize>,
    retry_deadline_was_future: Arc<AtomicBool>,
}

#[async_trait]
impl RemoteCommandStream for PendingCleanupCommandStream {
    /// 测试命令保持运行，直到调用方取消；worker 随后必须只走关闭与 deadline 重试路径。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        self.input.reader_started.notify_one();
        pending().await
    }

    /// 输入泵仍使用生产契约的独立 writer，不让测试跳过 stdin 生命周期。
    fn input_handle(&self) -> CommandInputHandle {
        let input: Arc<dyn RemoteCommandInput> = self.input.clone();
        CommandInputHandle::new(input)
    }

    /// 首次关闭注入失败，第二次检查调用方提供的新 deadline，随后允许关闭完成。
    async fn close_until(&mut self, deadline: tokio::time::Instant) -> AppResult<()> {
        let attempt = self.close_attempts.fetch_add(1, Ordering::AcqRel);
        if attempt == 0 {
            return Err(AppError::infrastructure("测试模拟首次 channel 清理失败"));
        }
        if attempt == 1 {
            let is_future = deadline > tokio::time::Instant::now();
            self.retry_deadline_was_future
                .store(is_future, Ordering::Release);
            if !is_future {
                return Err(AppError::timeout("测试检测到清理 deadline 已过期"));
            }
        }
        Ok(())
    }
}

impl ScriptedCommandStream {
    /// 以显式事件序列注入远端流，让退出状态与 EOF 的顺序可重复验证。
    fn new(events: Vec<Option<CommandEvent>>, input: Arc<GatedInput>) -> Self {
        Self {
            events: Mutex::new(events.into()),
            input,
            close_failures_remaining: 0,
            close_attempts: Arc::new(AtomicUsize::new(0)),
            input_completion_gate: None,
        }
    }

    /// 仅在被测远端命令已消费完整请求并收到 EOF 后释放首个脚本事件。
    fn gate_events_until_input_complete(mut self, expected_bytes: usize) -> Self {
        self.input_completion_gate = Some(expected_bytes);
        self
    }

    /// 注入有限次数的关闭失败并返回计数器，以验证 cleanup owner 在重试前后始终可观察。
    fn with_close_failures(
        events: Vec<Option<CommandEvent>>,
        input: Arc<GatedInput>,
        failures: usize,
    ) -> (Self, Arc<AtomicUsize>) {
        let close_attempts = Arc::new(AtomicUsize::new(0));
        (
            Self {
                events: Mutex::new(events.into()),
                input,
                close_failures_remaining: failures,
                close_attempts: close_attempts.clone(),
                input_completion_gate: None,
            },
            close_attempts,
        )
    }
}

#[async_trait]
impl RemoteCommandStream for ScriptedCommandStream {
    /// 事件读取启动时解除 writer 门闩，模拟 channel 读写并行运行。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        self.input.reader_started.notify_one();
        if let Some(expected_bytes) = self.input_completion_gate.take() {
            self.input.wait_for_input_completion(expected_bytes).await;
        }
        self.events
            .lock()
            .map_err(|_| AppError::infrastructure("测试事件锁中毒"))?
            .pop_front()
            .ok_or_else(|| AppError::infrastructure("测试事件序列缺少 EOF"))
    }

    /// 返回独立 writer 句柄，符合远端读写并行契约。
    fn input_handle(&self) -> CommandInputHandle {
        let input: Arc<dyn RemoteCommandInput> = self.input.clone();
        CommandInputHandle::new(input)
    }

    /// fake stream 无底层资源，遵守调用方提供的绝对关闭 deadline。
    async fn close_until(&mut self, _deadline: tokio::time::Instant) -> AppResult<()> {
        let attempt = self.close_attempts.fetch_add(1, Ordering::AcqRel);
        if attempt < self.close_failures_remaining {
            Err(AppError::infrastructure(
                "测试模拟远端 channel close 首次失败",
            ))
        } else {
            Ok(())
        }
    }
}

struct FakeRemoteHandle {
    host_id: HostId,
    stream: Mutex<Option<kerminal_application::connections::CommandStreamHandle>>,
}

impl FakeRemoteHandle {
    /// 保存一个一次性 exec stream，避免测试伪造共享 SSH 的二次生命周期。
    fn new(
        host_id: HostId,
        stream: kerminal_application::connections::CommandStreamHandle,
    ) -> Self {
        Self {
            host_id,
            stream: Mutex::new(Some(stream)),
        }
    }
}

#[async_trait]
impl RemoteConnectionHandle for FakeRemoteHandle {
    /// 返回本测试明确绑定的主机标识。
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// fake lease 仅开放 SSH 连接能力，不模拟协议能力变化。
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities::for_protocol(RemoteProtocol::Ssh)
    }

    /// 方言仅满足 lease 契约，测试不进行命令字符串编码。
    fn shell_dialect(&self) -> RemoteShellDialect {
        RemoteShellDialect::Posix
    }

    /// 当前场景只测试 exec，因此交互 terminal 请求必须失败。
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported("测试未配置 terminal"))
    }

    /// 返回唯一 scripted stream，验证应用调用不会另建终端 registry。
    async fn exec_stream(
        &self,
        _request: ExecRequest,
        _cancel: CancellationToken,
    ) -> AppResult<kerminal_application::connections::CommandStreamHandle> {
        self.stream
            .lock()
            .map_err(|_| AppError::infrastructure("测试 exec 锁中毒"))?
            .take()
            .ok_or_else(|| AppError::conflict("测试 exec stream 已消费"))
    }

    /// 该 fake 不接受原始 shell 脚本，避免测试用例绕过结构化 argv 边界。
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<kerminal_application::connections::CommandStreamHandle> {
        Err(AppError::unsupported("测试未配置原始 shell 执行"))
    }

    /// 当前场景不打开 SFTP subsystem。
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        Err(AppError::unsupported("测试未配置 SFTP"))
    }

    /// 当前场景不建立 TCP tunnel。
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported("测试未配置 tunnel"))
    }

    /// 当前场景不请求远端 listener。
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Err(AppError::unsupported("测试未配置 remote forward"))
    }

    /// fake lease 不持有后台资源，遵守绝对 deadline 契约并立即完成关闭。
    async fn close_until(&self, _deadline: tokio::time::Instant, _reason: &str) -> AppResult<()> {
        Ok(())
    }
}

struct FakeRemoteConnections {
    handle: Arc<FakeRemoteHandle>,
}

#[async_trait]
impl RemoteConnectionService for FakeRemoteConnections {
    /// 返回已注入 lease，并拒绝不同目标以证明命令保持显式绑定。
    async fn connect(
        &self,
        host_id: HostId,
        intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        if host_id != self.handle.host_id || intent != ConnectionIntent::Exec {
            return Err(AppError::invalid("测试连接目标或用途不匹配"));
        }
        Ok(RemoteConnectionLease::new(self.handle.clone()))
    }

    /// 当前场景不查询 host key。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("测试未配置主机指纹查询"))
    }

    /// 当前场景不修改 known_hosts。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("测试未配置主机信任"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

/// 创建当前测试二进制的隔离子进程调用，避免依赖外部 shell 或系统工具。
fn helper_invocation(test_name: &str) -> CommandInvocation {
    let executable = std::env::current_exe().expect("获取测试二进制路径失败");
    CommandInvocation {
        program: executable.to_string_lossy().into_owned(),
        arguments: vec![
            "--exact".to_owned(),
            test_name.to_owned(),
            "--nocapture".to_owned(),
        ],
        environment: BTreeMap::new(),
        working_directory: None,
    }
}

/// 在 payload 两端放唯一标记，以便从测试 harness 的文字输出中还原精确二进制数据。
fn large_binary_payload(body_bytes: usize) -> Vec<u8> {
    let mut payload = b"KERM-INPUT-BEGIN".to_vec();
    payload.extend((0..body_bytes).map(|index| (index % 251) as u8));
    payload.extend_from_slice(b"KERM-INPUT-END");
    payload
}

/// 二进制回归失败只显示长度和首个差异位置，避免大 payload 膨胀测试日志。
///
/// @author kongweiguang
fn assert_payload_matches(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len(), "回显 payload 长度不一致");
    let first_difference = actual
        .iter()
        .zip(expected)
        .position(|(actual_byte, expected_byte)| actual_byte != expected_byte);
    assert_eq!(first_difference, None, "回显 payload 首个差异字节偏移");
}

/// 给测试命令配置足够大的输出预算，同时仍验证结果受显式上限约束。
fn limits(max_output_bytes: usize) -> CommandExecutionLimits {
    CommandExecutionLimits {
        timeout: Duration::from_secs(10),
        max_output_bytes,
    }
}
