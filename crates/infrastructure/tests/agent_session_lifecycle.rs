//! Agent 用例与真实 local PTY 的纵向生命周期回归。
//!
//! @author kongweiguang

#[cfg(windows)]
use std::{ffi::OsString, os::windows::ffi::OsStringExt};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PreferencesService, ProfileService,
    agents::{
        AgentProviderSessionStatus, AgentService, AgentSessionService, AgentWorkspaceDocument,
        AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind, AgentWorkspaceRepository,
        AgentWorkspaceRevision, CreateAgentSessionRequest,
    },
    preferences::{
        AgentLauncherSettings, ConfigurationGuideStatus, ConfigurationRepository,
        CustomAgentDefinition, ProfileDraft, ProfileRepository, ProfileRevision,
        SettingsRepository, SettingsSnapshot, VersionedProfile, VersionedSettings,
    },
    sessions::{PlainTextSnapshotRequest, SessionFilter, SessionService},
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch, AgentSessionScope,
        AgentSessionStatus, AgentTargetBinding,
    },
    identity::AgentSessionId,
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState, TerminalProfile,
    },
};
#[cfg(windows)]
use kerminal_domain::{
    identity::SessionId,
    sessions::{SessionInfo, SessionLifecycle},
};
use kerminal_infrastructure::{
    agents::FileAgentWorkspaceRepository,
    local_pty::LocalPtySessionTransportFactory,
    platform::default_shell::SystemDefaultLocalShellProvider,
    sessions::{SessionManager, SessionRuntimeConfig},
};
use tokio::{
    runtime::Handle,
    sync::RwLock,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CUSTOM_AGENT_ID: &str = "11111111-1111-4111-8111-111111111111";

struct TestRoot(PathBuf);

impl TestRoot {
    /// 每次运行使用独立临时根，避免读写正式 `.kerminal` 或与并行测试冲突。
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kerminal-agent-lifecycle-{timestamp}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("isolated config root is created");
        Self(path)
    }

    /// 返回已创建的临时数据根，让测试适配器保持和产品相同的受管路径结构。
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 临时根归本测试独占，测试结束后只清理该 UUID 路径。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct MemoryConfigurationRepository {
    settings: RwLock<Option<SettingsSnapshot>>,
}

impl MemoryConfigurationRepository {
    /// 替换仅存在内存中的 settings，模拟外部编辑器修改所选 launcher。
    async fn replace_settings(&self, settings: &SettingsSnapshot) {
        *self.settings.write().await = Some(settings.clone());
    }
}

#[async_trait]
impl SettingsRepository for MemoryConfigurationRepository {
    /// 返回 typed launcher 设置，避免生命周期测试自己维护应用序列化格式。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let settings = self.settings.read().await.clone();
        let revision = settings
            .as_ref()
            .map(|_| ConfigRevision::from_sha256("agent-lifecycle-test".to_owned()));
        Ok(VersionedSettings {
            settings: settings.unwrap_or_default(),
            revision,
        })
    }

    /// 生命周期测试只读设置，不模拟用户保存或文件 codec。
    async fn save_settings(
        &self,
        _settings: SettingsSnapshot,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        Err(AppError::unsupported(
            "Agent lifecycle settings fake is read-only",
        ))
    }
}

#[async_trait]
impl ConfigurationRepository for MemoryConfigurationRepository {
    /// 测试设置始终可读写，且不会触碰正式配置目录或恢复状态。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 配置原文不属于 Agent 生命周期 fake 的职责，设置读取统一走 typed port。
    async fn read(&self, _path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported(
            "Agent lifecycle settings fake is typed-only",
        ))
    }

    /// 测试设置通过显式替换更新，拒绝伪造 PreferencesService 的持久化成功。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported(
            "Agent lifecycle test settings are read-only through the application port",
        ))
    }

    /// 测试仓储不播种配置指南，仅满足设置读取的恢复态契约。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

#[derive(Default)]
struct EmptyProfileRepository;

#[async_trait]
impl ProfileRepository for EmptyProfileRepository {
    /// 测试不定义显式 profile，Windows 自定义命令使用正式平台默认 shell provider。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }

    /// ProfileService 仅需列举默认目标；编辑查询接口在该测试中保持空。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }

    /// 测试未保存 profile，因此单项读取明确返回不存在。
    async fn get_profile(&self, _id: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }

    /// Lifecycle 测试不写 profile，避免把测试设置更新误当成 profile 持久化行为。
    async fn save_profile(
        &self,
        _draft: ProfileDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "test profile repository is read-only",
        ))
    }

    /// Lifecycle 测试不删除 profile，避免产生未覆盖的存储副作用。
    async fn delete_profile(
        &self,
        _id: &str,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "test profile repository is read-only",
        ))
    }

    /// Lifecycle 测试不切换默认 profile，保持测试 shell 选择确定。
    async fn set_default_profile(
        &self,
        _id: &str,
        _expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported(
            "test profile repository is read-only",
        ))
    }
}

/// 用真实子进程见证记录每次启动；先隔开 libtest 状态行，避免 PTY 软换行破坏就绪标记。
///
/// @author kongweiguang
#[test]
#[ignore = "launched as a child process by the Agent lifecycle test"]
fn fake_agent_cli_process() {
    let marker = std::env::args()
        .find(|argument| argument.starts_with("--agent-marker="))
        .unwrap_or_else(|| "--agent-marker=missing".to_owned());
    let witness_path = std::env::args()
        .find_map(|argument| {
            argument
                .strip_prefix("--agent-startup-witness=")
                .map(PathBuf::from)
        })
        .expect("the controlled CLI receives its isolated startup witness");
    let mut witness = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(witness_path)
        .expect("the controlled CLI can append its startup witness");
    writeln!(witness, "{}", std::process::id()).expect("the CLI records its actual process ID");
    witness
        .flush()
        .expect("the startup witness is flushed before readiness");
    drop(witness);
    println!("\r\nAGENT_CHILD_READY:{marker}");
    std::io::stdout()
        .flush()
        .expect("the controlled CLI flushes its readiness marker to the real PTY");
    std::thread::sleep(Duration::from_secs(90));
}

