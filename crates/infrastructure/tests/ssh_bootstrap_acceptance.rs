//! SSH 交互 shell bootstrap 的真实 PTY 验收。
//!
//! 测试经正式 SSH transport、SessionManager 与终端 parser 检查 Bash bootstrap 是否到达
//! OSC 133 A/B 和可信空提示符。它仅接受 KERM_ACCEPT_SSH_TARGET、
//! KERM_ACCEPT_SSH_USERNAME、KERM_ACCEPT_SSH_PRIVATE_KEY_FILE、
//! KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE 四个任务 fixture 输入，并将目标限制为
//! 127.0.0.1:22661。初始观察期间测试不额外向 PTY 写入键盘输入；输出仅含布尔值、计数和长度，
//! 不记录或显示终端快照正文。单独出现的 “>” 只作为续行提示候选，不作为 PS2 证明；
//! SSH lease 与会话资源使用同一绝对清理期限，期限耗尽按验收失败处理。
//! 全流 scanner 在 create 前接入 reader；仅在 close 后汇总连续 sequence 与跨 chunk 模式计数。
//! input_bytes_sent 只计算额外测试输入，不把启动请求或 shell 自身输出算成测试键盘输入。
//! 保留的 bootstrap_payload_bytes_including_cr 指 PTY stdin bootstrap 字节；exec 路径为零。
//! bootstrap_send_acknowledged 保留历史字段名，现与 exec_request_acknowledged 一样只表示
//! 此 Bash 默认交互路径收到 SSH exec Success，不表示 hook 已执行；就绪仍必须由 OSC tracker 证明。
//! 合成 resize trace 默认只驻留有界内存；显式设置 KERM_ACCEPT_EXPORT_SSH_RESIZE_TRACE=1
//! 才会将旧 pane 的 no-profile 合成 Bash prompt 与 p/q 阶段写入 target/diagnostics。
//!
//! @author kongweiguang

// 复用共享 SSH harness 的凭据解析器和连接装配；其余 fixture 由其他集成测试使用。
#[allow(dead_code)]
mod support;

#[path = "support/ssh_bootstrap_output_recorder.rs"]
mod ssh_bootstrap_output_recorder;

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use kerminal_application::{
    connections::{ConnectionIntent, CredentialResolver, RemoteConnectionService, RemoteShellKind},
    sessions::{
        CreateSessionRequest, PlainTextSnapshotRequest, SessionService, SessionTransportFactory,
        TerminalCommandAvailability,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshRoute,
    },
    identity::{HostId, PaneId, TabId},
    sessions::{SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget, TerminalSize},
};
use kerminal_infrastructure::{
    connections::{KnownHostsStore, SshSessionTransportFactory},
    local_pty::LocalPtySessionTransportFactory,
    sessions::{
        CompositeSessionTransportFactory, SessionManager, SessionRuntimeConfig, SessionTargetKind,
    },
    storage::StorageCoordinator,
};
use tokio::{
    runtime::Handle,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use ssh_bootstrap_output_recorder::FullStreamRecorder;
use support::ssh_harness::{TestCredentialResolver, connection_service};

const SSH_FIXTURE_TARGET: &str = "127.0.0.1:22661";
const SSH_FIXTURE_KEY_REFERENCE: &str = "acceptance:ssh-bootstrap-client-key";
const OBSERVATION_BUDGET: Duration = Duration::from_secs(24);
const CREATE_BUDGET: Duration = Duration::from_secs(28);
const CLEANUP_BUDGET: Duration = Duration::from_secs(90);
const OCTAL_BOOTSTRAP_PREFIX: &str = r"\154\137\160";

/// 只保存固定 loopback 目标和任务显式注入的 fixture 文件位置。
struct SshFixtureConfig {
    target: SocketAddr,
    username: String,
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
}

impl SshFixtureConfig {
    /// 四项环境输入必须齐全且目标端点固定，避免忽略验收误连本机以外的 SSH 服务。
    ///
    /// @author kongweiguang
    fn from_environment() -> Result<Self, &'static str> {
        let target = env::var("KERM_ACCEPT_SSH_TARGET")
            .map_err(|_| "SSH fixture target environment is missing")?
            .parse::<SocketAddr>()
            .map_err(|_| "SSH fixture target is invalid")?;
        let username = env::var("KERM_ACCEPT_SSH_USERNAME")
            .map_err(|_| "SSH fixture username environment is missing")?;
        let private_key_file = env::var_os("KERM_ACCEPT_SSH_PRIVATE_KEY_FILE")
            .map(PathBuf::from)
            .ok_or("SSH fixture private-key environment is missing")?;
        let known_hosts_file = env::var_os("KERM_ACCEPT_SSH_KNOWN_HOSTS_FILE")
            .map(PathBuf::from)
            .ok_or("SSH fixture known-hosts environment is missing")?;
        let expected = SSH_FIXTURE_TARGET
            .parse::<SocketAddr>()
            .map_err(|_| "SSH fixture target constant is invalid")?;

        if target != expected || target.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err("SSH bootstrap acceptance only permits the task-private loopback target");
        }
        if username.trim().is_empty() {
            return Err("SSH fixture username is empty");
        }
        if !private_key_file.is_file() || !known_hosts_file.is_file() {
            return Err("SSH fixture key or known-hosts input is unavailable");
        }

        Ok(Self {
            target,
            username,
            private_key_file,
            known_hosts_file,
        })
    }

    /// 仅从已校验的 loopback socket 生成 SSH endpoint，不读取系统 SSH 配置。
    ///
    /// @author kongweiguang
    fn endpoint(&self) -> HostEndpoint {
        HostEndpoint {
            host: self.target.ip().to_string(),
            port: self.target.port(),
            username: self.username.clone(),
        }
    }
}

/// 仅输出可以安全聚合的 bootstrap、OSC tracker 与提示符证据。
#[derive(Default)]
struct AcceptanceObservation {
    fixture_shell_is_bash: bool,
    session_created: bool,
    session_running: bool,
    bootstrap_send_acknowledged: bool,
    bootstrap_payload_bytes_including_cr: usize,
    exec_request_acknowledged: bool,
    shell_integration_observed: bool,
    osc133_a_observed: bool,
    osc133_a_epoch: u64,
    osc133_b_observed: bool,
    trusted_prompt: bool,
    prompt_line_empty: bool,
    literal_octal_prefix_present: bool,
    literal_octal_escape_count: usize,
    continuation_prompt_candidate: bool,
    snapshots_sampled: usize,
}

/// 会话与 remote 服务共享临时 known-hosts 生命周期，别名保留这组装配返回值的所有权关系。
type SessionManagerFixture = (
    Arc<SessionManager>,
    HostId,
    Arc<dyn RemoteConnectionService>,
    tempfile::TempDir,
);

/// 用正式 SSH transport 装配 Bash PTY；recorder 必须在 create 前接入，避免漏掉启动首包。
///
/// @author kongweiguang
fn build_session_manager<R>(
    config: &SshFixtureConfig,
    recorder: Arc<R>,
) -> Result<SessionManagerFixture, &'static str>
where
    R: kerminal_application::sessions::logging::SessionOutputRecorder + 'static,
{
    let private_key = Zeroizing::new(
        fs::read(&config.private_key_file).map_err(|_| "SSH fixture key could not be read")?,
    );
    if private_key.is_empty() {
        return Err("SSH fixture key is empty");
    }

    let credentials = Arc::new(TestCredentialResolver::default());
    credentials.insert_private_key(SSH_FIXTURE_KEY_REFERENCE, &private_key);
    let credentials: Arc<dyn CredentialResolver> = credentials;

    let known_hosts_root =
        tempfile::tempdir().map_err(|_| "private known-hosts root unavailable")?;
    fs::copy(
        &config.known_hosts_file,
        known_hosts_root.path().join("known_hosts"),
    )
    .map_err(|_| "task-private known-hosts input could not be copied")?;
    let known_hosts = Arc::new(KnownHostsStore::new(
        StorageCoordinator::new(known_hosts_root.path()),
        false,
    ));

    let credential_ref = CredentialRef::new(SSH_FIXTURE_KEY_REFERENCE)
        .map_err(|_| "fixture reference is invalid")?;
    let profile = HostConnectionProfile {
        host_id: HostId::new(),
        protocol: RemoteProtocol::Ssh,
        endpoint: config.endpoint(),
        authentication: Some(SshAuthentication::PrivateKey {
            path: None,
            secret: Some(credential_ref),
            passphrase: None,
        }),
        rdp_password: None,
        route: SshRoute {
            proxy: None,
            jump_hosts: Vec::new(),
        },
        shell_dialect: RemoteShellDialect::Posix,
        connect_timeout_ms: 20_000,
        keepalive_interval_ms: None,
    };
    let host_id = profile.host_id.clone();
    let remote: Arc<dyn RemoteConnectionService> =
        connection_service(profile, credentials, known_hosts);

    let local_transport: Arc<dyn SessionTransportFactory> =
        Arc::new(LocalPtySessionTransportFactory::new());
    let ssh_transport: Arc<dyn SessionTransportFactory> =
        Arc::new(SshSessionTransportFactory::new(Arc::clone(&remote)));
    let transports: Arc<dyn SessionTransportFactory> = Arc::new(
        CompositeSessionTransportFactory::new(local_transport)
            .with_provider(SessionTargetKind::Ssh, ssh_transport),
    );
    let sessions = Arc::new(
        SessionManager::new(
            transports,
            Handle::current(),
            SessionRuntimeConfig::default(),
        )
        .with_output_recorder(recorder),
    );

    Ok((sessions, host_id, remote, known_hosts_root))
}

