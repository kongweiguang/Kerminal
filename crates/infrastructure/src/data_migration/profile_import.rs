//! 将明确选择的旧 Profile 导入当前契约，保留已有模板和分组。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs, io,
    path::{Component, Path, PathBuf},
};

use kerminal_application::{AppError, AppResult, data_migration::ProfileImportPreview};
use kerminal_domain::preferences::ConfigDocumentPath;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value, value};
use zeroize::Zeroizing;

use super::{FileEdit, profile_hosts};
use crate::configuration;

const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// 在源锁持有期间保留可恢复的用户原文；退出规划时清除环境变量字节。
struct SourceProfile {
    id: String,
    group_id: Option<String>,
    was_default: bool,
    source: Zeroizing<Vec<u8>>,
}

/// 只保留受限编辑和用户可见数量；配置与凭据正文不进入摘要。
pub(super) struct ProfileImportPlan {
    pub(super) preview: ProfileImportPreview,
    pub(super) edits: Vec<FileEdit>,
}

/// 只规划模板、分组、本地主机和密文变化；目标的格式标记由事务层作为同值哨兵。
pub(super) fn plan_profile_import(source: &Path, target: &Path) -> AppResult<ProfileImportPlan> {
    let profiles = source_profiles(source)?;
    if profiles.is_empty() {
        return Ok(ProfileImportPlan {
            preview: ProfileImportPreview {
                source_profiles: 0,
                new_profiles: 0,
                new_hosts: 0,
                groups_changed: false,
                vault_changed: false,
            },
            edits: Vec::new(),
        });
    }
    let required_groups = profiles
        .iter()
        .filter_map(|profile| profile.group_id.clone())
        .collect::<BTreeSet<_>>();
    let (group_edit, groups) = plan_groups(source, target, &required_groups)?;
    let mut vault_edits = Vec::new();
    let hosts = profile_hosts::plan_profile_hosts(source, target, &mut vault_edits, &groups)?;
    for profile in &profiles {
        let relative = ConfigDocumentPath::Profile(profile.id.clone())
            .relative_path()
            .map_err(|_| AppError::invalid("Profile ID 无效"))?;
        let current = Zeroizing::new(
            read_optional(source, &relative)?
                .ok_or_else(|| AppError::conflict("来源 Profile 已消失"))?,
        );
        if current.as_slice() != profile.source.as_slice() {
            return Err(AppError::conflict("来源 Profile 在导入规划期间发生变化"));
        }
    }
    let mut edits = plan_templates(target, &profiles)?;
    let preview = ProfileImportPreview {
        source_profiles: profiles.len(),
        new_profiles: edits.len(),
        new_hosts: hosts.edits.len(),
        groups_changed: group_edit.is_some(),
        vault_changed: !vault_edits.is_empty(),
    };
    edits.extend(group_edit);
    edits.extend(hosts.edits);
    edits.extend(vault_edits);
    Ok(ProfileImportPlan { preview, edits })
}

/// 目标已存在默认项时，新导入模板不抢占默认项；原文件语义不同则报告冲突。
fn plan_templates(root: &Path, profiles: &[SourceProfile]) -> AppResult<Vec<FileEdit>> {
    let current_default = current_default_profile(root)?;
    let mut new_profile_ids = BTreeSet::new();
    for profile in profiles {
        let relative = ConfigDocumentPath::Profile(profile.id.clone())
            .relative_path()
            .map_err(|_| AppError::invalid("Profile ID 无效"))?;
        if read_optional(root, &relative)?.is_none() {
            new_profile_ids.insert(profile.id.as_str());
        }
    }
    let default_id = if current_default.is_none() {
        profiles
            .iter()
            .find(|profile| profile.was_default && new_profile_ids.contains(profile.id.as_str()))
            .or_else(|| {
                profiles
                    .iter()
                    .find(|profile| new_profile_ids.contains(profile.id.as_str()))
            })
            .map(|profile| profile.id.as_str())
    } else {
        None
    };
    let mut edits = Vec::new();
    for profile in profiles {
        let path = ConfigDocumentPath::Profile(profile.id.clone());
        let relative = path
            .relative_path()
            .map_err(|_| AppError::invalid("Profile ID 无效"))?;
        let text = std::str::from_utf8(profile.source.as_slice())
            .map_err(|_| AppError::invalid("旧 Profile 无效"))?;
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|_| AppError::invalid("旧 Profile 无效"))?;
        document.remove("sidebar_group_id");
        let before = read_optional(root, &relative)?;
        if let Some(before) = before {
            let existing_text =
                std::str::from_utf8(&before).map_err(|_| AppError::invalid("现有 Profile 无效"))?;
            configuration::validate_current_configuration_document(&path, existing_text)?;
            let mut existing: toml::Value = toml::from_str(existing_text)
                .map_err(|_| AppError::invalid("现有 Profile 无效"))?;
            let mut incoming: toml::Value = toml::from_str(&document.to_string())
                .map_err(|_| AppError::invalid("旧 Profile 无效"))?;
            existing
                .as_table_mut()
                .ok_or_else(|| AppError::invalid("现有 Profile 无效"))?
                .remove("is_default");
            incoming
                .as_table_mut()
                .ok_or_else(|| AppError::invalid("旧 Profile 无效"))?
                .remove("is_default");
            if existing != incoming {
                return Err(AppError::conflict("同 ID Profile 已被修改，导入未覆盖"));
            }
            continue;
        }
        let is_default = default_id == Some(profile.id.as_str());
        document["is_default"] = value(is_default);
        let after = document.to_string();
        configuration::validate_current_configuration_document(&path, &after)?;
        edits.push(FileEdit {
            relative_path: relative,
            before: None,
            after: after.into_bytes(),
        });
    }
    Ok(edits)
}

