//! 用真实 Unix PTY 验证 SSH Zsh/Fish 启动命令的配置、工作目录和退出闭环。
//!
//! 所有 HOME、配置目录、临时目录与 profile 标记均限制在单个 TempDir；终端字节只保留在
//! 有界内存中，不输出或保存用户 profile、命令回显或实际路径。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use kerminal_application::connections::RemoteShellKind;
use kerminal_domain::connections::RemoteShellDialect;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::remote_interactive_startup;
use crate::process_tree::ProcessTreeGuard;

const TEST_DEADLINE: Duration = Duration::from_secs(15);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const READER_CHUNK_BYTES: usize = 4096;
const OSC_A: &[u8] = b"\x1b]133;A\x07";
const OSC_B: &[u8] = b"\x1b]133;B\x07";
const OSC_C: &[u8] = b"\x1b]133;C\x07";
const OSC_D_ZERO: &[u8] = b"\x1b]133;D;0\x07";
const CWD_OK_TOKEN: &str = "KERM_CWD_OK_91F2";
const CWD_WRONG_TOKEN: &str = "KERM_CWD_WRONG_61D0";

/// 找到 PATH 中真实安装的 shell；缺少该 shell 时由各用例明确报告跳过能力。
///
/// @author kongweiguang
fn find_shell(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// 用安装中的 Zsh 执行生产启动命令，验证 login 配置链与受控 cwd。
///
/// @author kongweiguang
#[test]
fn zsh_remote_startup_runs_login_profiles_and_cleans_owned_directory() {
    let Some(shell) = find_shell("zsh") else {
        println!("SKIPPED: zsh executable is unavailable on PATH");
        return;
    };
    run_remote_startup_case(RemoteShellKind::Zsh, RemoteShellDialect::Posix, shell)
        .unwrap_or_else(|reason| panic!("real Zsh PTY startup failed: {reason}"));
}

/// 用安装中的 Fish 执行生产启动命令，验证交互 config、init hook 与受控 cwd。
///
/// @author kongweiguang
#[test]
fn fish_remote_startup_runs_interactive_config_and_cleans_owned_directory() {
    let Some(shell) = find_shell("fish") else {
        println!("SKIPPED: fish executable is unavailable on PATH");
        return;
    };
    run_remote_startup_case(RemoteShellKind::Fish, RemoteShellDialect::Fish, shell)
        .unwrap_or_else(|reason| panic!("real Fish PTY startup failed: {reason}"));
}

/// 用隔离且含 URI 保留字符的真实目录验证启动及 OSC7；fixture 不触碰用户文件或配置。
///
/// @author kongweiguang
fn run_remote_startup_case(
    kind: RemoteShellKind,
    dialect: RemoteShellDialect,
    executable: PathBuf,
) -> Result<(), &'static str> {
    let fixture = tempfile::tempdir().map_err(|_| "private fixture could not be created")?;
    let home = fixture.path().join("private home");
    let user_config = fixture.path().join("private user config");
    let xdg_config = fixture.path().join("private xdg config");
    let fish_config = xdg_config.join("fish");
    let trace = fixture.path().join("profile trace");
    let private_tmp = fixture.path().join("private tmp");
    let working_directory = fixture.path().join("work 中文 #% dir's here");
    for directory in [
        home.as_path(),
        user_config.as_path(),
        fish_config.as_path(),
        trace.as_path(),
        private_tmp.as_path(),
        working_directory.as_path(),
    ] {
        fs::create_dir_all(directory)
            .map_err(|_| "private fixture directory could not be created")?;
    }

    let sibling_sentinel = private_tmp.join("sibling-sentinel");
    fs::write(&sibling_sentinel, b"preserve-this-sibling")
        .map_err(|_| "private sibling sentinel could not be created")?;
    write_private_profiles(kind, &home, &user_config, &fish_config)?;

    let working_directory_text = working_directory
        .to_str()
        .ok_or("private working directory is not valid UTF-8")?;
    let startup = remote_interactive_startup(kind, dialect, Some(working_directory_text))
        .map_err(|_| "production remote startup builder rejected the fixture")?
        .ok_or("recognized shell did not receive a managed startup")?;
    if !startup.shell_integration_enabled {
        return Err("recognized shell startup was not marked as prepared");
    }

    let mut pty = PtyRun::spawn(
        executable,
        &startup.exec_command,
        &home,
        &user_config,
        &xdg_config,
        &trace,
        &private_tmp,
        &working_directory,
    )?;
    let deadline = Instant::now() + TEST_DEADLINE;

    let result = (|| {
        pty.wait_for_initial_prompt(deadline, fixture_prompt(kind))?;
        let startup_directories = wrapper_directories(&private_tmp)
            .map_err(|_| "private wrapper directory could not be inspected")?;
        if startup_directories.len() != 1 {
            return Err("startup did not create exactly one owned wrapper directory");
        }
        let owned_startup_directory = startup_directories[0].clone();

        pty.write_line(cwd_check_command(kind))?;
        pty.wait_for_cwd_command_cycle(deadline)?;
        pty.write_line("exit 0")?;
        pty.finish_natural_exit(deadline)?;

        if owned_startup_directory.exists()
            || !wrapper_directories(&private_tmp)
                .map_err(|_| "private wrapper directory could not be inspected")?
                .is_empty()
        {
            return Err("startup left its private wrapper directory behind");
        }
        if fs::read(&sibling_sentinel).ok().as_deref() != Some(b"preserve-this-sibling") {
            return Err("startup changed its sibling sentinel");
        }
        if !profile_load_counts_match(kind, &home, &user_config, &fish_config, &trace) {
            return Err("private user profile load count or ZDOTDIR/XDG selection was incorrect");
        }
        if !osc_prompt_cycles_are_ordered(&pty.output) {
            return Err("expected OSC 133 prompt and command cycle was not observed");
        }
        let directory_marker = osc7_directory_marker(working_directory_text);
        if marker_count(&pty.output, &directory_marker) < 2 {
            return Err("prompt did not report the correctly URI-encoded working directory");
        }
        if has_bootstrap_echo(&pty.output) {
            return Err("bootstrap source or octal-escaped script appeared in PTY output");
        }
        Ok(())
    })();

    if result.is_err() {
        pty.abort_owned_process_group();
    }
    result
}