/// 通过真实认证后的只读 shell 探测确认 fixture 为 Bash，并在同一关闭期限内释放连接。
///
/// @author kongweiguang
async fn verify_fixture_shell(
    remote: &dyn RemoteConnectionService,
    host_id: HostId,
    cancellation: CancellationToken,
    cleanup_deadline: Instant,
) -> bool {
    let Ok(lease) = remote
        .connect(host_id, ConnectionIntent::Terminal, cancellation)
        .await
    else {
        return false;
    };
    let is_bash = lease.shell_kind() == RemoteShellKind::Bash;
    let closed = lease
        .close_until(cleanup_deadline, "SSH bootstrap acceptance shell probe")
        .await
        .is_ok();
    is_bash && closed
}

/// 在一个绝对观察期限内采样有限纯文本快照和可信 prompt tracker，不泄露快照内容。
///
/// @author kongweiguang
async fn observe_prompt(
    sessions: &SessionManager,
    session_id: &kerminal_domain::identity::SessionId,
    deadline: Instant,
    observation: &mut AcceptanceObservation,
) {
    loop {
        if let Ok(metadata) = sessions.runtime_metadata(session_id.clone()).await {
            // 本测试固定 Bash/default shell；正式 transport 仅在 exec Success 后发布 integration enabled。
            // 此处只映射启动请求 ACK，不用它替代下方 OSC tracker 和完整原始流中的 marker。
            observation.bootstrap_send_acknowledged = metadata.shell_integration_enabled;
            observation.exec_request_acknowledged = metadata.shell_integration_enabled;
            observation.shell_integration_observed |= metadata.shell_integration_observed;
            observation.osc133_b_observed |= metadata.prompt_ready;
        }

        if let Ok(context) = sessions.terminal_command_context(session_id.clone()).await {
            observation.osc133_a_epoch = observation.osc133_a_epoch.max(context.prompt_epoch);
            observation.osc133_a_observed |= context.prompt_epoch > 0;
            observation.osc133_b_observed |= context.prompt_ready;
            observation.prompt_line_empty = context.prompt_line_empty;
            observation.trusted_prompt = context.availability
                == TerminalCommandAvailability::Available
                && context.prompt_ready
                && context.prompt_line_empty;
        }

        if let Ok(snapshot) = sessions
            .plaintext_snapshot(
                session_id.clone(),
                PlainTextSnapshotRequest {
                    include_scrollback: false,
                    max_lines: 12,
                    max_bytes: Some(4_096),
                },
            )
            .await
        {
            observation.snapshots_sampled = observation.snapshots_sampled.saturating_add(1);
            observation.literal_octal_prefix_present |=
                snapshot.text.contains(OCTAL_BOOTSTRAP_PREFIX);
            observation.literal_octal_escape_count = observation
                .literal_octal_escape_count
                .max(count_literal_octal_escapes(&snapshot.text));
            observation.continuation_prompt_candidate |= !observation.trusted_prompt
                && snapshot
                    .text
                    .lines()
                    .last()
                    .is_some_and(|line| line.trim() == ">");
        }

        if observation.trusted_prompt || Instant::now() >= deadline {
            return;
        }
        sleep_until((Instant::now() + Duration::from_millis(120)).min(deadline)).await;
    }
}

/// 统计文本中可见的反斜线加三位八进制形式，只返回数量而不保留匹配内容。
///
/// @author kongweiguang
fn count_literal_octal_escapes(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0usize;
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        if bytes[index] == b'\\'
            && bytes[index + 1..index + 4]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            count = count.saturating_add(1);
            index += 4;
        } else {
            index += 1;
        }
    }
    count
}

