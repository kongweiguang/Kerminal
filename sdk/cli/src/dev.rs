//! 开发模式使用临时配置根和显式重载文件，不接触用户正式配置。
//! @author kongweiguang

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use kerminal_plugin_protocol::ManifestPolicy;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use process_wrap::std::{ChildWrapper, CommandWrap};

use crate::{Result, build};

/// Kerminal 退出或监听失败时统一回收子进程与隔离配置根。
pub fn run(project: &Path, executable: Option<&Path>, policy: ManifestPolicy) -> Result<()> {
    let project = project.canonicalize()?;
    let plugin = build::build(&project, None, policy)?;
    let config = tempfile::tempdir()?;
    let desktop = match executable {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()?
            .parent()
            .ok_or("CLI 没有安装目录")?
            .join(if cfg!(windows) {
                "kerminal.exe"
            } else {
                "kerminal"
            }),
    };
    let mut command = CommandWrap::with_new(desktop, |command| {
        command
            .env("KERMINAL_CONFIG_ROOT", config.path())
            .arg("--dev-plugin")
            .arg(&plugin);
    });
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    let child = command.spawn()?;
    let mut child = ChildGuard(child);
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        let _ = sender.send(event);
    })?;
    watcher.watch(&project, RecursiveMode::Recursive)?;
    let mut generation = 0u64;
    loop {
        if let Some(status) = child.0.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(format!("Kerminal 开发进程异常退出: {status}").into());
        }
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) if relevant(&project, &event) => {
                while let Ok(event) = receiver.recv_timeout(Duration::from_millis(250)) {
                    event?;
                }
                match build::build(&project, None, policy) {
                    Ok(plugin) => {
                        generation = generation.checked_add(1).ok_or("开发重载代次溢出")?;
                        publish_reload(config.path(), generation, &plugin)?;
                        eprintln!("插件构建完成，已请求重载代次 {generation}");
                    }
                    Err(error) => eprintln!("插件构建失败，保留上一次结果: {error}"),
                }
            }
            Ok(Ok(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("文件监听已断开".into()),
        }
    }
}

/// 只响应源码变更，构建产物和依赖目录不能触发无限重建。
fn relevant(project: &Path, event: &Event) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|path| {
        ![
            ".kerminal",
            ".kerminal-sdk",
            ".git",
            "node_modules",
            "target",
        ]
        .iter()
        .any(|name| path.starts_with(project.join(name)))
    })
}

/// 先完整写入同目录临时文件再替换，桌面端只会看到完整 JSON。
fn publish_reload(root: &Path, generation: u64, plugin: &Path) -> Result<()> {
    let directory = root.join("plugins");
    fs::create_dir_all(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    serde_json::to_writer(
        temporary.as_file_mut(),
        &serde_json::json!({
            "generation": generation,
            "plugin_dir": plugin.canonicalize()?,
        }),
    )?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join("dev-reload.json"))
        .map_err(|error| error.error)?;
    Ok(())
}

struct ChildGuard(Box<dyn ChildWrapper>);

impl Drop for ChildGuard {
    /// 主进程退出后仍回收进程组，避免遗留使用临时配置根的子进程。
    fn drop(&mut self) {
        let _ = self.0.start_kill();
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.wait();
        }
    }
}

/// 默认桌面程序与 CLI 同目录，调用方也可明确指定独立候选。
pub fn desktop_path() -> Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or("CLI 没有安装目录")?
        .join(if cfg!(windows) {
            "kerminal.exe"
        } else {
            "kerminal"
        }))
}