/// 测试独立按 UTF8 字节构造最窄 file URI，避免测试把生产 hook 的未编码输出也当作正确。
///
/// @author kongweiguang
fn osc7_directory_marker(path: &str) -> Vec<u8> {
    use std::fmt::Write;

    let mut marker = String::from("\x1b]7;file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/._~-".contains(&byte) {
            marker.push(char::from(byte));
        } else {
            write!(&mut marker, "%{byte:02X}").expect("write to String");
        }
    }
    marker.push('\x07');
    marker.into_bytes()
}

/// 写入只会被测试子 shell 读取的哨兵配置；HOME 中的 decoy 可发现错误目录回退。
///
/// @author kongweiguang
fn write_private_profiles(
    kind: RemoteShellKind,
    home: &Path,
    user_config: &Path,
    fish_config: &Path,
) -> Result<(), &'static str> {
    match kind {
        RemoteShellKind::Zsh => {
            fs::write(
                user_config.join(".zshenv"),
                "printf x >> \"$KERMINAL_TRACE_DIR/zshenv\"\n",
            )
            .map_err(|_| "private Zsh env profile could not be created")?;
            fs::write(
                user_config.join(".zprofile"),
                "[[ -o login ]] && printf x >> \"$KERMINAL_TRACE_DIR/zprofile\"\n",
            )
            .map_err(|_| "private Zsh login profile could not be created")?;
            fs::write(
                user_config.join(".zshrc"),
                "[[ -o interactive ]] && printf x >> \"$KERMINAL_TRACE_DIR/zshrc\"\nPROMPT='ZSH_FIXTURE> '\n",
            )
            .map_err(|_| "private Zsh rc could not be created")?;
            fs::write(
                user_config.join(".zlogin"),
                "[[ -o login ]] && printf x >> \"$KERMINAL_TRACE_DIR/zlogin\"\n",
            )
            .map_err(|_| "private Zsh login rc could not be created")?;
            fs::write(
                user_config.join(".zlogout"),
                "printf x >> \"$KERMINAL_TRACE_DIR/zlogout\"\n",
            )
            .map_err(|_| "private Zsh logout rc could not be created")?;
            fs::write(
                home.join(".zshenv"),
                "printf x >> \"$KERMINAL_TRACE_DIR/wrong-home-zshenv\"\n",
            )
            .map_err(|_| "private HOME decoy could not be created")?;
        }
        RemoteShellKind::Fish => {
            fs::write(
                fish_config.join("config.fish"),
                "if status is-login\n    printf x >> \"$KERMINAL_TRACE_DIR/fish-login-config\"\nend\nif status is-interactive\n    printf x >> \"$KERMINAL_TRACE_DIR/fish-interactive-config\"\nend\nfunction fish_prompt\n    printf 'FISH_FIXTURE> '\nend\n",
            )
            .map_err(|_| "private Fish config could not be created")?;
            let home_fish_config = home.join(".config/fish");
            fs::create_dir_all(&home_fish_config)
                .map_err(|_| "private HOME Fish decoy directory could not be created")?;
            fs::write(
                home_fish_config.join("config.fish"),
                "if status is-interactive\n    printf x >> \"$KERMINAL_TRACE_DIR/wrong-home-fish\"\nend\n",
            )
            .map_err(|_| "private HOME Fish decoy could not be created")?;
        }
        _ => return Err("PTY fixture only accepts Zsh or Fish"),
    }
    Ok(())
}