/// 只枚举受管普通文件，并在解析前检查上限和文件名身份。
fn source_profiles(root: &Path) -> AppResult<Vec<SourceProfile>> {
    let directory = root.join("profiles");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err(AppError::conflict("导入来源 Profile 目录不安全")),
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|_| AppError::conflict("导入来源 Profile 目录不可读取"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AppError::conflict("导入来源 Profile 目录不可读取"))?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut profiles = Vec::new();
    for entry in entries {
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::invalid("Profile 文件名无效"))?;
        if Path::new(&file_name).extension() != Some(OsStr::new("toml")) {
            continue;
        }
        if profiles.len() >= 16_384 {
            return Err(AppError::invalid("Profile 数量超出导入限制"));
        }
        let id = Path::new(&file_name)
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| AppError::invalid("Profile 文件名无效"))?
            .to_owned();
        let path = ConfigDocumentPath::Profile(id.clone());
        let relative = path
            .relative_path()
            .map_err(|_| AppError::invalid("Profile ID 无效"))?;
        if relative != PathBuf::from("profiles").join(&file_name) {
            return Err(AppError::invalid("Profile 文件名与 ID 不一致"));
        }
        let source = Zeroizing::new(
            read_optional(root, &relative)?
                .ok_or_else(|| AppError::conflict("来源 Profile 已消失"))?,
        );
        let text = std::str::from_utf8(source.as_slice())
            .map_err(|_| AppError::invalid("旧 Profile 无效"))?;
        let mut document = text
            .parse::<DocumentMut>()
            .map_err(|_| AppError::invalid("旧 Profile 无效"))?;
        if document.get("id").and_then(Item::as_str) != Some(id.as_str()) {
            return Err(AppError::invalid("Profile 文件名与 ID 不一致"));
        }
        let group_id = match document.get("sidebar_group_id") {
            Some(item) => Some(
                item.as_str()
                    .ok_or_else(|| AppError::invalid("旧 Profile 分组无效"))?
                    .to_owned(),
            ),
            None => None,
        };
        let was_default = document
            .get("is_default")
            .and_then(Item::as_bool)
            .ok_or_else(|| AppError::invalid("旧 Profile 默认值无效"))?;
        document.remove("sidebar_group_id");
        configuration::validate_current_configuration_document(&path, &document.to_string())?;
        profiles.push(SourceProfile {
            id,
            group_id,
            was_default,
            source,
        });
    }
    Ok(profiles)
}

/// 已有默认项以目标根为准；多默认属于损坏状态，不能由导入偷偷修复。
fn current_default_profile(root: &Path) -> AppResult<Option<String>> {
    let directory = root.join("profiles");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => return Err(AppError::conflict("目标 Profile 目录不安全")),
    }
    let mut found = None;
    for entry in
        fs::read_dir(directory).map_err(|_| AppError::conflict("目标 Profile 目录不可读取"))?
    {
        let entry = entry.map_err(|_| AppError::conflict("目标 Profile 目录不可读取"))?;
        let file_name = entry.file_name();
        let Some(id) = Path::new(&file_name).file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if Path::new(&file_name).extension() != Some(OsStr::new("toml")) {
            continue;
        }
        let path = ConfigDocumentPath::Profile(id.to_owned());
        let relative = path
            .relative_path()
            .map_err(|_| AppError::invalid("目标 Profile ID 无效"))?;
        let bytes = read_optional(root, &relative)?
            .ok_or_else(|| AppError::conflict("目标 Profile 已消失"))?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| AppError::invalid("目标 Profile 无效"))?;
        configuration::validate_current_configuration_document(&path, text)?;
        let document: toml::Value =
            toml::from_str(text).map_err(|_| AppError::invalid("目标 Profile 无效"))?;
        if document.get("is_default").and_then(toml::Value::as_bool) == Some(true)
            && found.replace(id.to_owned()).is_some()
        {
            return Err(AppError::conflict("目标存在多个默认 Profile"));
        }
    }
    Ok(found)
}

