//! 受管子进程树回归测试使用的子进程模式。
//!
//! @author kongweiguang

use std::{
    env, fs, io,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

pub(crate) const PROCESS_HELPER_ROLE: &str = "KERM_PROCESS_TREE_HELPER_ROLE";
pub(crate) const PROCESS_HELPER_PID_FILE: &str = "KERM_PROCESS_TREE_HELPER_PID_FILE";
pub(crate) const PROCESS_HELPER_FAILURE_FILE: &str = "KERM_PROCESS_TREE_HELPER_FAILURE_FILE";
pub(crate) const PROCESS_HELPER_STDOUT_MODE: &str = "KERM_PROCESS_TREE_HELPER_STDOUT_MODE";
pub(crate) const PROCESS_HELPER_STAGE_FILE: &str = "KERM_PROCESS_TREE_HELPER_STAGE_FILE";

/// 在专用测试函数中扮演 root 或 leaf；root 只写固定阶段标记，避免 ConPTY 吞掉启动进度。
///
/// @author kongweiguang
pub(crate) fn run_helper_mode() -> io::Result<()> {
    let result = match env::var(PROCESS_HELPER_ROLE).as_deref() {
        Ok("root") => {
            write_helper_stage("root-entered")?;
            spawn_leaf_and_wait()
        }
        Ok("leaf") => wait_forever(),
        _ => Ok(()),
    };

    if let Err(error) = &result
        && let Some(path) = env::var_os(PROCESS_HELPER_FAILURE_FILE)
    {
        let raw_os_error = error
            .raw_os_error()
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        let diagnostic = format!("kind={:?};raw_os_error={raw_os_error}", error.kind());
        let _ = fs::write(path, diagnostic);
    }

    result
}

/// 让 root 立即派生 leaf；PID 通过同目录临时文件完整写入后原子发布，避免父测试读到空文件。
/// 固定阶段侧车区分 libtest 未入测、CreateProcess 卡住及 PID 发布阶段；ConPTY 根标准句柄为空时用 NUL。
///
/// @author kongweiguang
fn spawn_leaf_and_wait() -> io::Result<()> {
    let pid_file = env::var_os(PROCESS_HELPER_PID_FILE).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "process helper PID file is missing",
        )
    })?;
    write_helper_stage("pid-path-read")?;
    let executable = env::current_exe()?;
    write_helper_stage("program-resolved")?;
    let stdout = match env::var(PROCESS_HELPER_STDOUT_MODE).as_deref() {
        Ok("null") => Stdio::null(),
        _ => Stdio::inherit(),
    };
    write_helper_stage("before-leaf-spawn")?;
    let mut child = Command::new(executable)
        .args([
            "--exact",
            "process_tree_spawn::process_tree_spawn_helper",
            "--nocapture",
        ])
        .env(PROCESS_HELPER_ROLE, "leaf")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()?;
    write_helper_stage("leaf-created")?;
    let pid_file = Path::new(&pid_file);
    if let Err(error) = publish_pid_file(pid_file, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    write_helper_stage("pid-published")?;
    wait_forever()
}

/// 先写完整 PID 到同目录新文件，再 rename 发布，避免读者把 create/truncate 窗口误判为损坏内容。
///
/// @author kongweiguang
fn publish_pid_file(path: &Path, pid: u32) -> io::Result<()> {
    let pending_path = path.with_extension("pid-pending");
    let mut pending_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_path)?;
    let pid_text = pid.to_string();
    let write_result = pending_file.write_all(pid_text.as_bytes());
    drop(pending_file);
    if let Err(error) = write_result {
        return Err(remove_pending_pid_after_error(&pending_path, error));
    }

    if let Err(error) = fs::rename(&pending_path, path) {
        return Err(remove_pending_pid_after_error(&pending_path, error));
    }
    Ok(())
}

/// 失败时尽量清理本次 staging 文件，同时保留发布错误并显式附加清理失败类别。
///
/// @author kongweiguang
fn remove_pending_pid_after_error(path: &Path, publish_error: io::Error) -> io::Error {
    match fs::remove_file(path) {
        Ok(()) => publish_error,
        Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => publish_error,
        Err(cleanup_error) => io::Error::other(format!(
            "PID publication failed ({publish_error}); pending-file cleanup failed ({:?})",
            cleanup_error.kind()
        )),
    }
}

/// 将预定义阶段写入独立 sidecar；写入失败必须传播，避免把缺失诊断误报为阶段成功。
///
/// @author kongweiguang
fn write_helper_stage(stage: &'static str) -> io::Result<()> {
    if let Some(path) = env::var_os(PROCESS_HELPER_STAGE_FILE) {
        fs::write(path, stage)?;
    }
    Ok(())
}

/// 保持 helper 活着直到测试通过 process group 或 Job 一次性终止整棵树。
fn wait_forever() -> io::Result<()> {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
