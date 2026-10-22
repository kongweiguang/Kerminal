//! 由 `portable_pty_job_list` 在测试临时目录编译并启动的 Windows 子进程探针。
//!
//! @author kongweiguang

use std::env;
use std::fs;
use std::io;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// 先派生一个立即常驻的孙进程，再用文件发布 PID，稳定暴露 spawn 后加入 Job 的竞态窗口。
///
/// @author kongweiguang
fn main() {
    if let Err(error) = run() {
        eprintln!("portable-pty Job List helper failed: {error}");
        std::process::exit(1);
    }
}

/// 根据阶段参数运行根进程或常驻孙进程；两者都必须由同一个 Job Object 收回。
///
/// @author kongweiguang
fn run() -> io::Result<()> {
    let stage = env::args_os()
        .nth(1)
        .and_then(|value| value.into_string().ok());
    match stage.as_deref() {
        Some("leaf") => loop {
            thread::sleep(Duration::from_secs(60));
        },
        Some("root") => {
            let marker = env::var_os("KERMINAL_PTY_JOB_MARKER").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "test marker path is missing")
            })?;
            let child = Command::new(env::current_exe()?).arg("leaf").spawn()?;
            fs::write(marker, child.id().to_string())?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected root or leaf stage",
        )),
    }
}
