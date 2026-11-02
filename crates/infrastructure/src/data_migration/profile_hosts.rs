//! 将旧 Profile 的本地主机投影迁移为独立 host 快照。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    local_shells::{LocalShellCandidate, LocalShellDiscovery},
    preferences::DefaultLocalShellProvider,
};
use kerminal_domain::preferences::ConfigDocumentPath;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use super::{FileEdit, journal};
use crate::{
    configuration,
    platform::{
        default_shell::SystemDefaultLocalShellProvider, local_shells::SystemLocalShellDiscovery,
    },
    vault::profile_migration,
};

const MAX_PROFILE_COUNT: usize = 16_384;

/// Profile 来源与新主机映射只存在一次规划期；环境变量在释放时清除。
#[derive(Deserialize)]
struct LegacyProfile {
    id: String,
    name: String,
    shell: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sidebar_group_id: Option<String>,
    sort_order: i64,
    created_at: String,
    updated_at: String,
    #[serde(skip)]
    source: Vec<u8>,
}

impl Drop for LegacyProfile {
    /// 模板本身可能携带敏感环境覆盖，不让临时解码值在内存中长期残留。
    fn drop(&mut self) {
        for value in self.env.values_mut() {
            value.zeroize();
        }
        self.source.zeroize();
    }
}

#[derive(Serialize)]
struct LocalHostDocument<'a> {
    schema_version: u8,
    protocol: &'static str,
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<&'a str>,
    sort_order: i64,
    created_at: &'a str,
    updated_at: &'a str,
    migration_origin: &'a str,
    local: LocalHostFields<'a>,
}

#[derive(Serialize)]
struct LocalHostFields<'a> {
    shell: &'a str,
    args: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_ref: Option<&'a str>,
}

#[derive(Deserialize)]
struct ExistingLocalHost {
    id: String,
    protocol: String,
    name: String,
    group_id: Option<String>,
    sort_order: i64,
    created_at: String,
    updated_at: String,
    migration_origin: Option<String>,
    local: ExistingLocalFields,
}

#[derive(Deserialize)]
struct ExistingLocalFields {
    shell: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    environment_ref: Option<String>,
}

/// 同一映射供 workspace、Agent 与主机快照共同使用，避免独立生成不同 ID。
pub(super) struct ProfileHostPlan {
    pub(super) mapping: BTreeMap<String, String>,
    pub(super) edits: Vec<FileEdit>,
}

#[derive(Serialize)]
struct InitialProfileDocument<'a> {
    schema_version: u8,
    id: &'a str,
    name: &'a str,
    shell: &'a str,
    args: Vec<&'a str>,
    is_default: bool,
    sort_order: i64,
    created_at: &'a str,
    updated_at: &'a str,
}

/// 仅在全新根初始化时扫描系统 Shell，避免用户删除所有模板后被后续启动重新创建。
pub(super) fn plan_initial_profiles() -> AppResult<Vec<FileEdit>> {
    let candidates = SystemLocalShellDiscovery::new().discover(&CancellationToken::new())?;
    let candidates = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.program().to_str().is_some()
                && candidate
                    .args()
                    .iter()
                    .all(|argument| argument.to_str().is_some())
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let default_program = SystemDefaultLocalShellProvider::new()
        .resolve_default()?
        .program;
    let default_ix = candidates
        .iter()
        .position(|candidate| programs_match(candidate, &default_program))
        .unwrap_or(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AppError::infrastructure("初始化 Profile 的系统时钟无效"))?
        .as_secs()
        .to_string();
    let mut edits = Vec::with_capacity(candidates.len());
    for (ix, candidate) in candidates.iter().enumerate() {
        let id = format!("standard-{}", journal::sha256(candidate.id().as_bytes()));
        let shell = candidate
            .program()
            .to_str()
            .ok_or_else(|| AppError::invalid("系统 Shell 路径无法表示"))?;
        let args = candidate
            .args()
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .ok_or_else(|| AppError::invalid("系统 Shell 参数无法表示"))
            })
            .collect::<AppResult<Vec<_>>>()?;
        let document = InitialProfileDocument {
            schema_version: 1,
            id: &id,
            name: candidate.display_name(),
            shell,
            args,
            is_default: ix == default_ix,
            sort_order: i64::try_from(ix).map_err(|_| AppError::invalid("系统 Shell 数量超限"))?,
            created_at: &timestamp,
            updated_at: &timestamp,
        };
        let source = toml::to_string_pretty(&document)
            .map_err(|_| AppError::invalid("系统 Profile 无法编码"))?;
        let path = ConfigDocumentPath::Profile(id);
        configuration::validate_current_configuration_document(&path, &source)?;
        edits.push(FileEdit {
            relative_path: path
                .relative_path()
                .map_err(|_| AppError::invalid("系统 Profile ID 无效"))?,
            before: None,
            after: source.into_bytes(),
        });
    }
    Ok(edits)
}

