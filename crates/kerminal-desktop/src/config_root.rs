//! 用户数据目录解析。开发运行默认写入仓库 target，正式运行使用 `~/.kerminal`。
//!
//! @author kongweiguang

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt,
    path::{Component, Path, PathBuf},
};

const CONFIG_ROOT_ENV: &str = "KERMINAL_CONFIG_ROOT";

/// 已解析的数据根；保留类型边界，避免调用方把工作目录误当成持久化目录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRoot(PathBuf);

impl ConfigRoot {
    /// 开发构建未显式指定时固定使用仓库 target 下的隔离目录，避免误写用户数据。
    pub fn resolve() -> Result<Self, ConfigRootError> {
        let override_path = env::var_os(CONFIG_ROOT_ENV);
        let home = current_home_dir();
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .ok_or(ConfigRootError::WorkspacePathUnavailable)?
            .to_path_buf();
        let path = resolve_from_inputs(override_path, cfg!(debug_assertions), &workspace, home)?;
        Ok(Self(path))
    }

    /// 供配置、vault 与数据库 adapter 共用同一个已解析根目录。
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// 显式路径也可能经由 junction 或符号链接指向正式根；身份检查只解析现存路径，不创建目录。
    ///
    /// @author kongweiguang
    pub(crate) fn points_to_formal_default(path: &Path) -> bool {
        points_to_formal_default_from_inputs(path, current_home_dir(), env::current_dir().ok())
    }
}

/// 根据启动环境确定目录；开发默认隔离，正式默认使用用户 home 下的稳定路径。
/// 正式根规则与迁移离线检查共用，避免显式 --root 指向同一目录时绕过旧实例门禁。
///
/// @author kongweiguang
fn resolve_from_inputs(
    override_path: Option<OsString>,
    is_development: bool,
    workspace_root: &Path,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf, ConfigRootError> {
    if let Some(path) = override_path {
        if path.is_empty() {
            return Err(ConfigRootError::EmptyOverride);
        }
        return Ok(PathBuf::from(path));
    }

    if is_development {
        return Ok(workspace_root.join("target").join("kerminal-dev-config"));
    }

    formal_default_root(home_dir)
}

/// 保持既有 home/.kerminal 选择规则集中；此处只拼接路径，不查询文件系统。
///
/// @author kongweiguang
fn formal_default_root(home_dir: Option<PathBuf>) -> Result<PathBuf, ConfigRootError> {
    home_dir
        .map(|home| home.join(".kerminal"))
        .ok_or(ConfigRootError::HomeDirectoryUnavailable)
}

/// 只有能证明为不同目录才略过旧实例检查；home、cwd 或平台路径身份不明时保守启用检查。
///
/// @author kongweiguang
fn points_to_formal_default_from_inputs(
    path: &Path,
    home_dir: Option<PathBuf>,
    current_dir: Option<PathBuf>,
) -> bool {
    let Ok(formal_root) = formal_default_root(home_dir) else {
        return true;
    };
    // USERPROFILE/HOME 必须是绝对路径；相对 home 无法证明与旧版使用的正式目录相同。
    if !formal_root.is_absolute() {
        return true;
    }
    let Some(path) = resolved_path_identity(path, current_dir.as_deref()) else {
        return true;
    };
    let Some(formal_root) = resolved_path_identity(&formal_root, current_dir.as_deref()) else {
        return true;
    };
    paths_may_match_formal_root(&path, &formal_root)
}

/// 按文件系统顺序解析正式根身份，避免把 symlink/junction 后面的 `..` 错当作纯词法路径。
///
/// 逐段 canonicalize 仍存在的前缀；缺失后缀被 `..` 抵消后，继续解析后续目录别名。
/// 无法确定身份时调用方会保守启用旧实例探测；此函数只查元数据，不创建根目录或迁移文件。
/// Component 的 Prefix 变体在所有平台都存在；匹配保持穷尽，平台专有的前缀检查单独限定为 Windows。
///
/// @author kongweiguang
fn resolved_path_identity(path: &Path, current_dir: Option<&Path>) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next()
        && !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_)
                | std::path::Prefix::VerbatimDisk(_)
                | std::path::Prefix::UNC(_, _)
                | std::path::Prefix::VerbatimUNC(_, _)
        )
    {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        #[cfg(windows)]
        if path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
            return None;
        }
        let current_dir = current_dir.filter(|directory| directory.is_absolute())?;
        current_dir.join(path)
    };

    let mut resolved = PathBuf::new();
    let mut unresolved_suffix = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) => {
                resolved.push(component.as_os_str());
            }
            Component::RootDir => {
                resolved.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // 缺失后缀被 `..` 抵消后，后续组件必须重新尝试解析链接身份。
                if !unresolved_suffix.pop() {
                    resolved.pop();
                }
            }
            Component::Normal(name) if !unresolved_suffix.as_os_str().is_empty() => {
                unresolved_suffix.push(name);
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(_) => {
                        // 每个现存组件都先解析链接；后续 `..` 才能遵守文件系统路径语义。
                        resolved = std::fs::canonicalize(candidate).ok()?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        unresolved_suffix.push(name);
                    }
                    Err(_) => return None,
                }
            }
        }
    }

    if !unresolved_suffix.as_os_str().is_empty() {
        resolved.push(unresolved_suffix);
    }
    // 此时 ParentDir 已按真实已解析前缀处理，normalization 仅统一 Windows verbatim 前缀。
    Some(normalize_path(&resolved))
}