/// 用四变量任务私有配置跑一遍真实 SSH PTY；即使 prompt 未就绪也先统一关闭会话再断言。
///
/// 初始采样期间不额外写入测试键盘输入，避免用 Ctrl-C 或用户命令掩盖 bootstrap 失败。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private loopback OpenSSH fixture"]
async fn ssh_bash_pty_bootstrap_reports_redacted_prompt_state() {
    let config = SshFixtureConfig::from_environment().unwrap_or_else(|reason| panic!("{reason}"));
    let recorder = Arc::new(FullStreamRecorder::default());
    let (sessions, host_id, remote, _known_hosts_root) =
        build_session_manager(&config, Arc::clone(&recorder))
            .unwrap_or_else(|reason| panic!("{reason}"));
    let mut observation = AcceptanceObservation {
        // exec command 属于 SSH 控制请求，不能将脚本长度误报成 PTY stdin 发送字节。
        bootstrap_payload_bytes_including_cr: 0,
        ..AcceptanceObservation::default()
    };

    let cleanup_deadline = Instant::now() + CLEANUP_BUDGET;
    observation.fixture_shell_is_bash = verify_fixture_shell(
        remote.as_ref(),
        host_id.clone(),
        CancellationToken::new(),
        cleanup_deadline,
    )
    .await;

    if observation.fixture_shell_is_bash {
        let cancellation = CancellationToken::new();
        let create_deadline = Instant::now() + CREATE_BUDGET;
        let request = CreateSessionRequest {
            target: SessionTarget::Ssh(SshSessionTarget {
                host_id,
                working_directory: None,
                command: None,
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::default(),
        };
        let create_operation = sessions.create_cancellable(request, cancellation.clone());
        tokio::pin!(create_operation);
        let create_result = tokio::select! {
            result = &mut create_operation => result,
            _ = sleep_until(create_deadline) => {
                cancellation.cancel();
                create_operation.await
            }
        };

        if let Ok(session) = create_result {
            observation.session_created = true;
            observation.session_running = session.lifecycle == SessionLifecycle::Running;
            let observation_deadline = Instant::now() + OBSERVATION_BUDGET;
            observe_prompt(
                &sessions,
                &session.session_id,
                observation_deadline,
                &mut observation,
            )
            .await;
        }
    }

    let cleanup_completed = sessions.close_all_until(cleanup_deadline).await.is_ok();
    let stream = recorder.evidence();
    println!(
        concat!(
            "SSH_BOOTSTRAP_ACCEPTANCE ",
            "fixture_shell_is_bash={} session_created={} session_running={} ",
            "bootstrap_send_acknowledged={} bootstrap_payload_bytes_including_cr={} ",
            "shell_integration_observed={} osc133_a_observed={} osc133_a_epoch={} ",
            "osc133_b_observed={} trusted_prompt={} prompt_line_empty={} ",
            "literal_octal_prefix_present={} literal_octal_escape_count={} ",
            "continuation_prompt_candidate={} snapshots_sampled={} ",
            "input_bytes_sent=0 cleanup_completed={} exec_request_acknowledged={} ",
            "stream_bytes={} stream_chunks={} stream_first_sequence={} stream_last_sequence={} ",
            "stream_sequence_gap_count={} stream_recorder_failed_count={} stream_recorder_finished={} ",
            "stream_output_after_finish_count={} stream_session_mismatch_count={} ",
            "stream_literal_octal_prefix_present={} stream_literal_octal_escape_count={} ",
            "stream_osc133_a_count={} stream_osc133_b_count={} stream_complete={}"
        ),
        observation.fixture_shell_is_bash,
        observation.session_created,
        observation.session_running,
        observation.bootstrap_send_acknowledged,
        observation.bootstrap_payload_bytes_including_cr,
        observation.shell_integration_observed,
        observation.osc133_a_observed,
        observation.osc133_a_epoch,
        observation.osc133_b_observed,
        observation.trusted_prompt,
        observation.prompt_line_empty,
        observation.literal_octal_prefix_present,
        observation.literal_octal_escape_count,
        observation.continuation_prompt_candidate,
        observation.snapshots_sampled,
        cleanup_completed,
        observation.exec_request_acknowledged,
        stream.bytes,
        stream.chunks,
        stream.first_sequence,
        stream.last_sequence,
        stream.sequence_gap_count,
        stream.recorder_failed_count,
        stream.recorder_finished,
        stream.output_after_finish_count,
        stream.session_mismatch_count,
        stream.literal_octal_prefix_present,
        stream.literal_octal_escape_count,
        stream.osc133_a_count,
        stream.osc133_b_count,
        stream.complete,
    );

    assert!(
        observation.fixture_shell_is_bash,
        "fixture shell probe did not confirm Bash"
    );
    assert!(
        observation.session_created,
        "SSH PTY session was not created"
    );
    assert!(
        observation.session_running,
        "SSH PTY session did not reach Running"
    );
    assert!(
        observation.bootstrap_send_acknowledged,
        "interactive shell startup request was not acknowledged"
    );
    assert!(
        observation.exec_request_acknowledged,
        "SSH exec request was not acknowledged"
    );
    assert!(observation.osc133_a_observed, "OSC 133 A was not observed");
    assert!(observation.osc133_b_observed, "OSC 133 B was not observed");
    assert!(
        stream.complete,
        "initial SSH output stream evidence is incomplete"
    );
    assert!(
        stream.osc133_a_count > 0,
        "complete OSC 133 A was absent from raw output"
    );
    assert!(
        stream.osc133_b_count > 0,
        "complete OSC 133 B was absent from raw output"
    );
    assert!(
        !stream.literal_octal_prefix_present,
        "bootstrap octal prefix appeared in initial SSH output"
    );
    assert_eq!(
        stream.literal_octal_escape_count, 0,
        "literal octal escapes appeared in initial SSH output"
    );
    assert!(
        observation.trusted_prompt,
        "trusted empty shell prompt was not observed"
    );
    assert!(
        !observation.literal_octal_prefix_present,
        "bootstrap octal escape prefix appeared as visible terminal text"
    );
    assert_eq!(
        observation.literal_octal_escape_count, 0,
        "literal octal escapes appeared as visible terminal text"
    );
    assert!(
        cleanup_completed,
        "SSH PTY cleanup did not complete before its deadline"
    );
}

const MAX_RESIZE_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_RESIZE_CAPTURE_CHUNKS: usize = 4_096;
const RESIZE_FIXTURE_ROWS: u16 = 33;
const INITIAL_FIXTURE_COLUMNS: u16 = 120;
const SPLIT_FIXTURE_COLUMNS: u16 = 59;
const SYNTHETIC_PROMPT_R_PREFIX_COLUMNS: u16 = 180;
const RESIZE_TRACE_EXPORT_ENV: &str = "KERM_ACCEPT_EXPORT_SSH_RESIZE_TRACE";
const RESIZE_ACK_DIAGNOSTICS_ENV: &str = "KERM_ACCEPT_SSH_RESIZE_ACK_DIAGNOSTICS";
const R49_TRACE_MAGIC: &[u8; 8] = b"R49BASH1";
const RESIZE_MARKER_BUDGET: Duration = Duration::from_secs(18);

/// 原始输出只在测试进程内有界保留，以便逐字节重放同一 synthetic PTY stream。
struct ResizeTraceChunk {
    session_id: kerminal_domain::identity::SessionId,
    sequence: u64,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct ResizeTraceState {
    chunks: Vec<ResizeTraceChunk>,
    byte_count: usize,
}

/// 为忽略型 loopback 验收测试保存短期、定量有界的 PTY 输出，不写盘或打印原文。
#[derive(Default)]
struct BoundedResizeTraceRecorder {
    state: std::sync::Mutex<ResizeTraceState>,
    failed: std::sync::atomic::AtomicBool,
}

impl BoundedResizeTraceRecorder {
    /// 只复制指定 session 的已记录数据，并验证 reader sequence 连续，避免用缺口流做 A/B。
    ///
    /// @author kongweiguang
    fn chunks_for(
        &self,
        session_id: &kerminal_domain::identity::SessionId,
    ) -> Result<Vec<ResizeTraceChunk>, &'static str> {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err("bounded SSH resize trace recorder overflowed or was contended");
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "bounded SSH resize trace recorder mutex was poisoned")?;
        let mut chunks = state
            .chunks
            .iter()
            .filter(|chunk| &chunk.session_id == session_id)
            .map(|chunk| ResizeTraceChunk {
                session_id: chunk.session_id.clone(),
                sequence: chunk.sequence,
                bytes: chunk.bytes.clone(),
            })
            .collect::<Vec<_>>();
        chunks.sort_by_key(|chunk| chunk.sequence);
        let mut expected_sequence = 1_u64;
        for chunk in &chunks {
            if chunk.sequence != expected_sequence {
                return Err("bounded SSH resize trace has a reader sequence gap");
            }
            expected_sequence = expected_sequence.saturating_add(1);
        }
        if chunks.is_empty() {
            return Err("bounded SSH resize trace has no PTY output");
        }
        Ok(chunks)
    }

    /// 返回已经完成 terminal parsing 的阶段边界所对应的最新 reader sequence。
    ///
    /// @author kongweiguang
    fn latest_sequence(
        &self,
        session_id: &kerminal_domain::identity::SessionId,
    ) -> Result<u64, &'static str> {
        self.chunks_for(session_id)?
            .last()
            .map(|chunk| chunk.sequence)
            .ok_or("bounded SSH resize trace has no sequence")
    }
}

/// 仅在显式 opt-in 时导出旧 pane 的固定合成 PTY 字节；默认验收仍不落盘、不打印原文。
///
/// 文件只包含本测试生成的无 profile Bash prompt 与 p/q 交互阶段，带固定尺寸、阶段边界和
/// 连续 reader sequence；以 create_new 防止覆写，且不写 session、host 或凭据标识。
///
/// @author kongweiguang
fn export_old_resize_trace_if_opted_in(
    session_id: &kerminal_domain::identity::SessionId,
    chunks: &[ResizeTraceChunk],
    initial_sequence: u64,
    p_sequence: u64,
    q_sequence: u64,
) -> Result<Option<PathBuf>, &'static str> {
    if env::var_os(RESIZE_TRACE_EXPORT_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(None);
    }
    if initial_sequence == 0 || initial_sequence >= p_sequence || p_sequence >= q_sequence {
        return Err("synthetic SSH resize trace boundaries are invalid");
    }

    const RECORD_HEADER_BYTES: usize = 1 + 8 + 4;
    const TRACE_HEADER_BYTES: usize = 8 + 8 + 24;
    let mut encoded = Vec::with_capacity(
        TRACE_HEADER_BYTES
            + MAX_RESIZE_CAPTURE_BYTES
            + MAX_RESIZE_CAPTURE_CHUNKS * RECORD_HEADER_BYTES
            + RECORD_HEADER_BYTES,
    );
    encoded.extend_from_slice(R49_TRACE_MAGIC);
    for value in [
        INITIAL_FIXTURE_COLUMNS,
        SPLIT_FIXTURE_COLUMNS,
        RESIZE_FIXTURE_ROWS,
        SYNTHETIC_PROMPT_R_PREFIX_COLUMNS,
    ] {
        encoded.extend_from_slice(&value.to_le_bytes());
    }
    for sequence in [initial_sequence, p_sequence, q_sequence] {
        encoded.extend_from_slice(&sequence.to_le_bytes());
    }

    let mut expected_sequence = 1_u64;
    let mut total_payload_bytes = 0_usize;
    let mut record_count = 0_usize;
    let mut stage_counts = [0_usize; 4];
    for chunk in chunks
        .iter()
        .take_while(|chunk| chunk.sequence <= q_sequence)
    {
        if &chunk.session_id != session_id || chunk.sequence != expected_sequence {
            return Err("synthetic SSH resize trace has a session mismatch or sequence gap");
        }
        if chunk.bytes.is_empty() || chunk.bytes.len() > u32::MAX as usize {
            return Err("synthetic SSH resize trace has an invalid reader chunk");
        }
        let stage = if chunk.sequence <= initial_sequence {
            1_u8
        } else if chunk.sequence <= p_sequence {
            2_u8
        } else {
            3_u8
        };
        total_payload_bytes = total_payload_bytes.saturating_add(chunk.bytes.len());
        record_count = record_count.saturating_add(1);
        if total_payload_bytes > MAX_RESIZE_CAPTURE_BYTES
            || record_count > MAX_RESIZE_CAPTURE_CHUNKS
        {
            return Err("synthetic SSH resize trace exceeded its capture budget");
        }
        stage_counts[usize::from(stage)] = stage_counts[usize::from(stage)].saturating_add(1);
        encoded.push(stage);
        encoded.extend_from_slice(&chunk.sequence.to_le_bytes());
        encoded.extend_from_slice(&(chunk.bytes.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&chunk.bytes);
        expected_sequence = expected_sequence.saturating_add(1);
    }
    if expected_sequence != q_sequence.saturating_add(1) || stage_counts[1..].contains(&0) {
        return Err("synthetic SSH resize trace is missing a stage boundary");
    }

    encoded.push(0);
    encoded.extend_from_slice(&0_u64.to_le_bytes());
    encoded.extend_from_slice(&0_u32.to_le_bytes());
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let diagnostics_directory = repository_root.join("target").join("diagnostics");
    if !diagnostics_directory.is_dir() {
        return Err("target diagnostics directory is unavailable");
    }
    let path = diagnostics_directory.join(format!(
        "ssh-bash-resize-xterm-r49-{}.r49bin",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "a fresh synthetic SSH resize trace file could not be created")?;
    if file.write_all(&encoded).is_err() {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err("synthetic SSH resize trace could not be written completely");
    }
    drop(file);
    Ok(Some(
        path.strip_prefix(&repository_root)
            .map(PathBuf::from)
            .unwrap_or(path),
    ))
}

#[async_trait::async_trait]
impl kerminal_application::sessions::logging::SessionOutputRecorder for BoundedResizeTraceRecorder {
    /// reader 回调只做有界 try-lock 与内存复制；容量或争用失败会让验收显式失败。
    ///
    /// @author kongweiguang
    fn try_record_output(
        &self,
        session_id: &kerminal_domain::identity::SessionId,
        sequence: u64,
        bytes: &[u8],
    ) -> kerminal_application::sessions::logging::OutputRecordResult {
        let Ok(mut state) = self.state.try_lock() else {
            self.failed
                .store(true, std::sync::atomic::Ordering::Release);
            return kerminal_application::sessions::logging::OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: kerminal_application::sessions::logging::SessionLogFailureReason::QueueFull,
            };
        };
        if state.chunks.len() >= MAX_RESIZE_CAPTURE_CHUNKS
            || state.byte_count.saturating_add(bytes.len()) > MAX_RESIZE_CAPTURE_BYTES
        {
            self.failed
                .store(true, std::sync::atomic::Ordering::Release);
            return kerminal_application::sessions::logging::OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: kerminal_application::sessions::logging::SessionLogFailureReason::QueueFull,
            };
        }
        state.byte_count = state.byte_count.saturating_add(bytes.len());
        state.chunks.push(ResizeTraceChunk {
            session_id: session_id.clone(),
            sequence,
            bytes: bytes.to_vec(),
        });
        kerminal_application::sessions::logging::OutputRecordResult::Queued
    }

    /// 记录器不持有 writer 或文件；会话关闭后的证据仍只留在当前测试的有界内存里。
    ///
    /// @author kongweiguang
    async fn finish_session(
        &self,
        _session_id: &kerminal_domain::identity::SessionId,
        _deadline: std::time::Instant,
    ) -> kerminal_application::AppResult<kerminal_application::sessions::logging::SessionLogState>
    {
        Ok(kerminal_application::sessions::logging::SessionLogState::inactive())
    }

    /// 无独立 writer 任务；所有 trace 在关闭 session 后由测试同步核对并随作用域释放。
    ///
    /// @author kongweiguang
    async fn shutdown_until(
        &self,
        _deadline: std::time::Instant,
    ) -> kerminal_application::AppResult<()> {
        Ok(())
    }
}