/// 通过不会包含私有路径的固定命令验证 shell 最终位于带空格和单引号的目标目录。
///
/// @author kongweiguang
fn cwd_check_command(kind: RemoteShellKind) -> &'static str {
    match kind {
        RemoteShellKind::Zsh => {
            "[[ \"$PWD\" == \"$KERMINAL_EXPECTED_CWD\" ]] && printf '%s\\n' \"$KERMINAL_CWD_OK_TOKEN\" || printf '%s\\n' \"$KERMINAL_CWD_WRONG_TOKEN\""
        }
        RemoteShellKind::Fish => {
            "if test \"$PWD\" = \"$KERMINAL_EXPECTED_CWD\"; printf '%s\\n' \"$KERMINAL_CWD_OK_TOKEN\"; else; printf '%s\\n' \"$KERMINAL_CWD_WRONG_TOKEN\"; end"
        }
        _ => "",
    }
}

/// 返回 fixture 自定义且不含私有路径的可见提示符，避免 OSC 到达被误认为 prompt 已呈现。
///
/// @author kongweiguang
fn fixture_prompt(kind: RemoteShellKind) -> &'static [u8] {
    match kind {
        RemoteShellKind::Zsh => b"ZSH_FIXTURE> ",
        RemoteShellKind::Fish => b"FISH_FIXTURE> ",
        _ => b"",
    }
}

/// 有界 PTY 所有者同时保存 process-group guard、reader 和原始字节，以便失败时只清理本测试。
///
/// @author kongweiguang
struct PtyRun {
    child: Box<dyn Child + Send + Sync>,
    process_group: ProcessTreeGuard,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    events: Receiver<ReaderEvent>,
    reader_thread: Option<JoinHandle<()>>,
    output: Vec<u8>,
    reader_eof: bool,
    exit_status: Option<portable_pty::ExitStatus>,
    finished: bool,
}

/// Reader 线程只向固定容量 channel 发送块或 EOF，不累积完整输出或记录正文。
enum ReaderEvent {
    Bytes(Vec<u8>),
    Eof,
    Error,
}

