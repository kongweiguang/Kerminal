//! AppImage AppRun 注入环境的宿主恢复。
//!
//! cargo-packager 0.11.8 固定复制的 AppRun 会无条件覆盖 PYTHONHOME，并把 AppDir
//! 路径前插到 PATH、LD_LIBRARY_PATH、PYTHONPATH 等变量。Kerminal 在 AppImage 内
//! 运行时确实需要这些值加载打包依赖，但新建的本地 PTY 属于宿主 shell；继续继承
//! 挂载目录会让宿主 python3 等程序在 AppDir 缺少标准库或卸载后直接失效。
//! 重新运行外层 AppImage 时也要恢复宿主环境，让新版启动器只注入自己的 AppDir。
//!
//! @author kongweiguang

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// AppRun 以“AppDir 前缀:宿主原值”方式注入的路径列表变量。
///
/// 列表来自 cargo-packager 0.11.8 下载的 AppRun 源码；每一项都把宿主原值保留在
/// 后段，因此可以按 AppDir 组件恢复，不依赖启动器额外保存备份变量。只恢复路径类
/// 变量，AppImage 应用自身的进程环境和其它非路径变量保持不动。
///
/// @author kongweiguang
const APPIMAGE_PATH_LIST_VARIABLES: &[&str] = &[
    "PATH",
    "LD_LIBRARY_PATH",
    "PYTHONPATH",
    "XDG_DATA_DIRS",
    "PERLLIB",
    "GSETTINGS_SCHEMA_DIR",
    "QT_PLUGIN_PATH",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
];

/// 从真实可执行文件路径识别 AppImage AppDir。
///
/// 同时要求 `<AppDir>/usr/bin/<binary>` 布局和 `<AppDir>/AppRun` 标记：raw FUSE
/// 挂载与直接执行解包后的 AppRun 都满足这一契约；DEB 安装的 `/usr/bin/kerminal`
/// 没有 `/AppRun`，开发构建也不在 AppDir 布局内，因此不会误删系统路径。
///
/// @author kongweiguang
#[cfg(any(target_os = "linux", test))]
pub(crate) fn appdir_from_executable(executable: &Path) -> Option<std::path::PathBuf> {
    let bin_dir = executable.parent()?;
    if bin_dir.file_name() != Some(OsStr::new("bin")) {
        return None;
    }
    let usr_dir = bin_dir.parent()?;
    if usr_dir.file_name() != Some(OsStr::new("usr")) {
        return None;
    }
    let appdir = usr_dir.parent()?;
    // 根目录不可能承载 AppDir 标记；显式排除，避免把系统级布局当成打包目录。
    if appdir.parent().is_none() || !appdir.join("AppRun").is_file() {
        return None;
    }
    Some(appdir.to_path_buf())
}

/// 从路径列表变量中剔除 AppDir 内组件，返回宿主原值。
///
/// AppRun 先把 AppDir 组件放在前面、宿主原值放在后面，所以按路径组件过滤即可
/// 还原用户自己的搜索顺序，不需要额外的原值备份。相对路径、其它绝对路径和空
/// 组件无法归因，保持原位置，避免把本机显式配置误判成启动器注入；过滤后没有
/// 有效组件时返回 None，表示该变量原本未设置或为空。
///
/// @author kongweiguang
fn restore_path_list(value: &OsStr, appdir: &Path) -> Option<OsString> {
    let mut removed = false;
    let mut retained = Vec::new();
    for component in std::env::split_paths(value) {
        if component.starts_with(appdir) {
            removed = true;
        } else {
            retained.push(component);
        }
    }
    if !removed {
        return Some(value.to_os_string());
    }
    if retained.is_empty()
        || retained
            .iter()
            .all(|component| component.as_os_str().is_empty())
    {
        return None;
    }
    // 保留宿主原值时不应把过滤后剩下的路径重新解释成别的路径；拼接失败时保守沿用原值。
    match std::env::join_paths(retained) {
        Ok(joined) => Some(joined),
        Err(_) => Some(value.to_os_string()),
    }
}

/// 在写入 profile 与 shell startup 环境之前清理 AppImage 启动器注入值。
///
/// 只修改 CommandBuilder 继承的环境快照，不触碰进程自身环境，因此 AppImage
/// 运行时的依赖查找和更新自替换逻辑不受影响。显式 profile/startup 覆盖由调用方
/// 随后写入，优先级保持不变。AppRun 会无条件覆盖 PYTHONHOME，被覆盖前的本机
/// 显式值没有备份可恢复；这里只移除明确的 AppDir Python 根，避免伪造出错误值。
///
/// @author kongweiguang
pub(crate) fn restore_host_environment(command: &mut portable_pty::CommandBuilder, appdir: &Path) {
    let changes = host_environment_changes(appdir, |name| {
        command.get_env(name).map(OsStr::to_os_string)
    });
    for (name, value) in changes {
        match value {
            Some(restored) => command.env(name, restored),
            None => command.env_remove(name),
        }
    }
}

/// 统一 PTY 和重启的恢复规则，调用方提供各自的继承快照，避免修改父进程环境。
fn host_environment_changes(
    appdir: &Path,
    mut get: impl FnMut(&str) -> Option<OsString>,
) -> Vec<(&'static str, Option<OsString>)> {
    let mut changes = Vec::new();
    for &name in APPIMAGE_PATH_LIST_VARIABLES {
        if let Some(value) = get(name) {
            let restored = restore_path_list(&value, appdir);
            if restored.as_deref() != Some(value.as_os_str()) {
                changes.push((name, restored));
            }
        }
    }
    if get("PYTHONHOME").is_some_and(|value| Path::new(&value) == appdir.join("usr")) {
        changes.push(("PYTHONHOME", None));
    }
    changes
}

/// AppImage 安装替换外层文件，重启必须走该入口。只信任已识别 AppDir 之外的绝对路径，
/// 并让新版启动器重建挂载相关环境，避免新进程继续依赖旧挂载或临时解包目录。
#[cfg(target_os = "linux")]
pub fn restart_command(
    executable: &Path,
    appimage: Option<&OsStr>,
    working_directory: &Path,
) -> std::io::Result<Option<std::process::Command>> {
    let Some((appdir, appimage)) = appdir_from_executable(executable).zip(appimage.map(Path::new))
    else {
        return Ok(None);
    };
    if !appimage.is_absolute() || appimage.starts_with(&appdir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AppImage 重启入口必须是旧 AppDir 之外的绝对路径",
        ));
    }

    let mut command = std::process::Command::new(appimage);
    let directory = if working_directory.starts_with(&appdir) {
        appimage.parent().unwrap_or(Path::new("/"))
    } else {
        working_directory
    };
    command.current_dir(directory);
    for (name, value) in host_environment_changes(&appdir, |name| std::env::var_os(name)) {
        match value {
            Some(restored) => command.env(name, restored),
            None => command.env_remove(name),
        };
    }
    for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        command.env_remove(name);
    }
    Ok(Some(command))
}