/// 只合并导入主机实际引用的组；相同 ID 的不同组不会被覆盖或静默混用。
fn plan_groups(
    source: &Path,
    target: &Path,
    required: &BTreeSet<String>,
) -> AppResult<(Option<FileEdit>, BTreeSet<String>)> {
    if required.is_empty() {
        return Ok((None, BTreeSet::new()));
    }
    let relative = PathBuf::from("hosts/groups.toml");
    let source_bytes =
        read_optional(source, &relative)?.ok_or_else(|| AppError::conflict("来源缺少主机分组"))?;
    let source_text =
        std::str::from_utf8(&source_bytes).map_err(|_| AppError::invalid("来源主机分组无效"))?;
    configuration::validate_current_configuration_document(
        &ConfigDocumentPath::HostGroups,
        source_text,
    )?;
    let source_doc = source_text
        .parse::<DocumentMut>()
        .map_err(|_| AppError::invalid("来源主机分组无效"))?;
    let source_rows = group_tables(&source_doc)?;
    let mut source_by_id = BTreeMap::new();
    for table in source_rows {
        let id = table
            .get("id")
            .and_then(Item::as_str)
            .ok_or_else(|| AppError::invalid("来源主机分组无效"))?
            .to_owned();
        if source_by_id.insert(id, table).is_some() {
            return Err(AppError::conflict("来源主机分组 ID 重复"));
        }
    }
    let before = read_optional(target, &relative)?;
    let mut target_doc = match before.as_ref() {
        Some(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| AppError::invalid("目标主机分组无效"))?;
            configuration::validate_current_configuration_document(
                &ConfigDocumentPath::HostGroups,
                text,
            )?;
            text.parse::<DocumentMut>()
                .map_err(|_| AppError::invalid("目标主机分组无效"))?
        }
        None => "schema_version = 1\n"
            .parse::<DocumentMut>()
            .map_err(|_| AppError::invalid("主机分组无法初始化"))?,
    };
    let mut target_by_id = BTreeMap::new();
    for table in group_tables(&target_doc)? {
        let id = table
            .get("id")
            .and_then(Item::as_str)
            .ok_or_else(|| AppError::invalid("目标主机分组无效"))?
            .to_owned();
        if target_by_id.insert(id, table).is_some() {
            return Err(AppError::conflict("目标主机分组 ID 重复"));
        }
    }
    let mut additions = Vec::new();
    for id in required {
        let from = source_by_id
            .get(id)
            .ok_or_else(|| AppError::conflict("来源主机分组不存在"))?;
        if let Some(existing) = target_by_id.get(id) {
            let old: toml::Value = toml::from_str(&from.to_string())
                .map_err(|_| AppError::invalid("来源主机分组无效"))?;
            let current: toml::Value = toml::from_str(&existing.to_string())
                .map_err(|_| AppError::invalid("目标主机分组无效"))?;
            if old != current {
                return Err(AppError::conflict("同 ID 主机分组不一致，导入未覆盖"));
            }
        } else {
            additions.push(from.clone());
        }
    }
    if additions.is_empty() {
        return Ok((None, required.clone()));
    }
    if target_doc.get("groups").is_none() {
        target_doc["groups"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let groups = target_doc
        .get_mut("groups")
        .ok_or_else(|| AppError::invalid("目标主机分组无效"))?;
    for table in additions {
        match &mut *groups {
            Item::ArrayOfTables(tables) => tables.push(table),
            Item::Value(Value::Array(array)) => {
                array.push(Value::InlineTable(table.into_inline_table()))
            }
            _ => return Err(AppError::invalid("目标主机分组无效")),
        }
    }
    let after = target_doc.to_string();
    configuration::validate_current_configuration_document(
        &ConfigDocumentPath::HostGroups,
        &after,
    )?;
    Ok((
        Some(FileEdit {
            relative_path: relative,
            before,
            after: after.into_bytes(),
        }),
        required.clone(),
    ))
}

/// 同时接受旧文档中的数组表和 inline 数组，保留每条分组的扩展字段。
fn group_tables(document: &DocumentMut) -> AppResult<Vec<Table>> {
    match document.get("groups") {
        None => Ok(Vec::new()),
        Some(Item::ArrayOfTables(tables)) => Ok(tables.iter().cloned().collect()),
        Some(Item::Value(Value::Array(array))) => array
            .iter()
            .map(|value| {
                value
                    .as_inline_table()
                    .cloned()
                    .map(|table| table.into_table())
                    .ok_or_else(|| AppError::invalid("主机分组行无效"))
            })
            .collect(),
        _ => Err(AppError::invalid("主机分组列表无效")),
    }
}

/// 逐段拒绝符号链接并限制读取大小；不存在的父目录只表示目标尚未创建。
fn read_optional(root: &Path, relative: &Path) -> AppResult<Option<Vec<u8>>> {
    let mut current = root.to_path_buf();
    let parts = relative.components().collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(AppError::invalid("导入路径无效"));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::conflict("导入路径不能包含链接"));
            }
            Ok(metadata)
                if index + 1 == parts.len()
                    && (!metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES) =>
            {
                return Err(AppError::conflict("导入文件无效或过大"));
            }
            Ok(metadata) if index + 1 < parts.len() && !metadata.is_dir() => {
                return Err(AppError::conflict("导入目录无效"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AppError::conflict("导入文件不可读取")),
        }
    }
    fs::read(current)
        .map(Some)
        .map_err(|source| AppError::infrastructure_with_source("读取导入文件失败", source))
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_profile_import_unit.rs"]
mod tests;