impl PtyRun {
    /// 以真实 shell `-c` 执行 production exec 字符串，并从 spawn 起持有 PTY 进程组。
    ///
    /// @author kongweiguang
    fn spawn(
        executable: PathBuf,
        exec_command: &str,
        home: &Path,
        user_config: &Path,
        xdg_config: &Path,
        trace: &Path,
        private_tmp: &Path,
        working_directory: &Path,
    ) -> Result<Self, &'static str> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| "native PTY could not be created")?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| "native PTY reader could not be opened")?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|_| "native PTY writer could not be opened")?;
        let mut command = CommandBuilder::new(executable);
        command.args(["-c", exec_command]);
        command.env("HOME", home.as_os_str());
        command.env("ZDOTDIR", user_config.as_os_str());
        command.env("XDG_CONFIG_HOME", xdg_config.as_os_str());
        command.env("KERMINAL_TRACE_DIR", trace.as_os_str());
        command.env("KERMINAL_EXPECTED_CWD", working_directory.as_os_str());
        command.env("KERMINAL_CWD_OK_TOKEN", CWD_OK_TOKEN);
        command.env("KERMINAL_CWD_WRONG_TOKEN", CWD_WRONG_TOKEN);
        command.env("TMPDIR", private_tmp.as_os_str());
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|_| "real shell could not be spawned into the PTY")?;
        drop(pair.slave);
        let process_group =
            match ProcessTreeGuard::from_pty(pair.master.as_ref(), child.as_ref(), None) {
                Ok(guard) => guard,
                Err((_error, _guard)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("native PTY did not expose its isolated process group");
                }
            };

        let (sender, events) = mpsc::sync_channel(32);
        let reader_thread = match thread::Builder::new()
            .name("remote-startup-pty-reader".into())
            .spawn(move || read_pty(reader, sender))
        {
            Ok(reader_thread) => reader_thread,
            Err(_) => {
                let _ = process_group.terminate();
                let _ = child.wait();
                return Err("bounded PTY reader thread could not be started");
            }
        };

        Ok(Self {
            child,
            process_group,
            master: Some(pair.master),
            writer: Some(writer),
            events,
            reader_thread: Some(reader_thread),
            output: Vec::new(),
            reader_eof: false,
            exit_status: None,
            finished: false,
        })
    }

    /// 等待生产 hook 输出首个完整 OSC 133 prompt 周期后再发送用户输入。
    ///
    /// @author kongweiguang
    fn wait_for_initial_prompt(
        &mut self,
        deadline: Instant,
        prompt: &[u8],
    ) -> Result<(), &'static str> {
        self.wait_for(deadline, |output| {
            marker_count(output, OSC_A) >= 1
                && marker_count(output, OSC_B) >= 1
                && contains(output, prompt)
        })
    }

    /// 等待固定 cwd 结果、真实 C/D 命令边界和下一次 A/B prompt 均已输出。
    ///
    /// @author kongweiguang
    fn wait_for_cwd_command_cycle(&mut self, deadline: Instant) -> Result<(), &'static str> {
        self.wait_for(deadline, |output| {
            contains(output, CWD_WRONG_TOKEN.as_bytes())
                || cwd_command_cycle_is_complete(output, CWD_OK_TOKEN.as_bytes())
        })?;
        if contains(&self.output, CWD_WRONG_TOKEN.as_bytes()) {
            return Err("production startup did not leave the shell in the requested cwd");
        }
        if !contains(&self.output, CWD_OK_TOKEN.as_bytes()) {
            return Err("cwd verification did not produce its fixed success token");
        }
        Ok(())
    }

    /// 写入一条固定测试命令并使用 PTY 的 CR 作为 Enter，避免改动环境内真实 shell 配置。
    ///
    /// @author kongweiguang
    fn write_line(&mut self, line: &str) -> Result<(), &'static str> {
        let writer = self
            .writer
            .as_mut()
            .ok_or("PTY input writer is no longer available")?;
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\r"))
            .and_then(|()| writer.flush())
            .map_err(|_| "fixed test input could not be sent through the PTY")
    }

    /// 收到完整 child exit 与 reader EOF 后才确认自然退出和尾部字节都已回收。
    ///
    /// @author kongweiguang
    fn finish_natural_exit(&mut self, deadline: Instant) -> Result<(), &'static str> {
        while !self.reader_eof || self.exit_status.is_none() {
            self.receive_one(deadline, true)?;
            if self.exit_status.is_none() {
                self.exit_status = self
                    .child
                    .try_wait()
                    .map_err(|_| "shell exit status could not be observed")?;
            }
        }
        let status = self
            .child
            .wait()
            .map_err(|_| "naturally exited shell could not be reaped")?;
        if !status.success() || status.exit_code() != 0 {
            return Err("shell did not finish with the requested zero exit status");
        }
        drop(self.writer.take());
        drop(self.master.take());
        self.reader_thread
            .take()
            .ok_or("PTY reader owner disappeared")?
            .join()
            .map_err(|_| "PTY reader did not finish after EOF")?;
        self.finished = true;
        Ok(())
    }

    /// 等待有界输出 predicate，同时把提前 exit、reader 失败和预算耗尽作为真实失败。
    ///
    /// @author kongweiguang
    fn wait_for(
        &mut self,
        deadline: Instant,
        condition: impl Fn(&[u8]) -> bool,
    ) -> Result<(), &'static str> {
        loop {
            if condition(&self.output) {
                return Ok(());
            }
            if self.reader_eof {
                return Err("PTY reached EOF before the expected shell event");
            }
            self.receive_one(deadline, true)?;
            if self.exit_status.is_none() {
                self.exit_status = self
                    .child
                    .try_wait()
                    .map_err(|_| "shell exit status could not be observed")?;
            }
            if self.exit_status.is_some() && !condition(&self.output) {
                return Err("shell exited before producing the expected event");
            }
        }
    }

    /// 在一个 PTY 总 deadline 内取一块输出；输出上限避免 shell profile 无限刷屏占用内存。
    ///
    /// @author kongweiguang
    fn receive_one(&mut self, deadline: Instant, retain_bytes: bool) -> Result<(), &'static str> {
        let now = Instant::now();
        if now >= deadline {
            return Err("PTY startup test exceeded its total deadline");
        }
        let timeout = (deadline - now).min(Duration::from_millis(50));
        match self.events.recv_timeout(timeout) {
            Ok(ReaderEvent::Bytes(bytes)) => {
                if retain_bytes {
                    if self.output.len().saturating_add(bytes.len()) > MAX_CAPTURE_BYTES {
                        return Err("PTY output exceeded its bounded capture size");
                    }
                    self.output.extend_from_slice(&bytes);
                }
            }
            Ok(ReaderEvent::Eof) => self.reader_eof = true,
            Ok(ReaderEvent::Error) => return Err("PTY reader returned an unexpected error"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("PTY reader channel closed before reporting EOF");
            }
        }
        Ok(())
    }

    /// 测试失败时仅终止本 PTY 的进程组，并在有限时间内消费 reader 后释放其句柄。
    ///
    /// @author kongweiguang
    fn abort_owned_process_group(&mut self) {
        let cleanup_deadline = Instant::now() + CLEANUP_DEADLINE;
        if self.exit_status.is_none() {
            if let Some(writer) = self.writer.as_mut() {
                let _ = writer.write_all(b"\x03exit\r");
                let _ = writer.flush();
            }
            while Instant::now() < cleanup_deadline {
                let _ = self.receive_one(cleanup_deadline, false);
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.exit_status = Some(status);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        }
        if self.exit_status.is_none() || !self.reader_eof {
            let _ = self.process_group.terminate();
        }
        while !self.reader_eof && Instant::now() < cleanup_deadline {
            let _ = self.receive_one(cleanup_deadline, false);
        }
        if self.exit_status.is_none() {
            while Instant::now() < cleanup_deadline {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.exit_status = Some(status);
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }
        drop(self.writer.take());
        drop(self.master.take());
        if self.reader_eof {
            if let Some(reader_thread) = self.reader_thread.take() {
                let _ = reader_thread.join();
            }
        } else {
            // 进程组已定向终止；若内核 reader 仍未给 EOF，不能无界 join 测试线程。
            self.reader_thread.take();
        }
        self.finished = true;
    }
}

impl Drop for PtyRun {
    /// 对 panic/早退路径执行同样的有界、仅限本 PTY 进程组的清理。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if !self.finished {
            self.abort_owned_process_group();
        }
    }
}