/// 使用系统解析出的默认可执行文件确定唯一默认项；无法精确匹配时由调用方选择首项。
fn programs_match(candidate: &LocalShellCandidate, default: &std::ffi::OsStr) -> bool {
    let candidate = candidate.program().to_string_lossy();
    let default = default.to_string_lossy();
    if cfg!(windows) {
        candidate.eq_ignore_ascii_case(&default)
    } else {
        candidate == default
    }
}

/// 只读规划独立本地主机；调用方负责将全部文件并入一个 journal 和最终 marker。
pub(super) fn plan_profile_hosts(
    source_root: &Path,
    target_root: &Path,
    vault_edits: &mut Vec<FileEdit>,
    imported_groups: &BTreeSet<String>,
) -> AppResult<ProfileHostPlan> {
    let profiles = read_legacy_profiles(source_root)?;
    if profiles.is_empty() {
        return Ok(ProfileHostPlan {
            mapping: BTreeMap::new(),
            edits: Vec::new(),
        });
    }
    let source_identity = source_identity(source_root)?;
    let mut groups = group_ids(target_root)?;
    groups.extend(imported_groups.iter().cloned());
    let mut occupied = existing_host_paths(target_root)?;
    let normalized_vault = vault_edits.first().map(|edit| edit.after.as_slice());
    let mut mapping = BTreeMap::new();
    let mut create = Vec::new();
    for profile in &profiles {
        if profile
            .sidebar_group_id
            .as_ref()
            .is_some_and(|id| !groups.contains(id))
        {
            return Err(AppError::conflict("本地主机的旧分组不存在，迁移未执行"));
        }
        let origin = migration_origin(&source_identity, &profile.id);
        let base = format!("local-{}", journal::sha256(profile.id.as_bytes()));
        let mut suffix = 0usize;
        let (host_id, existing) = loop {
            let candidate = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            match occupied.get(&candidate.to_ascii_lowercase()) {
                None => break (candidate, false),
                Some(path) => {
                    if let Some(existing) = read_matching_host(path, &origin)? {
                        if !host_matches_profile(target_root, &existing, profile, normalized_vault)?
                        {
                            return Err(AppError::conflict("已迁移的本地主机已被修改，导入未覆盖"));
                        }
                        break (existing.id, true);
                    }
                }
            }
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| AppError::conflict("本地主机 ID 无法分配"))?;
            if suffix > MAX_PROFILE_COUNT {
                return Err(AppError::conflict("本地主机 ID 冲突过多"));
            }
        };
        if mapping
            .insert(profile.id.clone(), host_id.clone())
            .is_some()
        {
            return Err(AppError::conflict("旧 Profile ID 重复，迁移未执行"));
        }
        if !existing {
            occupied.insert(
                host_id.to_ascii_lowercase(),
                target_root.join("hosts").join(format!("{host_id}.toml")),
            );
            create.push((profile, host_id, origin));
        }
    }

    let environments = create
        .iter()
        .map(|(profile, host_id, _)| (host_id.clone(), &profile.env))
        .collect::<BTreeMap<_, _>>();
    let sealed =
        profile_migration::seal_profile_environments(target_root, &environments, normalized_vault)
            .map_err(|source| {
                AppError::infrastructure_with_source("封存本地主机环境失败", source)
            })?;
    let mut references = BTreeMap::new();
    if let Some(sealed) = sealed {
        references = sealed.references;
        vault_edits.clear();
        if let Some(key) = sealed.new_key {
            vault_edits.push(FileEdit {
                relative_path: PathBuf::from("secrets/vault-key.toml"),
                before: None,
                after: key.as_slice().to_vec(),
            });
        }
        vault_edits.push(FileEdit {
            relative_path: PathBuf::from("secrets/vault.toml"),
            before: sealed.vault_before.as_ref().map(|bytes| bytes.to_vec()),
            after: sealed.vault_after,
        });
    }

    let mut edits = Vec::with_capacity(create.len() + profiles.len());
    for (profile, host_id, origin) in create {
        let document = LocalHostDocument {
            schema_version: 3,
            protocol: "local",
            id: &host_id,
            name: &profile.name,
            group_id: profile.sidebar_group_id.as_deref(),
            sort_order: profile.sort_order,
            created_at: &profile.created_at,
            updated_at: &profile.updated_at,
            migration_origin: &origin,
            local: LocalHostFields {
                shell: &profile.shell,
                args: &profile.args,
                cwd: profile.cwd.as_deref(),
                environment_ref: references.get(&host_id).map(String::as_str),
            },
        };
        let after = toml::to_string_pretty(&document)
            .map_err(|_| AppError::invalid("本地主机快照无法编码"))?;
        let path = ConfigDocumentPath::Host(host_id);
        configuration::validate_current_configuration_document(&path, &after)?;
        edits.push(FileEdit {
            relative_path: path
                .relative_path()
                .map_err(|_| AppError::invalid("本地主机 ID 无效"))?,
            before: None,
            after: after.into_bytes(),
        });
    }
    if source_root == target_root {
        for profile in &profiles {
            if profile.sidebar_group_id.is_none() {
                let path = ConfigDocumentPath::Profile(profile.id.clone());
                edits.push(FileEdit {
                    relative_path: path
                        .relative_path()
                        .map_err(|_| AppError::invalid("Profile ID 无效"))?,
                    before: Some(profile.source.clone()),
                    after: profile.source.clone(),
                });
            }
        }
    }
    Ok(ProfileHostPlan { mapping, edits })
}