/// 以固定行数和无像素尺寸创建 fixture 网格，避免把窗口 DPI 换算混入 PTY 尺寸变量。
///
/// @author kongweiguang
fn resize_fixture_size(columns: u16) -> TerminalSize {
    TerminalSize::new(columns, RESIZE_FIXTURE_ROWS, 0, 0)
        .expect("fixture terminal dimensions are non-zero")
}

/// 为完全隔离的 no-profile Bash 构造合成长 prompt；可选的 `PROMPT_COMMAND` 只给诊断夹具装置信号尺寸回执。
///
/// @author kongweiguang
fn synthetic_bash_prompt(with_resize_ack: bool) -> (String, Option<String>, String) {
    let nonce = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .to_ascii_uppercase();
    let marker = format!("KRMEND{nonce}Z");
    let resize_ack_token = with_resize_ack.then(|| format!("KRMRSZ{nonce}Z"));
    let prompt = format!(
        "{}{}>",
        "R".repeat(usize::from(SYNTHETIC_PROMPT_R_PREFIX_COLUMNS)),
        marker
    );
    let prompt_command = resize_ack_token.as_ref().map(|token| {
        let trap =
            format!("trap 'printf \"\\033]777;{token};%s\\007\" \"$(stty size </dev/tty)\"' WINCH");
        shell_single_quote(&format!("PROMPT_COMMAND={trap}"))
    });
    let prompt_command = prompt_command
        .map(|value| format!(" {value}"))
        .unwrap_or_default();
    let command = format!(
        "env -i PATH=/usr/local/bin:/usr/bin:/bin TERM=xterm-256color HISTFILE=/dev/null{prompt_command} PS1='{prompt}' bash --noprofile --norc -i"
    );
    (marker, resize_ack_token, command)
}

/// 用 POSIX 单引号传递 fixture-only PROMPT_COMMAND，避免外层远程命令提前执行 stty 或展开 trap 内容。
///
/// @author kongweiguang
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 在有界 recorder 字节中查找本次 resize 请求之后的 OSC 尺寸回执，避免旧同尺寸 marker 被复用。
///
/// @author kongweiguang
fn resize_ack_sequence(
    chunks: &[ResizeTraceChunk],
    token: &str,
    expected_size: TerminalSize,
    after_sequence_exclusive: u64,
) -> Result<Option<u64>, &'static str> {
    let prefix = format!("\x1b]777;{token};").into_bytes();
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(&chunk.bytes);
    }

    let mut search_from = 0usize;
    while let Some(prefix_offset) = bytes[search_from..]
        .windows(prefix.len())
        .position(|window| window == prefix)
    {
        let payload_start = search_from + prefix_offset + prefix.len();
        let Some(payload_length) = bytes[payload_start..].iter().position(|byte| *byte == 0x07)
        else {
            return Ok(None);
        };
        let payload_end = payload_start + payload_length;
        let payload = std::str::from_utf8(&bytes[payload_start..payload_end])
            .map_err(|_| "synthetic Bash resize ACK dimensions are not ASCII")?;
        let mut dimensions = payload.split_whitespace();
        let rows = dimensions
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or("synthetic Bash resize ACK rows are invalid")?;
        let columns = dimensions
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or("synthetic Bash resize ACK columns are invalid")?;
        if dimensions.next().is_some() {
            return Err("synthetic Bash resize ACK has extra dimension fields");
        }
        let marker_end = payload_end;
        let mut cumulative_bytes = 0usize;
        let marker_sequence = chunks.iter().find_map(|chunk| {
            cumulative_bytes = cumulative_bytes.saturating_add(chunk.bytes.len());
            (marker_end < cumulative_bytes).then_some(chunk.sequence)
        });
        let Some(marker_sequence) = marker_sequence else {
            return Err("synthetic Bash resize ACK has no reader sequence");
        };
        if rows == expected_size.rows()
            && columns == expected_size.columns()
            && marker_sequence > after_sequence_exclusive
        {
            return Ok(Some(marker_sequence));
        }
        search_from = payload_end.saturating_add(1);
    }
    Ok(None)
}

/// 等待 no-profile Bash 的 SIGWINCH trap 回执；它只证明远端 PTY 尺寸可见，不代表 Readline 锚点已同步。
///
/// @author kongweiguang
async fn wait_for_resize_ack(
    sessions: &SessionManager,
    recorder: &BoundedResizeTraceRecorder,
    session_id: &kerminal_domain::identity::SessionId,
    token: &str,
    expected_size: TerminalSize,
    after_sequence_exclusive: u64,
    deadline: Instant,
) -> Result<u64, &'static str> {
    let mut subscription = sessions
        .subscribe(session_id.clone())
        .await
        .map_err(|_| "SSH resize ACK subscription could not be opened")?;
    let cancellation = CancellationToken::new();
    loop {
        let chunks = recorder.chunks_for(session_id)?;
        if let Some(sequence) =
            resize_ack_sequence(&chunks, token, expected_size, after_sequence_exclusive)?
        {
            return Ok(sequence);
        }
        tokio::select! {
            _ = sleep_until(deadline) => {
                return Err("synthetic Bash did not ACK the requested PTY size before deadline");
            }
            change = subscription.recv(&cancellation) => {
                if !matches!(change, Ok(Some(_))) {
                    return Err("SSH session stopped before synthetic Bash resize ACK");
                }
            }
        }
    }
}

/// 检查渲染快照中的可见 runs；只用于合成 marker/input，不重建或输出终端正文。
///
/// @author kongweiguang
fn resize_snapshot_contains(
    snapshot: &kerminal_terminal::snapshot::RenderSnapshot,
    needle: &str,
) -> bool {
    snapshot.rows.iter().any(|row| {
        let row_text = row
            .runs
            .iter()
            .map(|run| run.text.as_ref())
            .collect::<String>();
        row_text.contains(needle)
    })
}