/// 持续读 PTY 并通过有界 channel 交付数据，避免 shell 输出与测试写输入互相阻塞。
///
/// @author kongweiguang
fn read_pty(mut reader: Box<dyn Read + Send>, sender: SyncSender<ReaderEvent>) {
    let mut buffer = [0u8; READER_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(ReaderEvent::Eof);
                return;
            }
            Ok(length) => {
                if sender
                    .send(ReaderEvent::Bytes(buffer[..length].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Unix PTY master 在最后一个 slave 关闭后可用 EIO 表示 EOF。
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                let _ = sender.send(ReaderEvent::Eof);
                return;
            }
            Err(_) => {
                let _ = sender.send(ReaderEvent::Error);
                return;
            }
        }
    }
}

/// 列出本次私有 TMPDIR 中专用前缀的真实 wrapper 目录，不读取其中脚本正文。
///
/// @author kongweiguang
fn wrapper_directories(private_tmp: &Path) -> io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(private_tmp)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("kerminal.") && entry.path().is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

/// 只在所有实际 Profile marker 次数精确匹配且 HOME decoy 未运行时通过。
///
/// @author kongweiguang
fn profile_load_counts_match(
    kind: RemoteShellKind,
    home: &Path,
    user_config: &Path,
    fish_config: &Path,
    trace: &Path,
) -> bool {
    match kind {
        RemoteShellKind::Zsh => {
            marker_file_matches(trace.join("zshenv"), 2)
                && marker_file_matches(trace.join("zprofile"), 1)
                && marker_file_matches(trace.join("zshrc"), 1)
                && marker_file_matches(trace.join("zlogin"), 1)
                && marker_file_matches(trace.join("zlogout"), 1)
                && !trace.join("wrong-home-zshenv").exists()
                && user_config.join(".zshenv").exists()
                && home.join(".zshenv").exists()
        }
        RemoteShellKind::Fish => {
            marker_file_matches(trace.join("fish-login-config"), 1)
                && marker_file_matches(trace.join("fish-interactive-config"), 1)
                && !trace.join("wrong-home-fish").exists()
                && fish_config.join("config.fish").exists()
        }
        _ => false,
    }
}