/// 独立验证真实 CLI 消费固定公开文本，不记录任意输入；阻塞 stdin 由有界通道隔离，精确单测试进程最多存活 10 分钟。
///
/// 输入行最多 256 字节、总量最多 64 KiB；读线程在截止后可随专用 libtest 进程结束，不能在超时路径等待它解锁 stdin。
/// @author kongweiguang
#[test]
#[ignore = "launched as a child process for native GPUI Agent input acceptance"]
fn fake_agent_cli_echo_process() {
    use std::{
        io::Read,
        sync::mpsc::{self, RecvTimeoutError},
    };

    const READY: &str = "KERMINAL_GPUI_AGENT_READY";
    const INPUT: &[u8] = b"KERMINAL_GPUI_AGENT_INPUT";
    const EXIT: &[u8] = b"KERMINAL_GPUI_AGENT_EXIT";
    const MAX_LINE_BYTES: usize = 256;
    const MAX_TOTAL_BYTES: usize = 64 * 1024;
    // 原生逐步快照与归档、联合退出需要充分时间，同时保留上限，防止验收 fixture 无限遗留。
    const MAX_LIFETIME: Duration = Duration::from_secs(10 * 60);

    enum StdinEvent {
        Input,
        Exit,
        Eof,
        Limit,
        Failed,
    }

    let marker = std::env::args()
        .find_map(|argument| argument.strip_prefix("--agent-marker=").map(str::to_owned))
        .expect("the echo CLI receives its fixed public readiness marker");
    assert_eq!(marker, READY, "the echo CLI only accepts its public marker");
    let witness_path = std::env::args()
        .find_map(|argument| {
            argument
                .strip_prefix("--agent-startup-witness=")
                .map(PathBuf::from)
        })
        .expect("the echo CLI receives its task-private startup witness");
    let mut witness = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(witness_path)
        .expect("the echo CLI can append its task-private witness");
    writeln!(witness, "{}", std::process::id()).expect("the echo CLI records its real PID");
    writeln!(witness, "{READY}").expect("the echo CLI records its public readiness marker");
    witness
        .flush()
        .expect("readiness witness is flushed before stdout");

    let (sender, events) = mpsc::sync_channel(2);
    let _reader = std::thread::spawn(move || {
        let emit_line = |line: &[u8]| {
            if line == INPUT {
                sender.send(StdinEvent::Input).is_ok()
            } else if line == EXIT {
                let _ = sender.send(StdinEvent::Exit);
                false
            } else {
                true
            }
        };
        let input = std::io::stdin();
        let mut input = input.lock();
        let mut buffer = [0_u8; 128];
        let mut line = Vec::with_capacity(MAX_LINE_BYTES);
        let mut discard_line = false;
        let mut total = 0;
        loop {
            let available = (MAX_TOTAL_BYTES - total).min(buffer.len());
            if available == 0 {
                let _ = sender.send(StdinEvent::Limit);
                return;
            }
            let count = match input.read(&mut buffer[..available]) {
                Ok(0) => {
                    if !discard_line && !emit_line(&line) {
                        return;
                    }
                    let _ = sender.send(StdinEvent::Eof);
                    return;
                }
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = sender.send(StdinEvent::Failed);
                    return;
                }
            };
            total += count;
            for byte in buffer[..count].iter().copied() {
                if matches!(byte, b'\r' | b'\n') {
                    if !discard_line && !emit_line(&line) {
                        return;
                    }
                    line.clear();
                    discard_line = false;
                } else if !discard_line {
                    if line.len() < MAX_LINE_BYTES {
                        line.push(byte);
                    } else {
                        line.clear();
                        discard_line = true;
                    }
                }
            }
        }
    });

    println!("\r\n{READY}");
    std::io::stdout()
        .flush()
        .expect("the echo CLI flushes readiness to its real PTY");
    let deadline = std::time::Instant::now() + MAX_LIFETIME;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let (marker, stop, failed) = match events.recv_timeout(remaining) {
            Ok(StdinEvent::Input) => ("KERMINAL_GPUI_AGENT_INPUT_ACK", false, false),
            Ok(StdinEvent::Exit) => ("KERMINAL_GPUI_AGENT_EXIT_ACK", true, false),
            Ok(StdinEvent::Eof) => ("KERMINAL_GPUI_AGENT_EOF", true, false),
            Ok(StdinEvent::Limit) => ("KERMINAL_GPUI_AGENT_INPUT_LIMIT", true, true),
            Ok(StdinEvent::Failed) | Err(RecvTimeoutError::Disconnected) => {
                ("KERMINAL_GPUI_AGENT_STDIN_ERROR", true, true)
            }
            Err(RecvTimeoutError::Timeout) => ("KERMINAL_GPUI_AGENT_TIMEOUT", true, false),
        };
        writeln!(witness, "{marker}")
            .expect("the echo CLI records only fixed public event markers");
        witness
            .flush()
            .expect("the echo CLI flushes the input confirmation before stdout");
        println!("\r\n{marker}");
        std::io::stdout()
            .flush()
            .expect("the echo CLI flushes its fixed event marker");
        assert!(
            !failed,
            "the echo CLI could not continue its bounded stdin read"
        );
        if stop {
            return;
        }
    }
}

/// Windows Custom 会把整串交给 PowerShell，故显式使用调用运算符；其他平台保持精确 argv 解析输入。
///
/// @author kongweiguang
fn custom_cli_fixture_command(executable: &Path, marker: &str, witness: &Path) -> String {
    #[cfg(windows)]
    let invocation = "& ";
    #[cfg(not(windows))]
    let invocation = "";
    format!(
        "{invocation}\"{}\" --exact fake_agent_cli_process --ignored --nocapture --test-threads=1 -- --agent-marker={marker} \"--agent-startup-witness={}\"",
        executable.display(),
        witness.display()
    )
}