/// 等待确定的可见文本条件或 session change，不用固定延时猜测远端 shell 已经回显。
///
/// @author kongweiguang
async fn wait_for_resize_snapshot_text(
    sessions: &SessionManager,
    session_id: &kerminal_domain::identity::SessionId,
    needle: &str,
    deadline: Instant,
) -> Result<kerminal_terminal::snapshot::RenderSnapshot, &'static str> {
    let mut subscription = sessions
        .subscribe(session_id.clone())
        .await
        .map_err(|_| "SSH resize session subscription could not be opened")?;
    let cancellation = CancellationToken::new();
    loop {
        let snapshot = sessions
            .render_snapshot(
                session_id.clone(),
                kerminal_application::sessions::RenderSnapshotRequest::default(),
            )
            .await
            .map_err(|_| "SSH resize render snapshot could not be read")?;
        if resize_snapshot_contains(&snapshot, needle) {
            return Ok(snapshot);
        }
        tokio::select! {
            _ = sleep_until(deadline) => {
                return Err("SSH resize fixture did not reach its synthetic marker before deadline");
            }
            change = subscription.recv(&cancellation) => {
                if !matches!(change, Ok(Some(_))) {
                    return Err("SSH resize session stopped before its synthetic marker appeared");
                }
            }
        }
    }
}

/// 从明确 session ID 的列表结果读取 size/lifecycle，不能以 snapshot 网格替代 transport 投影。
///
/// @author kongweiguang
async fn resize_session_info(
    sessions: &SessionManager,
    session_id: &kerminal_domain::identity::SessionId,
) -> Result<kerminal_domain::sessions::SessionInfo, &'static str> {
    sessions
        .list(kerminal_application::sessions::SessionFilter::default())
        .await
        .map_err(|_| "SSH resize session list could not be read")?
        .into_iter()
        .find(|info| &info.session_id == session_id)
        .ok_or("SSH resize session disappeared from the manager")
}

/// 只汇总渲染 cells 的数值布局、cursor cell ASCII code 与合成 marker 可见性。
#[derive(Debug)]
struct ResizeStageMetrics {
    columns: u16,
    rows: u16,
    cursor_row: u16,
    cursor_column: u16,
    cursor_cell_ascii_code: Option<u8>,
    row_last_cell_columns: Vec<u16>,
    marker_visible: bool,
    p_visible: bool,
    q_visible: bool,
}

/// 提取限定到 synthetic fixture 的格点指标，不把 prompt、命令或原始 PTY bytes 写进输出。
///
/// @author kongweiguang
fn resize_stage_metrics(
    snapshot: &kerminal_terminal::snapshot::RenderSnapshot,
    marker: &str,
) -> ResizeStageMetrics {
    let row_last_cell_columns = snapshot
        .rows
        .iter()
        .map(|row| {
            row.runs
                .iter()
                .flat_map(|run| run.cell_spans.iter())
                .map(|span| span.column_range.end)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let cursor_cell_ascii_code = snapshot.cursor_cell.as_ref().and_then(|cell| {
        let mut bytes = cell.text.bytes();
        let first = bytes.next()?;
        (bytes.next().is_none() && first.is_ascii()).then_some(first)
    });
    ResizeStageMetrics {
        columns: snapshot.size.columns(),
        rows: snapshot.size.rows(),
        cursor_row: snapshot.cursor.row,
        cursor_column: snapshot.cursor.column,
        cursor_cell_ascii_code,
        row_last_cell_columns,
        marker_visible: resize_snapshot_contains(snapshot, marker),
        p_visible: resize_snapshot_contains(snapshot, "p"),
        q_visible: resize_snapshot_contains(snapshot, "q"),
    }
}

struct ResizeSequenceReport {
    old_initial_info: kerminal_domain::sessions::SessionInfo,
    old_narrow_info: kerminal_domain::sessions::SessionInfo,
    old_wide_info: kerminal_domain::sessions::SessionInfo,
    old_running_after_new_close: bool,
    new_initial_info: kerminal_domain::sessions::SessionInfo,
    old_actual_initial: ResizeStageMetrics,
    old_actual_narrow_after_resize: ResizeStageMetrics,
    old_actual_narrow_before_p: ResizeStageMetrics,
    old_actual_narrow_after_p: ResizeStageMetrics,
    old_actual_wide_after_resize: ResizeStageMetrics,
    old_actual_wide_before_q: ResizeStageMetrics,
    old_actual_wide_after_q: ResizeStageMetrics,
    new_actual_initial: ResizeStageMetrics,
    old_replay: [ResizeStageMetrics; 5],
    new_replay_initial: ResizeStageMetrics,
    old_trace_bytes: usize,
    old_trace_chunks: usize,
    old_initial_sequence: u64,
    old_p_sequence: u64,
    old_q_sequence: u64,
    resize_ack_observations: Option<[ResizeAckObservation; 2]>,
    old_trace_export_path: Option<PathBuf>,
    new_trace_bytes: usize,
    new_trace_chunks: usize,
}

#[derive(Debug)]
struct ResizeAckObservation {
    expected_rows: u16,
    expected_columns: u16,
    before_resize_sequence: u64,
    sequence_sampled_after_resize_return: u64,
    remote_ack_sequence: u64,
    pre_input_sequence_before_snapshot: u64,
    pre_input_sequence: u64,
    after_ack: ResizeStageMetrics,
}

/// 逐字节回放旧 pane 的同一 reader stream，阶段边界由初始 marker、p 和 q 的 reader sequence 固定。
///
/// @author kongweiguang
fn replay_old_resize_trace(
    session_id: kerminal_domain::identity::SessionId,
    chunks: &[ResizeTraceChunk],
    initial_sequence: u64,
    p_sequence: u64,
    q_sequence: u64,
    marker: &str,
) -> Result<[ResizeStageMetrics; 5], &'static str> {
    if initial_sequence >= p_sequence || p_sequence >= q_sequence {
        return Err("SSH resize fixture did not produce distinct input trace boundaries");
    }
    let mut engine = kerminal_terminal::TerminalEngine::new(
        session_id,
        resize_fixture_size(INITIAL_FIXTURE_COLUMNS),
        kerminal_terminal::TerminalLimits::default(),
    )
    .map_err(|_| "Alacritty engine rejected initial SSH fixture dimensions")?;
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.sequence <= initial_sequence)
    {
        engine.process_output(&chunk.bytes);
    }
    let initial = resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    );
    engine
        .resize(resize_fixture_size(SPLIT_FIXTURE_COLUMNS))
        .map_err(|_| "Alacritty engine rejected split SSH fixture dimensions")?;
    let narrow_before_p = resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    );
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.sequence > initial_sequence && chunk.sequence <= p_sequence)
    {
        engine.process_output(&chunk.bytes);
    }
    let narrow_after_p = resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    );
    engine
        .resize(resize_fixture_size(INITIAL_FIXTURE_COLUMNS))
        .map_err(|_| "Alacritty engine rejected expanded SSH fixture dimensions")?;
    let wide_before_q = resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    );
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.sequence > p_sequence && chunk.sequence <= q_sequence)
    {
        engine.process_output(&chunk.bytes);
    }
    let wide_after_q = resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    );
    Ok([
        initial,
        narrow_before_p,
        narrow_after_p,
        wide_before_q,
        wide_after_q,
    ])
}

/// 在同一窄尺寸用独立 Alacritty engine 回放新 pane 初始输出，区分新网格与旧 pane resize 路径。
///
/// @author kongweiguang
fn replay_new_pane_trace(
    session_id: kerminal_domain::identity::SessionId,
    chunks: &[ResizeTraceChunk],
    through_sequence: u64,
    marker: &str,
) -> Result<ResizeStageMetrics, &'static str> {
    let mut engine = kerminal_terminal::TerminalEngine::new(
        session_id,
        resize_fixture_size(SPLIT_FIXTURE_COLUMNS),
        kerminal_terminal::TerminalLimits::default(),
    )
    .map_err(|_| "Alacritty engine rejected new-pane SSH fixture dimensions")?;
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.sequence <= through_sequence)
    {
        engine.process_output(&chunk.bytes);
    }
    Ok(resize_stage_metrics(
        &engine.render_snapshot(kerminal_application::sessions::RenderSnapshotRequest::default()),
        marker,
    ))
}

