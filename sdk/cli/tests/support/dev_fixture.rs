//! 用无窗口进程模拟开发桌面，验证隔离根和重载文件的真实交接。
//! @author kongweiguang

use std::{
    env, fs, io,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

/// 只从受控测试目录报告真实参数，避免访问运行中的桌面或用户配置。
fn main() -> io::Result<()> {
    let control = PathBuf::from(
        env::var_os("KERMINAL_DEV_TEST_CONTROL")
            .ok_or_else(|| io::Error::other("missing test control"))?,
    );
    let root = PathBuf::from(
        env::var_os("KERMINAL_CONFIG_ROOT")
            .ok_or_else(|| io::Error::other("missing config root"))?,
    );
    let mut args = env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--dev-plugin"))
        || !args
            .next()
            .map(PathBuf::from)
            .is_some_and(|path| path.is_absolute())
    {
        return Err(io::Error::other("invalid development plugin argument"));
    }
    fs::write(control.join("root.txt"), root.to_string_lossy().as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match fs::read(root.join("plugins/dev-reload.json")) {
            Ok(content) => {
                fs::write(control.join("reload.json"), content)?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::other("reload deadline exceeded"))
}