/// 只接受根内普通 Profile 文件和稳定文件名，不能通过链接迁入外部文件。
fn read_legacy_profiles(root: &Path) -> AppResult<Vec<LegacyProfile>> {
    let directory = root.join("profiles");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err(AppError::conflict("Profile 目录不可安全读取")),
    }
    let mut entries = fs::read_dir(&directory)
        .map_err(|source| AppError::infrastructure_with_source("读取 Profile 目录失败", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::infrastructure_with_source("读取 Profile 列表失败", source))?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut profiles = Vec::new();
    for entry in entries {
        if entry.path().extension() != Some(OsStr::new("toml")) {
            continue;
        }
        if profiles.len() >= MAX_PROFILE_COUNT {
            return Err(AppError::invalid("Profile 数量超出迁移限制"));
        }
        let file_type = entry
            .file_type()
            .map_err(|_| AppError::conflict("Profile 路径不可读取"))?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(AppError::conflict("Profile 迁移只接受普通文件"));
        }
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::invalid("Profile 文件名无效"))?;
        let id = Path::new(&file_name)
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| AppError::invalid("Profile 文件名无效"))?;
        let path = ConfigDocumentPath::Profile(id.to_owned());
        if path
            .relative_path()
            .map_err(|_| AppError::invalid("Profile ID 无效"))?
            != PathBuf::from("profiles").join(&file_name)
        {
            return Err(AppError::invalid("Profile 文件名与 ID 不一致"));
        }
        let mut source =
            zeroize::Zeroizing::new(fs::read(entry.path()).map_err(|source| {
                AppError::infrastructure_with_source("读取 Profile 失败", source)
            })?);
        if source.len() > 16 * 1024 * 1024 {
            return Err(AppError::invalid("Profile 文件超出迁移限制"));
        }
        let text = std::str::from_utf8(source.as_slice())
            .map_err(|_| AppError::invalid("Profile 不是有效 UTF-8"))?;
        let mut profile: LegacyProfile =
            toml::from_str(text).map_err(|_| AppError::invalid("Profile 结构无效"))?;
        if profile.id != id {
            return Err(AppError::invalid("Profile 文件名与 ID 不一致"));
        }
        let mut template = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| AppError::invalid("Profile 结构无效"))?;
        template.remove("sidebar_group_id");
        configuration::validate_current_configuration_document(&path, &template.to_string())?;
        profile.source = source.to_vec();
        profiles.push(profile);
        source.zeroize();
    }
    Ok(profiles)
}