/// 用固定 loopback SSH fixture 建立旧/新 pane，采集 120→59→120 的远端 Bash synthetic prompt 阶段；
/// opt-in ACK 变体等待 Bash trap 回报尺寸；与 raw 导出使用独立开关，reader 序号可将两者对齐。
///
/// SSH channel window_change 是真实 transport 请求；它不是远端 shell 尺寸 ACK。此测试仅证明
/// manager 的已提交 SessionInfo 与同流 Alacritty replay，不冒充 GPUI 原生窗口或远端尺寸测量。
///
/// @author kongweiguang
async fn run_ssh_bash_resize_sequence(
    sessions: &SessionManager,
    host_id: HostId,
    remote: &dyn RemoteConnectionService,
    recorder: &BoundedResizeTraceRecorder,
    cleanup_deadline: Instant,
    resize_ack_diagnostics: bool,
) -> Result<ResizeSequenceReport, &'static str> {
    if !verify_fixture_shell(
        remote,
        host_id.clone(),
        CancellationToken::new(),
        cleanup_deadline,
    )
    .await
    {
        return Err("private SSH fixture did not confirm a clean Bash shell");
    }

    let tab_id = TabId::new();
    let old_pane_id = PaneId::new();
    let (old_marker, old_resize_ack_token, old_command) =
        synthetic_bash_prompt(resize_ack_diagnostics);
    let old_request = CreateSessionRequest {
        target: SessionTarget::Ssh(SshSessionTarget {
            host_id: host_id.clone(),
            working_directory: None,
            command: Some(kerminal_domain::sessions::SshInitialCommand::Shell(
                old_command,
            )),
        }),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: old_pane_id,
        },
        size: resize_fixture_size(INITIAL_FIXTURE_COLUMNS),
    };
    let old_cancel = CancellationToken::new();
    let old_create = sessions.create_cancellable(old_request, old_cancel.clone());
    tokio::pin!(old_create);
    let old_info = tokio::select! {
        result = &mut old_create => result,
        _ = sleep_until(Instant::now() + CREATE_BUDGET) => {
            old_cancel.cancel();
            old_create.await
        }
    }
    .map_err(|_| "old SSH pane session could not be created")?;
    if old_info.lifecycle != SessionLifecycle::Running {
        return Err("old SSH pane session did not reach Running");
    }
    let old_initial_snapshot = wait_for_resize_snapshot_text(
        sessions,
        &old_info.session_id,
        &old_marker,
        Instant::now() + RESIZE_MARKER_BUDGET,
    )
    .await?;
    let old_initial_sequence = recorder.latest_sequence(&old_info.session_id)?;
    let old_initial_info = resize_session_info(sessions, &old_info.session_id).await?;

    // reader 水位与后续 resize 调用不是跨线程/transport 的原子快照；它只划定请求前已观察到的输出。
    let old_narrow_before_resize_sequence = recorder.latest_sequence(&old_info.session_id)?;
    sessions
        .resize(
            old_info.session_id.clone(),
            resize_fixture_size(SPLIT_FIXTURE_COLUMNS),
        )
        .await
        .map_err(|_| "old SSH pane resize to split width failed")?;
    let old_narrow_resize_return_sequence = recorder.latest_sequence(&old_info.session_id)?;
    let old_narrow_snapshot = sessions
        .render_snapshot(
            old_info.session_id.clone(),
            kerminal_application::sessions::RenderSnapshotRequest::default(),
        )
        .await
        .map_err(|_| "old SSH pane narrow snapshot could not be read")?;
    let old_narrow_info = resize_session_info(sessions, &old_info.session_id).await?;
    let mut old_narrow_ack_observation = if let Some(token) = old_resize_ack_token.as_deref() {
        let remote_ack_sequence = wait_for_resize_ack(
            sessions,
            recorder,
            &old_info.session_id,
            token,
            resize_fixture_size(SPLIT_FIXTURE_COLUMNS),
            old_narrow_before_resize_sequence,
            Instant::now() + RESIZE_MARKER_BUDGET,
        )
        .await?;
        let after_ack_snapshot = sessions
            .render_snapshot(
                old_info.session_id.clone(),
                kerminal_application::sessions::RenderSnapshotRequest::default(),
            )
            .await
            .map_err(|_| "old SSH pane snapshot after narrow remote ACK could not be read")?;
        Some(ResizeAckObservation {
            expected_rows: RESIZE_FIXTURE_ROWS,
            expected_columns: SPLIT_FIXTURE_COLUMNS,
            before_resize_sequence: old_narrow_before_resize_sequence,
            sequence_sampled_after_resize_return: old_narrow_resize_return_sequence,
            remote_ack_sequence,
            pre_input_sequence_before_snapshot: 0,
            pre_input_sequence: 0,
            after_ack: resize_stage_metrics(&after_ack_snapshot, &old_marker),
        })
    } else {
        None
    };

    let new_pane_id = PaneId::new();
    let (new_marker, _new_resize_ack_token, new_command) = synthetic_bash_prompt(false);
    let new_request = CreateSessionRequest {
        target: SessionTarget::Ssh(SshSessionTarget {
            host_id,
            working_directory: None,
            command: Some(kerminal_domain::sessions::SshInitialCommand::Shell(
                new_command,
            )),
        }),
        owner: SessionOwner::Ui {
            tab_id,
            pane_id: new_pane_id,
        },
        size: resize_fixture_size(SPLIT_FIXTURE_COLUMNS),
    };
    let new_cancel = CancellationToken::new();
    let new_create = sessions.create_cancellable(new_request, new_cancel.clone());
    tokio::pin!(new_create);
    let new_info = tokio::select! {
        result = &mut new_create => result,
        _ = sleep_until(Instant::now() + CREATE_BUDGET) => {
            new_cancel.cancel();
            new_create.await
        }
    }
    .map_err(|_| "new SSH pane session could not be created")?;
    if new_info.lifecycle != SessionLifecycle::Running {
        return Err("new SSH pane session did not reach Running");
    }
    let new_initial_snapshot = wait_for_resize_snapshot_text(
        sessions,
        &new_info.session_id,
        &new_marker,
        Instant::now() + RESIZE_MARKER_BUDGET,
    )
    .await?;
    let new_initial_sequence = recorder.latest_sequence(&new_info.session_id)?;
    let new_chunks = recorder.chunks_for(&new_info.session_id)?;
    let new_initial_info = resize_session_info(sessions, &new_info.session_id).await?;

    // 前后水位与 snapshot 只是连续采样，不是 reader、engine 和 transport 的原子快照。
    let pre_p_sequence_before_snapshot = recorder.latest_sequence(&old_info.session_id)?;
    let old_narrow_before_p_snapshot = sessions
        .render_snapshot(
            old_info.session_id.clone(),
            kerminal_application::sessions::RenderSnapshotRequest::default(),
        )
        .await
        .map_err(|_| "old SSH pane immediate pre-p snapshot could not be read")?;
    let pre_p_sequence = recorder.latest_sequence(&old_info.session_id)?;
    if let Some(observation) = old_narrow_ack_observation.as_mut() {
        observation.pre_input_sequence_before_snapshot = pre_p_sequence_before_snapshot;
        observation.pre_input_sequence = pre_p_sequence;
    }

    sessions
        .write(old_info.session_id.clone(), b"p".to_vec())
        .await
        .map_err(|_| "synthetic p input could not be written to old SSH pane")?;
    let old_narrow_after_p_snapshot = wait_for_resize_snapshot_text(
        sessions,
        &old_info.session_id,
        "p",
        Instant::now() + RESIZE_MARKER_BUDGET,
    )
    .await?;
    let p_sequence = recorder.latest_sequence(&old_info.session_id)?;

    sessions
        .close_until(new_info.session_id.clone(), cleanup_deadline)
        .await
        .map_err(|_| "new SSH pane did not close before expansion")?;
    let old_running_after_new_close = resize_session_info(sessions, &old_info.session_id).await?;
    // 与窄化同样，水位只表示本地 recorder 的顺序观察，不和 SSH resize 原子同步。
    let old_wide_before_resize_sequence = recorder.latest_sequence(&old_info.session_id)?;
    sessions
        .resize(
            old_info.session_id.clone(),
            resize_fixture_size(INITIAL_FIXTURE_COLUMNS),
        )
        .await
        .map_err(|_| "old SSH pane resize to initial width failed")?;
    let old_wide_resize_return_sequence = recorder.latest_sequence(&old_info.session_id)?;
    let old_wide_after_resize_snapshot = sessions
        .render_snapshot(
            old_info.session_id.clone(),
            kerminal_application::sessions::RenderSnapshotRequest::default(),
        )
        .await
        .map_err(|_| "old SSH pane expanded snapshot could not be read")?;
    let old_wide_info = resize_session_info(sessions, &old_info.session_id).await?;
    let mut old_wide_ack_observation = if let Some(token) = old_resize_ack_token.as_deref() {
        let remote_ack_sequence = wait_for_resize_ack(
            sessions,
            recorder,
            &old_info.session_id,
            token,
            resize_fixture_size(INITIAL_FIXTURE_COLUMNS),
            old_wide_before_resize_sequence,
            Instant::now() + RESIZE_MARKER_BUDGET,
        )
        .await?;
        let after_ack_snapshot = sessions
            .render_snapshot(
                old_info.session_id.clone(),
                kerminal_application::sessions::RenderSnapshotRequest::default(),
            )
            .await
            .map_err(|_| "old SSH pane snapshot after wide remote ACK could not be read")?;
        Some(ResizeAckObservation {
            expected_rows: RESIZE_FIXTURE_ROWS,
            expected_columns: INITIAL_FIXTURE_COLUMNS,
            before_resize_sequence: old_wide_before_resize_sequence,
            sequence_sampled_after_resize_return: old_wide_resize_return_sequence,
            remote_ack_sequence,
            pre_input_sequence_before_snapshot: 0,
            pre_input_sequence: 0,
            after_ack: resize_stage_metrics(&after_ack_snapshot, &old_marker),
        })
    } else {
        None
    };
    // 与 p 相同，先后 reader 水位用于识别 snapshot 期间是否有迟到输出。
    let pre_q_sequence_before_snapshot = recorder.latest_sequence(&old_info.session_id)?;
    let old_wide_before_q_snapshot = sessions
        .render_snapshot(
            old_info.session_id.clone(),
            kerminal_application::sessions::RenderSnapshotRequest::default(),
        )
        .await
        .map_err(|_| "old SSH pane immediate pre-q snapshot could not be read")?;
    let pre_q_sequence = recorder.latest_sequence(&old_info.session_id)?;
    if let Some(observation) = old_wide_ack_observation.as_mut() {
        observation.pre_input_sequence_before_snapshot = pre_q_sequence_before_snapshot;
        observation.pre_input_sequence = pre_q_sequence;
    }
    sessions
        .write(old_info.session_id.clone(), b"q".to_vec())
        .await
        .map_err(|_| "synthetic q input could not be written to old SSH pane")?;
    let old_wide_after_q_snapshot = wait_for_resize_snapshot_text(
        sessions,
        &old_info.session_id,
        "q",
        Instant::now() + RESIZE_MARKER_BUDGET,
    )
    .await?;
    let q_sequence = recorder.latest_sequence(&old_info.session_id)?;

    let old_chunks = recorder.chunks_for(&old_info.session_id)?;
    // 导出器自身的 opt-in、容量、连续序号与 create_new 约束保持唯一；ACK 水位在报告中关联。
    let old_trace_export_path = export_old_resize_trace_if_opted_in(
        &old_info.session_id,
        &old_chunks,
        old_initial_sequence,
        p_sequence,
        q_sequence,
    )?;
    let old_replay = replay_old_resize_trace(
        old_info.session_id.clone(),
        &old_chunks,
        old_initial_sequence,
        p_sequence,
        q_sequence,
        &old_marker,
    )?;
    let new_replay_initial = replay_new_pane_trace(
        new_info.session_id,
        &new_chunks,
        new_initial_sequence,
        &new_marker,
    )?;
    let old_trace_bytes = old_chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    let new_trace_bytes = new_chunks.iter().map(|chunk| chunk.bytes.len()).sum();

    let resize_ack_observations = match (old_narrow_ack_observation, old_wide_ack_observation) {
        (Some(narrow), Some(wide)) => Some([narrow, wide]),
        (None, None) => None,
        _ => return Err("synthetic Bash resize ACK observations are incomplete"),
    };

    Ok(ResizeSequenceReport {
        old_initial_info,
        old_narrow_info,
        old_wide_info,
        old_running_after_new_close: old_running_after_new_close.lifecycle
            == SessionLifecycle::Running,
        new_initial_info,
        old_actual_initial: resize_stage_metrics(&old_initial_snapshot, &old_marker),
        old_actual_narrow_after_resize: resize_stage_metrics(&old_narrow_snapshot, &old_marker),
        old_actual_narrow_before_p: resize_stage_metrics(
            &old_narrow_before_p_snapshot,
            &old_marker,
        ),
        old_actual_narrow_after_p: resize_stage_metrics(&old_narrow_after_p_snapshot, &old_marker),
        old_actual_wide_after_resize: resize_stage_metrics(
            &old_wide_after_resize_snapshot,
            &old_marker,
        ),
        old_actual_wide_before_q: resize_stage_metrics(&old_wide_before_q_snapshot, &old_marker),
        old_actual_wide_after_q: resize_stage_metrics(&old_wide_after_q_snapshot, &old_marker),
        new_actual_initial: resize_stage_metrics(&new_initial_snapshot, &new_marker),
        old_replay,
        new_replay_initial,
        old_trace_bytes,
        old_trace_chunks: old_chunks.len(),
        old_initial_sequence,
        old_p_sequence: p_sequence,
        old_q_sequence: q_sequence,
        resize_ack_observations,
        old_trace_export_path,
        new_trace_bytes,
        new_trace_chunks: new_chunks.len(),
    })
}