/// 只接受一次真实启动的单个 PID，避免 registry 中未留下会话的额外进程被误判为没有启动。
///
/// @author kongweiguang
fn single_fixture_process_id(witness: &Path) -> Option<u32> {
    let contents = fs::read_to_string(witness).ok()?;
    let mut lines = contents.lines();
    let pid = lines.next()?.parse().ok()?;
    lines.next().is_none().then_some(pid)
}

/// 先结束 libtest 的活动状态行再写 marker，避免 80 列 PTY 将两者软换行后破坏整串匹配；原生 stdout 仍验证真实 PTY 输出。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
#[ignore = "launched as a child process by the Windows Agent lifecycle test"]
fn fake_agent_cli_exit_process() {
    let marker = std::env::args()
        .find_map(|argument| argument.strip_prefix("--agent-marker=").map(str::to_owned))
        .unwrap_or_else(|| "missing".to_owned());
    if let Some(witness_path) = std::env::var_os("KERMINAL_AGENT_LIFECYCLE_WITNESS") {
        fs::write(witness_path, format!("AGENT_EXIT_MARKER={marker}"))
            .expect("controlled Agent fixture witness is writable");
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(b"\r\n")
        .expect("controlled Agent CLI separates the marker from libtest status output");
    stdout
        .flush()
        .expect("controlled Agent CLI flushes the test-harness line boundary");
    stdout
        .write_all(format!("AGENT_EXIT_MARKER={marker}\r\n").as_bytes())
        .expect("controlled Agent CLI marker is written to native stdout");
    stdout
        .flush()
        .expect("controlled Agent CLI marker is flushed to native stdout");
}

/// 用隔离 settings 选择自定义 CLI，并复用真实 SessionManager/ConPTY 生命周期。
fn local_session_service() -> Arc<dyn SessionService> {
    Arc::new(SessionManager::new(
        Arc::new(LocalPtySessionTransportFactory::new()),
        Handle::current(),
        SessionRuntimeConfig::default(),
    ))
}

/// 构造一个经设置 schema 验证的 custom launcher 定义，不通过 shell 求值命令字符串。
/// 使用结构体更新语法一次构造目标快照，避免默认值与后续赋值分离。
///
/// @author kongweiguang
fn selected_custom_settings(command: String) -> SettingsSnapshot {
    SettingsSnapshot {
        agent_launcher: AgentLauncherSettings {
            selected_agent_key: format!("custom:{CUSTOM_AGENT_ID}"),
            custom_agents: vec![CustomAgentDefinition {
                id: CUSTOM_AGENT_ID.to_owned(),
                name: "Lifecycle Test CLI".to_owned(),
                command,
            }],
        },
        ..SettingsSnapshot::default()
    }
}

/// 等待本次 PTY 输出中出现唯一 marker，使用 engine 有界纯文本快照而不是读取文件旁路。
async fn wait_for_marker(
    sessions: &dyn SessionService,
    session_id: kerminal_domain::identity::SessionId,
    marker: &str,
) -> AppResult<bool> {
    timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = sessions
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 128,
                        max_bytes: Some(32 * 1024),
                    },
                )
                .await?;
            if snapshot.text.contains(marker) {
                return Ok::<bool, AppError>(true);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(Ok(false))
}