/// 单实例身份与迁移来源判断共用现有词法规则；Windows 只统一 disk/UNC 前缀，不访问或重开路径。
///
/// @author kongweiguang
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            #[cfg(windows)]
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::VerbatimDisk(drive) => {
                    normalized.push(format!("{}:", char::from(drive)));
                }
                std::path::Prefix::VerbatimUNC(server, share) => {
                    let mut unc = OsString::from(r"\\");
                    unc.push(server);
                    unc.push(r"\");
                    unc.push(share);
                    normalized.push(unc);
                }
                _ => normalized.push(prefix.as_os_str()),
            },
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Windows 的 ASCII 大小写按目录身份比较；不自行折叠 Unicode，不能证明相等或不等时交回保守门禁。
///
/// @author kongweiguang
#[cfg(windows)]
fn paths_may_match_formal_root(left: &Path, right: &Path) -> bool {
    let left: Vec<_> = left.components().collect();
    let right: Vec<_> = right.components().collect();
    if left.len() != right.len() {
        return false;
    }
    for (left, right) in left.into_iter().zip(right) {
        let left = left.as_os_str().as_encoded_bytes();
        let right = right.as_os_str().as_encoded_bytes();
        if left == right || left.eq_ignore_ascii_case(right) {
            continue;
        }
        if left.is_ascii() && right.is_ascii() {
            return false;
        }
    }
    true
}

/// Unix 保留文件系统大小写语义，不为匹配 Windows 的默认根策略修改目录名称。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn paths_may_match_formal_root(left: &Path, right: &Path) -> bool {
    left == right
}

/// Windows 使用 USERPROFILE，Unix 使用 HOME；缺失时显式失败，不猜测另一个账户目录。
fn current_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";

    env::var_os(key).map(PathBuf::from)
}

/// 启动目录解析失败时保留错误原因，便于桌面启动器显示可操作的诊断。
#[derive(Debug)]
pub enum ConfigRootError {
    EmptyOverride,
    HomeDirectoryUnavailable,
    WorkspacePathUnavailable,
}

impl fmt::Display for ConfigRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyOverride => formatter.write_str("KERMINAL_CONFIG_ROOT cannot be empty"),
            Self::HomeDirectoryUnavailable => {
                formatter.write_str("user home directory is unavailable")
            }
            Self::WorkspacePathUnavailable => {
                formatter.write_str("development workspace path is unavailable")
            }
        }
    }
}

impl Error for ConfigRootError {}

#[cfg(test)]
#[path = "../tests/support/config_root_unit.rs"]
mod tests;