/// 忽略型真实 SSH fixture 测试：保留真实 window_change 与 Bash 输入回显，再用同流重放 Alacritty。
///
/// 只把合成 PS1 和 p/q 字节送入无 profile/rc 的 Bash，不发送 Enter 或用户命令；默认原文
/// 只在 64 KiB 内存缓冲中短暂存在，显式 opt-in 才落入新建的 target/diagnostics trace 文件。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private loopback OpenSSH fixture"]
async fn ssh_bash_existing_and_new_panes_capture_split_resize_trace() {
    let config = SshFixtureConfig::from_environment().unwrap_or_else(|reason| panic!("{reason}"));
    let recorder = Arc::new(BoundedResizeTraceRecorder::default());
    let (sessions, host_id, remote, _known_hosts_root) =
        build_session_manager(&config, Arc::clone(&recorder))
            .unwrap_or_else(|reason| panic!("{reason}"));
    let cleanup_deadline = Instant::now() + Duration::from_secs(240);
    let result = run_ssh_bash_resize_sequence(
        sessions.as_ref(),
        host_id,
        remote.as_ref(),
        recorder.as_ref(),
        cleanup_deadline,
        false,
    )
    .await;
    let cleanup_completed = sessions.close_all_until(cleanup_deadline).await.is_ok();
    assert!(
        cleanup_completed,
        "SSH resize fixture sessions did not close before their deadline"
    );
    let report = result.unwrap_or_else(|reason| panic!("{reason}"));
    // 检查数值证据自身的网格约束，不把同流 replay 相同误当成 Bash 重排正确。
    for metrics in [
        &report.old_actual_initial,
        &report.old_actual_narrow_after_resize,
        &report.old_actual_narrow_before_p,
        &report.old_actual_narrow_after_p,
        &report.old_actual_wide_after_resize,
        &report.old_actual_wide_before_q,
        &report.old_actual_wide_after_q,
        &report.new_actual_initial,
        &report.new_replay_initial,
    ]
    .into_iter()
    .chain(report.old_replay.iter())
    {
        assert!(metrics.cursor_row < metrics.rows);
        assert!(metrics.cursor_column < metrics.columns);
        assert!(
            metrics
                .cursor_cell_ascii_code
                .is_none_or(|byte| byte.is_ascii())
        );
        assert!(
            metrics
                .row_last_cell_columns
                .iter()
                .all(|column| *column <= metrics.columns)
        );
    }
    println!(
        "SSH_BASH_RESIZE_TRACE old_bytes={} old_chunks={} new_bytes={} new_chunks={} old_initial_size={} old_narrow_size={} old_wide_size={} new_initial_size={} old_running_after_new_close={} old_initial={:?} old_narrow_after_resize={:?} old_narrow_before_p={:?} old_narrow_after_p={:?} old_wide_after_resize={:?} old_wide_before_q={:?} old_wide_after_q={:?} new_initial={:?} old_replay={:?} new_replay_initial={:?}",
        report.old_trace_bytes,
        report.old_trace_chunks,
        report.new_trace_bytes,
        report.new_trace_chunks,
        report.old_initial_info.size.columns(),
        report.old_narrow_info.size.columns(),
        report.old_wide_info.size.columns(),
        report.new_initial_info.size.columns(),
        report.old_running_after_new_close,
        report.old_actual_initial,
        report.old_actual_narrow_after_resize,
        report.old_actual_narrow_before_p,
        report.old_actual_narrow_after_p,
        report.old_actual_wide_after_resize,
        report.old_actual_wide_before_q,
        report.old_actual_wide_after_q,
        report.new_actual_initial,
        report.old_replay,
        report.new_replay_initial,
    );
    if let Some(path) = &report.old_trace_export_path {
        println!(
            "SSH_BASH_RESIZE_TRACE_EXPORTED path={} initial_sequence={} p_sequence={} q_sequence={} dimensions=120x33,59x33,120x33 synthetic_prompt_r_columns={}",
            path.display(),
            report.old_initial_sequence,
            report.old_p_sequence,
            report.old_q_sequence,
            SYNTHETIC_PROMPT_R_PREFIX_COLUMNS,
        );
    }
    assert_eq!(
        report.old_initial_info.size.columns(),
        INITIAL_FIXTURE_COLUMNS
    );
    assert_eq!(report.old_narrow_info.size.columns(), SPLIT_FIXTURE_COLUMNS);
    assert_eq!(report.old_wide_info.size.columns(), INITIAL_FIXTURE_COLUMNS);
    assert_eq!(
        report.new_initial_info.size.columns(),
        SPLIT_FIXTURE_COLUMNS
    );
    assert_eq!(report.old_actual_initial.columns, INITIAL_FIXTURE_COLUMNS);
    assert_eq!(
        report.old_actual_narrow_before_p.columns,
        SPLIT_FIXTURE_COLUMNS
    );
    assert_eq!(
        report.old_actual_narrow_after_p.columns,
        SPLIT_FIXTURE_COLUMNS
    );
    assert_eq!(
        report.old_actual_wide_before_q.columns,
        INITIAL_FIXTURE_COLUMNS
    );
    assert_eq!(
        report.old_actual_wide_after_q.columns,
        INITIAL_FIXTURE_COLUMNS
    );
    assert_eq!(report.new_actual_initial.columns, SPLIT_FIXTURE_COLUMNS);
    assert_eq!(report.old_actual_initial.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.old_actual_narrow_before_p.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.old_actual_narrow_after_p.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.old_actual_wide_before_q.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.old_actual_wide_after_q.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.new_actual_initial.rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(report.old_replay[0].columns, INITIAL_FIXTURE_COLUMNS);
    assert_eq!(report.old_replay[1].columns, SPLIT_FIXTURE_COLUMNS);
    assert_eq!(report.old_replay[2].columns, SPLIT_FIXTURE_COLUMNS);
    assert_eq!(report.old_replay[3].columns, INITIAL_FIXTURE_COLUMNS);
    assert_eq!(report.old_replay[4].columns, INITIAL_FIXTURE_COLUMNS);
    assert_eq!(report.new_replay_initial.columns, SPLIT_FIXTURE_COLUMNS);
    assert!(report.old_running_after_new_close);
    assert!(report.old_actual_initial.marker_visible);
    assert!(report.old_actual_narrow_before_p.marker_visible);
    assert!(report.old_actual_narrow_after_p.marker_visible);
    assert!(report.old_actual_narrow_after_p.p_visible);
    assert!(report.old_actual_wide_before_q.marker_visible);
    assert!(report.old_actual_wide_after_q.marker_visible);
    assert!(report.old_actual_wide_after_q.q_visible);
    assert!(report.new_actual_initial.marker_visible);
    assert!(report.old_replay.iter().all(|stage| stage.marker_visible));
    assert!(report.new_replay_initial.marker_visible);
}

/// 以无 profile trap 对照 Bash 实际 SIGWINCH/stty 尺寸可见顺序；等待 OSC marker 后才写 p/q。
///
/// 默认只输出 reader 序号、PTY 尺寸与 snapshot 数值；raw 文件仍须单独 opt-in，且仅含有界合成流。
/// 导出路径与下方 pre-input reader 水位一并报告，便于按水位划分同一旧 pane 的原始 chunk。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-private loopback OpenSSH fixture and explicit resize ACK opt-in"]
async fn ssh_bash_resize_ack_order_diagnostic() {
    assert_eq!(
        env::var_os(RESIZE_ACK_DIAGNOSTICS_ENV).as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "set KERM_ACCEPT_SSH_RESIZE_ACK_DIAGNOSTICS=1 to run this fixture-only diagnostic"
    );
    let config = SshFixtureConfig::from_environment().unwrap_or_else(|reason| panic!("{reason}"));
    let recorder = Arc::new(BoundedResizeTraceRecorder::default());
    let (sessions, host_id, remote, _known_hosts_root) =
        build_session_manager(&config, Arc::clone(&recorder))
            .unwrap_or_else(|reason| panic!("{reason}"));
    let cleanup_deadline = Instant::now() + Duration::from_secs(240);
    let result = run_ssh_bash_resize_sequence(
        sessions.as_ref(),
        host_id,
        remote.as_ref(),
        recorder.as_ref(),
        cleanup_deadline,
        true,
    )
    .await;
    let cleanup_completed = sessions.close_all_until(cleanup_deadline).await.is_ok();
    assert!(
        cleanup_completed,
        "SSH resize ACK diagnostic sessions did not close before their deadline"
    );
    let report = result.unwrap_or_else(|reason| panic!("{reason}"));
    let export_opted_in =
        env::var_os(RESIZE_TRACE_EXPORT_ENV).as_deref() == Some(std::ffi::OsStr::new("1"));
    assert_eq!(
        report.old_trace_export_path.is_some(),
        export_opted_in,
        "bounded synthetic trace export must follow its independent explicit opt-in"
    );
    let observations = report
        .resize_ack_observations
        .expect("diagnostic run must record both remote resize ACKs");
    assert_eq!(observations[0].expected_rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(observations[0].expected_columns, SPLIT_FIXTURE_COLUMNS);
    assert_eq!(observations[1].expected_rows, RESIZE_FIXTURE_ROWS);
    assert_eq!(observations[1].expected_columns, INITIAL_FIXTURE_COLUMNS);
    for observation in &observations {
        assert!(observation.remote_ack_sequence > observation.before_resize_sequence);
        // 只验证本地 reader 的单调水位；不能断言远端 ACK 必须晚于本地 resize 返回。
        assert!(
            observation.sequence_sampled_after_resize_return >= observation.before_resize_sequence
        );
        assert!(observation.pre_input_sequence_before_snapshot >= observation.remote_ack_sequence);
        assert!(observation.pre_input_sequence >= observation.pre_input_sequence_before_snapshot);
        assert!(observation.pre_input_sequence >= observation.remote_ack_sequence);
        assert_eq!(observation.after_ack.rows, observation.expected_rows);
        assert_eq!(observation.after_ack.columns, observation.expected_columns);
    }
    println!(
        "SSH_BASH_RESIZE_ACK_ORDER with_hook=true info_columns={},{},{} trace_export_path={:?} trace_initial_sequence={} trace_p_sequence={} trace_q_sequence={} old_initial={:?} narrow_after_resize={:?} narrow_before_p={:?} narrow_after_p={:?} wide_after_resize={:?} wide_before_q={:?} wide_after_q={:?} observations={:?} baseline_no_trap_required=true",
        report.old_initial_info.size.columns(),
        report.old_narrow_info.size.columns(),
        report.old_wide_info.size.columns(),
        report.old_trace_export_path,
        report.old_initial_sequence,
        report.old_p_sequence,
        report.old_q_sequence,
        report.old_actual_initial,
        report.old_actual_narrow_after_resize,
        report.old_actual_narrow_before_p,
        report.old_actual_narrow_after_p,
        report.old_actual_wide_after_resize,
        report.old_actual_wide_before_q,
        report.old_actual_wide_after_q,
        observations,
    );
}

#[cfg(test)]
mod resize_ack_parser_tests {
    use super::{ResizeTraceChunk, TerminalSize, resize_ack_sequence};
    use kerminal_domain::identity::SessionId;

    /// 构造不含真实终端正文的 parser 单测 chunk，固定 reader sequence 以验证水位边界。
    ///
    /// @author kongweiguang
    fn chunk(sequence: u64, bytes: &[u8]) -> ResizeTraceChunk {
        ResizeTraceChunk {
            session_id: SessionId::new(),
            sequence,
            bytes: bytes.to_vec(),
        }
    }

    /// 验证 OSC marker 跨 reader chunk 时以包含 BEL 的末段 sequence 作为回执位置。
    ///
    /// @author kongweiguang
    #[test]
    fn resize_ack_parser_marker_split_across_chunks_uses_sequence_containing_terminator() {
        let chunks = [chunk(8, b"\x1b]777;nonce;33 "), chunk(9, b"59\x07")];
        let size = TerminalSize::new(59, 33, 0, 0).expect("valid test size");

        assert_eq!(resize_ack_sequence(&chunks, "nonce", size, 7), Ok(Some(9)));
    }

    /// 验证请求前的同尺寸旧 marker 不满足新请求，后续新 marker 才能匹配。
    ///
    /// @author kongweiguang
    #[test]
    fn resize_ack_parser_old_same_size_marker_is_ignored_until_new_sequence_arrives() {
        let old_marker = b"\x1b]777;nonce;33 59\x07";
        let old_only = [chunk(12, old_marker)];
        let old_and_new = [chunk(12, old_marker), chunk(13, old_marker)];
        let size = TerminalSize::new(59, 33, 0, 0).expect("valid test size");

        assert_eq!(resize_ack_sequence(&old_only, "nonce", size, 12), Ok(None));
        assert_eq!(
            resize_ack_sequence(&old_and_new, "nonce", size, 12),
            Ok(Some(13))
        );
    }

    /// 验证合法但尺寸不符的 OSC 回执不会被误认成本次 resize 的 ACK。
    ///
    /// @author kongweiguang
    #[test]
    fn resize_ack_parser_wrong_size_marker_does_not_match() {
        let chunks = [chunk(4, b"\x1b]777;nonce;33 120\x07")];
        let size = TerminalSize::new(59, 33, 0, 0).expect("valid test size");

        assert_eq!(resize_ack_sequence(&chunks, "nonce", size, 0), Ok(None));
    }

    /// 验证未闭合的 OSC marker 保持未确认，不 panic 或把部分尺寸当作成功。
    ///
    /// @author kongweiguang
    #[test]
    fn resize_ack_parser_truncated_marker_is_unconfirmed_without_panicking() {
        let chunks = [chunk(5, b"\x1b]777;nonce;33 59")];
        let size = TerminalSize::new(59, 33, 0, 0).expect("valid test size");

        assert_eq!(resize_ack_sequence(&chunks, "nonce", size, 0), Ok(None));
    }
}