/// 等待指定真实 PTY 进入 Exited；有界轮询避免用固定延时掩盖进程退出竞态。
#[cfg(windows)]
async fn wait_for_terminal_exit(
    sessions: &dyn SessionService,
    session_id: &SessionId,
) -> AppResult<Option<SessionInfo>> {
    match timeout(Duration::from_secs(8), async {
        loop {
            let current = sessions.list(SessionFilter::default()).await?;
            if let Some(info) = current.into_iter().find(|info| {
                &info.session_id == session_id && info.lifecycle == SessionLifecycle::Exited
            }) {
                return Ok::<_, AppError>(Some(info));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(None),
    }
}

/// 仅保留受控 CLI marker、Rust test harness 固定状态和 PowerShell 错误类别，避免输出环境或路径。
///
/// @author kongweiguang
#[cfg(windows)]
fn controlled_cli_output_excerpt(snapshot: &str, marker: &str) -> String {
    let expected_marker = format!("AGENT_EXIT_MARKER={marker}");
    let expected_shell_return = format!("AGENT_POWERSHELL_RETURNED={marker}");
    let mut selected = Vec::new();
    for line in snapshot.lines() {
        let safe_line = if line.contains(&expected_marker) {
            Some(expected_marker.as_str())
        } else if line.contains(&expected_shell_return) {
            Some(expected_shell_return.as_str())
        } else if line.contains("running 1 test") {
            Some("running 1 test")
        } else if line.contains("fake_agent_cli_exit_process ... ok") {
            Some("fake_agent_cli_exit_process ... ok")
        } else if line.contains("fake_agent_cli_exit_process ... FAILED") {
            Some("fake_agent_cli_exit_process ... FAILED")
        } else if line.contains("test result: ok") {
            Some("test result: ok")
        } else if line.contains("test result: FAILED") {
            Some("test result: FAILED")
        } else if line.contains("ParserError") {
            Some("<PowerShell ParserError>")
        } else if line.contains("CommandNotFoundException") {
            Some("<PowerShell CommandNotFoundException>")
        } else if line.contains("NativeCommandError") {
            Some("<PowerShell NativeCommandError>")
        } else if line.contains("RuntimeException") {
            Some("<PowerShell RuntimeException>")
        } else {
            None
        };
        if let Some(safe_line) = safe_line {
            selected.push(safe_line);
            if selected.len() == 8 {
                break;
            }
        }
    }

    if selected.is_empty() {
        "<no controlled CLI or test-harness output in bounded snapshot>".to_owned()
    } else {
        selected.join(" | ")
    }
}

/// 在同一有界轮询中分别记录子测试 marker、PowerShell 返回 sentinel 和快照窗口元数据。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_lifecycle_output(
    sessions: &dyn SessionService,
    session_id: SessionId,
    marker: &str,
) -> AppResult<LifecycleOutputObservation> {
    let rust_marker = format!("AGENT_EXIT_MARKER={marker}");
    let powershell_return_marker = format!("AGENT_POWERSHELL_RETURNED={marker}");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut observation = LifecycleOutputObservation::default();
    loop {
        let snapshot = sessions
            .plaintext_snapshot(
                session_id.clone(),
                PlainTextSnapshotRequest {
                    include_scrollback: true,
                    max_lines: 128,
                    max_bytes: Some(32 * 1024),
                },
            )
            .await?;
        observation.rust_marker_seen |= snapshot.text.contains(&rust_marker);
        observation.powershell_return_seen |= snapshot.text.contains(&powershell_return_marker);
        observation.first_line = snapshot.first_line;
        observation.line_count = snapshot.line_count;
        observation.total_scrollback_lines = snapshot.total_scrollback_lines;
        observation.truncated = snapshot.truncated;
        observation.safe_excerpt = controlled_cli_output_excerpt(&snapshot.text, marker);

        if observation.rust_marker_seen && observation.powershell_return_seen {
            break;
        }
        if Instant::now() >= deadline {
            observation.timed_out = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(observation)
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct LifecycleOutputObservation {
    rust_marker_seen: bool,
    powershell_return_seen: bool,
    timed_out: bool,
    first_line: i64,
    line_count: usize,
    total_scrollback_lines: usize,
    truncated: bool,
    safe_excerpt: String,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum WindowsProcessLiveness {
    Running,
    Exited,
    #[default]
    Unknown,
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct WindowsProcessObservation {
    liveness: WindowsProcessLiveness,
    image_matches_launch: Option<bool>,
}

/// 只读探测真实 PTY child 的 PID、活性和映像身份，不记录命令行或绝对路径。
///
/// @author kongweiguang
#[cfg(windows)]
fn observe_windows_process(pid: Option<u32>, expected_image: &Path) -> WindowsProcessObservation {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    let Some(pid) = pid else {
        return WindowsProcessObservation::default();
    };
    // SAFETY: 仅申请有限查询权限，不操作目标进程；成功句柄在本函数末尾关闭。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let liveness = if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
            WindowsProcessLiveness::Exited
        } else {
            WindowsProcessLiveness::Unknown
        };
        return WindowsProcessObservation {
            liveness,
            image_matches_launch: None,
        };
    }

    let mut exit_code = 0;
    // SAFETY: 输出指针指向有效的栈变量，句柄来自上面的 OpenProcess。
    let exit_status_known = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    let liveness = if !exit_status_known {
        WindowsProcessLiveness::Unknown
    } else if exit_code == STILL_ACTIVE as u32 {
        WindowsProcessLiveness::Running
    } else {
        WindowsProcessLiveness::Exited
    };

    let mut image_buffer = vec![0_u16; 32_768];
    let mut image_length = image_buffer.len() as u32;
    // SAFETY: buffer 长度与 image_length 一致，查询使用同一只读进程句柄。
    let image_matches_launch = if unsafe {
        QueryFullProcessImageNameW(handle, 0, image_buffer.as_mut_ptr(), &mut image_length)
    } != 0
    {
        let observed_image =
            PathBuf::from(OsString::from_wide(&image_buffer[..image_length as usize]));
        windows_path_identity_matches(&observed_image, expected_image)
    } else {
        None
    };

    // SAFETY: 该有限权限句柄由本函数创建，且在离开前恰好关闭一次。
    let closed = unsafe { CloseHandle(handle) } != 0;
    WindowsProcessObservation {
        liveness: if closed {
            liveness
        } else {
            WindowsProcessLiveness::Unknown
        },
        image_matches_launch,
    }
}

/// 关闭 ACK 后仍有界回读原 PID；复用只读进程探针，不以 registry 移除替代真实进程结束。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_windows_process_exit(pid: Option<u32>, expected_image: &Path) -> bool {
    timeout(Duration::from_secs(3), async {
        loop {
            if observe_windows_process(pid, expected_image).liveness
                == WindowsProcessLiveness::Exited
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// 以文件系统解析后的 Windows 路径比较启动器身份，路径不可解析时保持未知而不猜测。
///
/// @author kongweiguang
#[cfg(windows)]
fn windows_path_identity_matches(actual: &Path, expected: &Path) -> Option<bool> {
    match (actual.canonicalize(), expected.canonicalize()) {
        (Ok(actual), Ok(expected)) => Some(
            actual
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.to_string_lossy()),
        ),
        _ => None,
    }
}

/// 新建 Custom 没有恢复声明入口；真实关闭后继续必须拒绝且不能另启进程，归档仍应保留历史。
///
/// @author kongweiguang
#[tokio::test]
async fn new_custom_cli_rejects_resume_without_spawning_and_archives() {
    let root = TestRoot::new();
    let isolated_root = root.path().to_path_buf();
    let unrelated_root = TestRoot::new();
    let unrelated_file = unrelated_root.path().join("outside-agent-root.txt");
    fs::write(&unrelated_file, b"unrelated workspace remains intact")
        .expect("independent workspace sentinel is created");
    let settings = Arc::new(MemoryConfigurationRepository::default());
    let executable = std::env::current_exe().expect("test executable path is available");
    let first_marker = "NEW_CUSTOM_CREATED";
    let witness = root.path().join("custom-cli-startups.txt");
    let first_command = custom_cli_fixture_command(&executable, first_marker, &witness);
    settings
        .replace_settings(&selected_custom_settings(first_command.clone()))
        .await;

    let sessions = local_session_service();
    let workspace: Arc<dyn AgentWorkspaceRepository> =
        Arc::new(FileAgentWorkspaceRepository::new(root.path().to_path_buf()));
    let agents = AgentSessionService::new(
        Arc::clone(&sessions),
        Arc::clone(&workspace),
        Arc::new(PreferencesService::new(settings.clone())),
    )
    .with_profile_service(Arc::new(ProfileService::new(
        Arc::new(EmptyProfileRepository),
        Arc::new(SystemDefaultLocalShellProvider::new()),
    )));
    agents
        .set_mcp_endpoint("http://127.0.0.1:40137/mcp")
        .await
        .expect("new Custom workspace receives an isolated endpoint value, not a live listener");

    let result = timeout(Duration::from_secs(30), async {
        let created = agents
            .create(
                CreateAgentSessionRequest {
                    title: Some("Lifecycle Test".to_owned()),
                    scope: AgentSessionScope::Global,
                    target: AgentTargetBinding::unbound(0),
                    launcher_key: None,
                    initial_palette: None,
                },
                CancellationToken::new(),
            )
            .await?;
        let session_id = created.session.agent_session_id.clone();
        #[cfg(windows)]
        let launched_from_selected = {
            let launch = &created.session.launch;
            let shell = launch.shell.to_ascii_lowercase();
            (shell.ends_with("powershell.exe") || shell.ends_with("pwsh.exe"))
                && launch
                    .args
                    .ends_with(&["-Command".to_owned(), first_command.clone()])
        };
        #[cfg(not(windows))]
        let launched_from_selected = {
            created.session.launch.shell == executable.to_string_lossy()
                && created.session.launch.args
                    == vec![
                        "--exact".to_owned(),
                        "fake_agent_cli_process".to_owned(),
                        "--ignored".to_owned(),
                        "--nocapture".to_owned(),
                        "--test-threads=1".to_owned(),
                        "--".to_owned(),
                        format!("--agent-marker={first_marker}"),
                        format!("--agent-startup-witness={}", witness.display()),
                    ]
        };
        let first_terminal = created.terminal.as_ref().expect("CLI terminal is created");
        let first_output = wait_for_marker(
            sessions.as_ref(),
            first_terminal.session_id.clone(),
            first_marker,
        )
        .await?;
        let child_pid = single_fixture_process_id(&witness);
        #[cfg(windows)]
        let native_pid = sessions
            .runtime_metadata(first_terminal.session_id.clone())
            .await?
            .process_id;
        let provider = workspace
            .read_provider_session(session_id.clone(), CancellationToken::new())
            .await?;
        let no_resume_capability = matches!(
            provider.status,
            AgentProviderSessionStatus::Available(ref provider)
                if provider.provider == AgentProvider::Custom
                    && !provider.resume_supported
                    && provider.resume_command.is_none()
        );
        sessions
            .close_until(
                first_terminal.session_id.clone(),
                Instant::now() + Duration::from_secs(5),
            )
            .await?;

        let closed_registry_is_empty = sessions.list(SessionFilter::default()).await?.is_empty();
        let changed_command = custom_cli_fixture_command(&executable, "CHANGED_SETTING", &witness);
        settings
            .replace_settings(&selected_custom_settings(changed_command))
            .await;

        let resume = agents
            .resume(session_id.clone(), CancellationToken::new())
            .await;
        let resume_is_unsupported = matches!(resume, Err(AppError::Unsupported { .. }));
        let no_new_owner = sessions.list(SessionFilter::default()).await?.is_empty();
        let no_new_process =
            child_pid.is_some() && single_fixture_process_id(&witness) == child_pid;
        let unchanged_provider = workspace
            .read_provider_session(session_id.clone(), CancellationToken::new())
            .await?
            == provider;
        let archived = agents
            .archive(session_id.clone(), CancellationToken::new())
            .await?;
        let persisted = workspace
            .read_session(session_id, CancellationToken::new())
            .await?;
        let archived_registry_is_empty = sessions.list(SessionFilter::default()).await?.is_empty();
        #[cfg(windows)]
        let processes_exited = Some(
            wait_for_windows_process_exit(native_pid, Path::new(&created.session.launch.shell))
                .await
                && wait_for_windows_process_exit(child_pid, &executable).await,
        );
        #[cfg(not(windows))]
        let processes_exited: Option<bool> = None;
        Ok::<_, AppError>((
            launched_from_selected,
            first_output,
            no_resume_capability && unchanged_provider,
            closed_registry_is_empty && resume_is_unsupported && no_new_owner && no_new_process,
            archived.session.status == AgentSessionStatus::Archived
                && persisted.session.status == AgentSessionStatus::Archived
                && archived.terminal.is_none()
                && archived_registry_is_empty,
            processes_exited,
        ))
    })
    .await;
    let shutdown = agents
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await;

    drop(agents);
    drop(workspace);
    drop(root);
    let evidence = result
        .expect("new Custom lifecycle finishes within its watchdog")
        .expect("new Custom lifecycle operations succeed");
    assert!(evidence.0, "new session records the selected CLI snapshot");
    assert!(evidence.1, "created CLI emits its selected command marker");
    assert!(
        evidence.2,
        "new Custom keeps its original explicit unsupported resume metadata"
    );
    assert!(
        evidence.3,
        "unsupported resume neither starts a process nor registers another owner"
    );
    assert!(
        evidence.4,
        "archive persists history without leaving a terminal in the registry"
    );
    #[cfg(windows)]
    assert_eq!(
        evidence.5,
        Some(true),
        "the Windows native shell and CLI child are gone"
    );
    shutdown.expect("Agent shutdown completes after archive");
    assert!(
        !isolated_root.exists(),
        "only this test's isolated workspace is removed"
    );
    assert_eq!(
        fs::read(&unrelated_file).unwrap(),
        b"unrelated workspace remains intact"
    );
}

/// 只为导入兼容样本一次性播种旧版允许显式 provider 的 schema 1 文档，不修改新建 Custom 的能力。
///
/// 冻结 v0.3.45 的 AgentSessionCreateRequest.provider 可保存恢复命令；该样本不代表旧 UI 有此配置入口。
/// @author kongweiguang
async fn seed_legacy_custom_session(
    workspace: &dyn AgentWorkspaceRepository,
    executable: &Path,
    resume_command: String,
) -> AppResult<AgentSessionId> {
    let id = AgentSessionId::new();
    let session_root = workspace
        .create_session_root(id.clone(), CancellationToken::new())
        .await?;
    let session = AgentSession::restore(
        1,
        id.clone(),
        AgentProvider::Custom,
        None,
        "Imported Custom history".to_owned(),
        "2026-10-16T00:00:00Z".to_owned(),
        "2026-10-16T00:00:00Z".to_owned(),
        AgentSessionStatus::Stale,
        workspace.workspace_root(),
        session_root.clone(),
        AgentSessionLaunch {
            command_label: "Original Custom create command".to_owned(),
            shell: executable.to_string_lossy().into_owned(),
            args: vec!["--original-create-command-must-not-run".to_owned()],
            cwd: session_root,
        },
        Some(AgentSessionScope::Global),
        Some(AgentTargetBinding::unbound(0)),
    )
    .expect("independent legacy Custom sample satisfies schema 1 invariants");
    let provider = AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: Some("legacy-custom-fixture".to_owned()),
        resume_command: Some(resume_command),
        resume_supported: true,
        last_resume_at: None,
    };
    workspace
        .write_documents(
            id.clone(),
            vec![
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Session,
                    content: AgentWorkspaceDocumentContent::Session(Box::new(session)),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Provider,
                    content: AgentWorkspaceDocumentContent::Provider(provider),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
            ],
            CancellationToken::new(),
        )
        .await?;
    Ok(id)
}

/// 只恢复已导入的 Custom 显式命令，证明真实 PTY 使用保存的恢复参数而非旧创建命令或当前设置。
///
/// @author kongweiguang
#[tokio::test]
async fn legacy_custom_cli_resumes_saved_command_and_archives() {
    let root = TestRoot::new();
    let isolated_root = root.path().to_path_buf();
    let unrelated_root = TestRoot::new();
    let unrelated_file = unrelated_root.path().join("outside-agent-root.txt");
    fs::write(&unrelated_file, b"unrelated workspace remains intact")
        .expect("independent workspace sentinel is created");
    let executable = std::env::current_exe().expect("test executable path is available");
    let witness = root.path().join("legacy-custom-cli-startups.txt");
    let marker = "LEGACY_RESUME";
    let resume_command = custom_cli_fixture_command(&executable, marker, &witness);
    let settings = Arc::new(MemoryConfigurationRepository::default());
    settings
        .replace_settings(&selected_custom_settings(custom_cli_fixture_command(
            &executable,
            "CHANGED_SETTING",
            &witness,
        )))
        .await;
    let sessions = local_session_service();
    let workspace: Arc<dyn AgentWorkspaceRepository> =
        Arc::new(FileAgentWorkspaceRepository::new(root.path().to_path_buf()));
    let id = seed_legacy_custom_session(workspace.as_ref(), &executable, resume_command.clone())
        .await
        .expect("independent legacy Custom history is seeded before any process starts");
    let agents = AgentSessionService::new(
        Arc::clone(&sessions),
        Arc::clone(&workspace),
        Arc::new(PreferencesService::new(settings)),
    )
    .with_profile_service(Arc::new(ProfileService::new(
        Arc::new(EmptyProfileRepository),
        Arc::new(SystemDefaultLocalShellProvider::new()),
    )));
    agents
        .set_mcp_endpoint("http://127.0.0.1:40137/mcp")
        .await
        .expect("legacy workspace receives an isolated endpoint value, not a live listener");

    let result = timeout(Duration::from_secs(30), async {
        let resumed = agents.resume(id.clone(), CancellationToken::new()).await?;
        let terminal = resumed
            .terminal
            .as_ref()
            .expect("legacy CLI gets a real PTY");
        #[cfg(windows)]
        let exact_resume_command = resumed
            .session
            .launch
            .args
            .ends_with(&["-Command".to_owned(), resume_command.clone()]);
        #[cfg(not(windows))]
        let exact_resume_command = resumed.session.launch.args.last() == Some(&resume_command);
        let output =
            wait_for_marker(sessions.as_ref(), terminal.session_id.clone(), marker).await?;
        let child_pid = single_fixture_process_id(&witness);
        #[cfg(windows)]
        let native_pid = sessions
            .runtime_metadata(terminal.session_id.clone())
            .await?
            .process_id;
        let provider = workspace
            .read_provider_session(id.clone(), CancellationToken::new())
            .await?;
        let provider_updated = matches!(
            provider.status,
            AgentProviderSessionStatus::Available(ref provider)
                if provider.resume_supported
                    && provider.resume_command.as_deref() == Some(resume_command.as_str())
                    && provider.provider_session_id.as_deref() == Some("legacy-custom-fixture")
                    && provider.last_resume_at.is_some()
        );
        let live = sessions.list(SessionFilter::default()).await?;
        let one_owner = live.len() == 1 && live[0].session_id == terminal.session_id;
        let archived = agents.archive(id.clone(), CancellationToken::new()).await?;
        let history = workspace.read_session(id, CancellationToken::new()).await?;
        let registry_empty = sessions.list(SessionFilter::default()).await?.is_empty();
        #[cfg(windows)]
        let processes_exited = Some(
            wait_for_windows_process_exit(native_pid, Path::new(&resumed.session.launch.shell))
                .await
                && wait_for_windows_process_exit(child_pid, &executable).await,
        );
        #[cfg(not(windows))]
        let processes_exited: Option<bool> = None;
        Ok::<_, AppError>((
            exact_resume_command && output && child_pid.is_some(),
            provider_updated && resumed.session.status == AgentSessionStatus::Active && one_owner,
            archived.session.status == AgentSessionStatus::Archived
                && history.session.status == AgentSessionStatus::Archived
                && archived.terminal.is_none()
                && registry_empty,
            processes_exited,
        ))
    })
    .await;
    let shutdown = agents
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await;
    drop(agents);
    drop(workspace);
    drop(root);

    let evidence = result
        .expect("legacy Custom lifecycle finishes within its watchdog")
        .expect("legacy Custom resume and archive operations succeed");
    assert!(
        evidence.0,
        "only the explicit saved resume command produces the real CLI marker"
    );
    assert!(
        evidence.1,
        "resume updates the legacy provider and owns exactly one terminal"
    );
    assert!(
        evidence.2,
        "archive keeps imported history and retires its terminal"
    );
    #[cfg(windows)]
    assert_eq!(
        evidence.3,
        Some(true),
        "archive reaps both the Windows native shell and controlled CLI child"
    );
    shutdown.expect("legacy Custom shutdown completes after archive");
    assert!(
        !isolated_root.exists(),
        "only this test's isolated workspace is removed"
    );
    assert_eq!(
        fs::read(&unrelated_file).unwrap(),
        b"unrelated workspace remains intact"
    );
}

/// Windows Custom CLI 退出后必须关闭真实 child；受限诊断用于区分 launcher、helper 与 PTY 输出阶段。
/// 短命 CLI 允许探测时已退出；若 PID 仍活跃，必须核对映像，避免把其它进程当作本次原生 child。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn windows_custom_cli_exit_marks_agent_stale_and_rejects_terminal_write() {
    let root = TestRoot::new();
    let isolated_root = root.path().to_path_buf();
    let settings = Arc::new(MemoryConfigurationRepository::default());
    let executable = std::env::current_exe().expect("test executable path is available");
    let marker = format!("AGENT_SHORT_EXIT_{}", Uuid::new_v4());
    let powershell_return_marker = format!("AGENT_POWERSHELL_RETURNED={marker}");
    let witness_path = root.path().join("custom-cli-witness.txt");
    let witness_path_for_powershell = witness_path.to_string_lossy().replace('\'', "''");
    let command = format!(
        "$env:KERMINAL_AGENT_LIFECYCLE_WITNESS = '{}'; & \"{}\" --exact fake_agent_cli_exit_process --ignored --nocapture --test-threads=1 -- --agent-marker={marker}; Write-Output '{powershell_return_marker}'",
        witness_path_for_powershell,
        executable.display()
    );
    settings
        .replace_settings(&selected_custom_settings(command.clone()))
        .await;

    let sessions = local_session_service();
    let profiles = Arc::new(ProfileService::new(
        Arc::new(EmptyProfileRepository),
        Arc::new(SystemDefaultLocalShellProvider::new()),
    ));
    let default_shell = profiles
        .default_local_session_target()
        .await
        .expect("the isolated Windows test resolves its normal local PowerShell target");
    let expected_shell_path = PathBuf::from(&default_shell.program);
    assert!(
        expected_shell_path.is_absolute(),
        "the Windows lifecycle test requires the absolute PowerShell executable selected by the application profile"
    );
    assert!(
        expected_shell_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("powershell.exe")),
        "the isolated lifecycle test uses the supported Windows PowerShell default shell"
    );
    let expected_shell_path = expected_shell_path
        .canonicalize()
        .expect("the selected PowerShell executable resolves to a concrete file");
    let workspace: Arc<dyn AgentWorkspaceRepository> =
        Arc::new(FileAgentWorkspaceRepository::new(root.path().to_path_buf()));
    let agents = AgentSessionService::new(
        Arc::clone(&sessions),
        workspace,
        Arc::new(PreferencesService::new(settings)),
    )
    .with_profile_service(Arc::clone(&profiles));
    agents
        .set_mcp_endpoint("http://127.0.0.1:40137/mcp")
        .await
        .expect("agent workspace receives the native MCP endpoint");

    let result = async {
        let created = agents
            .create(
                CreateAgentSessionRequest {
                    title: Some("Short-lived Custom CLI".to_owned()),
                    scope: AgentSessionScope::Global,
                    target: AgentTargetBinding::unbound(0),
                    launcher_key: None,
                    initial_palette: None,
                },
                CancellationToken::new(),
            )
            .await?;
        let terminal = created
            .terminal
            .as_ref()
            .ok_or_else(|| AppError::invalid("Custom Agent did not create a terminal"))?;
        let session_id = terminal.session_id.clone();
        let shell_name = created.session.launch.shell.to_ascii_lowercase();
        let launch = &created.session.launch;
        let powershell_selected = shell_name.ends_with("powershell.exe")
            && Path::new(&launch.shell).is_absolute()
            && windows_path_identity_matches(Path::new(&launch.shell), &expected_shell_path)
                == Some(true);
        let command_arg_matches = launch
            .args
            .iter()
            .position(|argument| argument == "-Command")
            .and_then(|index| launch.args.get(index + 1))
            .is_some_and(|argument| argument == &command);
        let launched_target_matches = match &terminal.target {
            kerminal_domain::sessions::SessionTarget::Local(local) => {
                local.program.to_string_lossy().as_ref() == launch.shell.as_str()
                    && local.args.len() == launch.args.len()
                    && local
                        .args
                        .iter()
                        .zip(&launch.args)
                        .all(|(actual, expected)| {
                            actual.to_string_lossy().as_ref() == expected.as_str()
                        })
            }
            _ => false,
        };
        let runtime_metadata = sessions.runtime_metadata(session_id.clone()).await?;
        let process_after_create =
            observe_windows_process(runtime_metadata.process_id, Path::new(&launch.shell));
        let marker_output =
            wait_for_lifecycle_output(sessions.as_ref(), session_id.clone(), &marker).await?;
        let process_after_output_poll =
            observe_windows_process(runtime_metadata.process_id, Path::new(&launch.shell));
        let exited = wait_for_terminal_exit(sessions.as_ref(), &session_id).await?;
        let process_after_exit_wait =
            observe_windows_process(runtime_metadata.process_id, Path::new(&launch.shell));
        let refreshed_agents = AgentService::list(&agents, CancellationToken::new()).await?;
        let refreshed = refreshed_agents
            .iter()
            .find(|view| view.session.agent_session_id == created.session.agent_session_id);
        let stale = refreshed.is_some_and(|view| view.session.status == AgentSessionStatus::Stale);
        let last_terminal_info = refreshed
            .and_then(|view| view.terminal.as_ref())
            .or(exited.as_ref());
        let last_terminal_lifecycle = last_terminal_info.map(|info| info.lifecycle);
        let stale_terminal_exited = refreshed
            .and_then(|view| view.terminal.as_ref())
            .is_some_and(|info| info.lifecycle == SessionLifecycle::Exited);

        let rejected_text = format!("Write-Output 'RESIDUAL_SHELL_RECEIVED_{marker}'\r\n");
        let write_rejected = sessions
            .write(session_id.clone(), rejected_text.as_bytes().to_vec())
            .await
            .is_err();
        let after_write = sessions
            .plaintext_snapshot(
                session_id,
                PlainTextSnapshotRequest {
                    include_scrollback: true,
                    max_lines: 128,
                    max_bytes: Some(32 * 1024),
                },
            )
            .await?;
        let witness_matches = fs::read_to_string(&witness_path)
            .is_ok_and(|contents| contents == format!("AGENT_EXIT_MARKER={marker}"));
        let pty_output_excerpt = controlled_cli_output_excerpt(&after_write.text, &marker);

        Ok::<_, AppError>(WindowsLifecycleEvidence {
            powershell_selected,
            command_arg_matches,
            launched_target_matches,
            output: marker_output,
            process_after_create,
            process_after_output_poll,
            process_after_exit_wait,
            witness_matches,
            terminal_exited: last_terminal_lifecycle == Some(SessionLifecycle::Exited),
            last_terminal_lifecycle,
            exit_code: last_terminal_info
                .and_then(|info| info.exit_status.as_ref().and_then(|status| status.code)),
            exit_success: last_terminal_info
                .and_then(|info| info.exit_status.as_ref().map(|status| status.success)),
            stale,
            stale_terminal_exited,
            write_rejected,
            rejected_text_absent: !after_write
                .text
                .contains(&format!("RESIDUAL_SHELL_RECEIVED_{marker}")),
            pty_output_excerpt,
        })
    }
    .await;
    let shutdown = agents
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await;
    let operations_succeeded = result.is_ok();
    let evidence = result.ok();
    drop(agents);
    drop(root);
    let isolated_root_removed = !isolated_root.exists();

    assert!(shutdown.is_ok(), "Agent shutdown completes after CLI exit");
    assert!(
        isolated_root_removed,
        "isolated Agent workspace fixture is removed"
    );
    assert!(
        operations_succeeded,
        "bounded Custom Agent lifecycle operations complete"
    );
    let evidence = evidence.expect("successful lifecycle operations return diagnostic evidence");

    assert!(
        evidence.powershell_selected,
        "the test uses the actual PowerShell Custom launch path"
    );
    assert!(
        evidence.command_arg_matches && evidence.launched_target_matches,
        "the persisted launch command and actual PTY target agree"
    );
    assert!(
        [
            &evidence.process_after_create,
            &evidence.process_after_output_poll,
            &evidence.process_after_exit_wait,
        ]
        .into_iter()
        .all(|process| {
            process.liveness != WindowsProcessLiveness::Running
                || process.image_matches_launch == Some(true)
        }),
        "any observed running native child must match the launched shell image"
    );
    assert!(
        evidence.output.rust_marker_seen && evidence.output.powershell_return_seen,
        "the Rust child marker and PowerShell post-return sentinel must both reach the PTY; rust_marker={}, powershell_return={}, output_poll_timed_out={}, witness_matches={}, terminal_exited={}, last_lifecycle={:?}, last_exit_code={:?}, exit_success={:?}, snapshot[first_line={}, line_count={}, total_scrollback_lines={}, truncated={}], process_after_create={:?}, process_after_output_poll={:?}, process_after_exit_wait={:?}, PTY excerpt at output poll: {}, PTY excerpt after lifecycle checks: {}",
        evidence.output.rust_marker_seen,
        evidence.output.powershell_return_seen,
        evidence.output.timed_out,
        evidence.witness_matches,
        evidence.terminal_exited,
        evidence.last_terminal_lifecycle,
        evidence.exit_code,
        evidence.exit_success,
        evidence.output.first_line,
        evidence.output.line_count,
        evidence.output.total_scrollback_lines,
        evidence.output.truncated,
        evidence.process_after_create,
        evidence.process_after_output_poll,
        evidence.process_after_exit_wait,
        evidence.output.safe_excerpt,
        evidence.pty_output_excerpt
    );
    assert!(
        evidence.witness_matches,
        "the controlled CLI reached its fixture-only witness path"
    );
    // Exited 只会在 reader EOF 后的 wait_exit 成功时写入；本地 PTY 的 wait_exit
    // 等待 spawn_child_monitor 对同一个 portable-pty Child 调用 wait() 的结果。
    assert!(
        evidence.terminal_exited && evidence.exit_code.is_some(),
        "PTY records the native child wait result; last_lifecycle={:?}, last_exit_code={:?}, process_after_exit_wait={:?}",
        evidence.last_terminal_lifecycle,
        evidence.exit_code,
        evidence.process_after_exit_wait
    );
    assert!(
        evidence.stale,
        "refresh marks the exited Custom Agent stale"
    );
    assert!(
        evidence.stale_terminal_exited,
        "stale projection retains the matching exited terminal"
    );
    assert!(
        evidence.write_rejected,
        "writes to the exited terminal are rejected"
    );
    assert!(
        evidence.rejected_text_absent,
        "rejected text never reaches terminal output"
    );
}

#[cfg(windows)]
struct WindowsLifecycleEvidence {
    powershell_selected: bool,
    command_arg_matches: bool,
    launched_target_matches: bool,
    output: LifecycleOutputObservation,
    process_after_create: WindowsProcessObservation,
    process_after_output_poll: WindowsProcessObservation,
    process_after_exit_wait: WindowsProcessObservation,
    witness_matches: bool,
    terminal_exited: bool,
    last_terminal_lifecycle: Option<SessionLifecycle>,
    exit_code: Option<i32>,
    exit_success: Option<bool>,
    stale: bool,
    stale_terminal_exited: bool,
    write_rejected: bool,
    rejected_text_absent: bool,
    pty_output_excerpt: String,
}