/// 比较固定 profile 标记的字节数，不把 profile 正文或临时路径写入诊断。
///
/// @author kongweiguang
fn marker_file_matches(path: PathBuf, expected_count: usize) -> bool {
    fs::read(path).is_ok_and(|actual| actual == vec![b'x'; expected_count])
}

/// 验证两个 prompt 周期以及 C→D(0)→A→B 的真实 OSC 顺序。
///
/// @author kongweiguang
fn osc_prompt_cycles_are_ordered(output: &[u8]) -> bool {
    let a = marker_positions(output, OSC_A);
    let b = marker_positions(output, OSC_B);
    let c = marker_positions(output, OSC_C);
    let d = marker_positions(output, OSC_D_ZERO);
    a.len() >= 2
        && b.len() >= 2
        && !c.is_empty()
        && !d.is_empty()
        && a[0] < b[0]
        && c[0] < d[0]
        && d[0] < a[1]
        && a[1] < b[1]
}

/// 确认 cwd 查询命令的真实 shell cycle 与第二次 prompt 均到达。
///
/// @author kongweiguang
fn cwd_command_cycle_is_complete(output: &[u8], success_token: &[u8]) -> bool {
    contains(output, success_token)
        && marker_count(output, OSC_C) >= 1
        && marker_count(output, OSC_D_ZERO) >= 1
        && marker_count(output, OSC_A) >= 2
        && marker_count(output, OSC_B) >= 2
}

/// 查找固定 OSC 序列的 byte offsets，以检验事件顺序而不是只做终端正文 contains。
///
/// @author kongweiguang
fn marker_positions(output: &[u8], marker: &[u8]) -> Vec<usize> {
    if marker.is_empty() || output.len() < marker.len() {
        return Vec::new();
    }
    output
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == marker).then_some(index))
        .collect()
}

/// 统计固定协议标记，不解释或记录 shell 输出中的其它控制字节。
///
/// @author kongweiguang
fn marker_count(output: &[u8], marker: &[u8]) -> usize {
    marker_positions(output, marker).len()
}

/// 匹配固定安全 token；该 helper 不格式化或返回任何 PTY 输出内容。
///
/// @author kongweiguang
fn contains(output: &[u8], token: &[u8]) -> bool {
    !token.is_empty() && output.windows(token.len()).any(|window| window == token)
}

/// 拒绝 bootstrap 源码标记和连续八进制转义回显，避免只检查 prompt 而漏掉刷屏回归。
///
/// @author kongweiguang
fn has_bootstrap_echo(output: &[u8]) -> bool {
    const PRIVATE_SOURCE_MARKERS: [&[u8]; 3] = [
        b"__kerminal_write_script",
        b"__kerminal_cleanup",
        b"__kerminal_hook_zdotdir",
    ];
    if PRIVATE_SOURCE_MARKERS
        .iter()
        .any(|marker| contains(output, marker))
    {
        return true;
    }
    output.windows(8).any(|bytes| {
        bytes[0] == b'\\'
            && bytes[1..4].iter().all(u8::is_ascii_digit)
            && bytes[1..4].iter().all(|byte| (b'0'..=b'7').contains(byte))
            && bytes[4] == b'\\'
            && bytes[5..8].iter().all(|byte| (b'0'..=b'7').contains(byte))
    })
}