/// 以规范化源根身份区分不同导入来源，相同来源的重复导入产生同一摘要。
fn source_identity(root: &Path) -> AppResult<String> {
    let canonical = fs::canonicalize(root)
        .map_err(|source| AppError::infrastructure_with_source("确认迁移来源失败", source))?;
    let path = canonical.to_string_lossy();
    let stable = if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.into_owned()
    };
    Ok(journal::sha256(stable.as_bytes()))
}

/// 来源身份与 Profile ID 均作为有界字段参与摘要，避免不同来源同名模板被错误合并。
fn migration_origin(source_identity: &str, profile_id: &str) -> String {
    journal::sha256(format!("{source_identity}\0{profile_id}").as_bytes())
}

/// 旧分组必须已在目标根中存在，无法确定归属时阻止静默丢失关系。
fn group_ids(root: &Path) -> AppResult<BTreeSet<String>> {
    let directory = root.join("hosts");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        _ => return Err(AppError::conflict("主机分组目录不可安全读取")),
    }
    let path = root.join("hosts").join("groups.toml");
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= 16 * 1024 * 1024 => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        _ => return Err(AppError::conflict("主机分组文件不可安全读取")),
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取主机分组失败",
                source,
            ));
        }
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| AppError::invalid("主机分组无效"))?;
    configuration::validate_current_configuration_document(&ConfigDocumentPath::HostGroups, text)?;
    let value: toml::Value = toml::from_str(text).map_err(|_| AppError::invalid("主机分组无效"))?;
    Ok(value
        .get("groups")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| {
            group
                .get("id")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .collect())
}

/// Windows 文件名大小写不敏感，因此即使在其他平台测试也采用相同的冲突规则。
fn existing_host_paths(root: &Path) -> AppResult<BTreeMap<String, PathBuf>> {
    let directory = root.join("hosts");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        _ => return Err(AppError::conflict("主机目录不可安全读取")),
    }
    let mut paths = BTreeMap::new();
    for entry in fs::read_dir(directory).map_err(|_| AppError::conflict("主机目录不可读取"))?
    {
        let entry = entry.map_err(|_| AppError::conflict("主机目录不可读取"))?;
        let path = entry.path();
        if path
            .extension()
            .and_then(OsStr::to_str)
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("toml"))
        {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| AppError::invalid("主机文件名无效"))?;
        if paths
            .insert(id.to_ascii_lowercase(), path.clone())
            .is_some()
        {
            return Err(AppError::conflict("主机 ID 存在大小写冲突"));
        }
    }
    Ok(paths)
}

/// 仅在确切来源匹配且是普通文件时复用已有主机，其他占用交给确定性后缀避让。
fn read_matching_host(path: &Path, origin: &str) -> AppResult<Option<ExistingLocalHost>> {
    let metadata = fs::symlink_metadata(path).map_err(|_| AppError::conflict("主机文件已变化"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    let source = fs::read_to_string(path).map_err(|_| AppError::conflict("主机文件不可读取"))?;
    let Ok(host) = toml::from_str::<ExistingLocalHost>(&source) else {
        return Ok(None);
    };
    if host.protocol == "local" && host.migration_origin.as_deref() == Some(origin) {
        Ok(Some(host))
    } else {
        Ok(None)
    }
}

/// 二次导入只接受尚未更改的快照；环境值以 Vault 解密结果比较，不写入日志。
fn host_matches_profile(
    root: &Path,
    host: &ExistingLocalHost,
    profile: &LegacyProfile,
    normalized_vault: Option<&[u8]>,
) -> AppResult<bool> {
    if host.name != profile.name
        || host.group_id != profile.sidebar_group_id
        || host.sort_order != profile.sort_order
        || host.created_at != profile.created_at
        || host.updated_at != profile.updated_at
        || host.local.shell != profile.shell
        || host.local.args != profile.args
        || host.local.cwd != profile.cwd
    {
        return Ok(false);
    }
    profile_migration::profile_environment_matches(
        root,
        &host.id,
        host.local.environment_ref.as_deref(),
        &profile.env,
        normalized_vault,
    )
    .map_err(|source| AppError::infrastructure_with_source("核对本地主机环境失败", source))
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_profile_hosts_unit.rs"]
mod tests;
