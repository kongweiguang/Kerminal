//! Git 状态、暂存、拉取和推送共用的本地文件保护策略。
//!
//! @author kongweiguang

use std::fs;
use std::io;
use std::path::Path;

use kerminal_application::{AppError, AppResult};

/// 保留既有本地状态规则，并忽略格式标记与迁移恢复材料，避免同步本机事务。
pub(super) const REQUIRED_IGNORE_RULES: &[&str] = &[
    "secrets/vault-key.toml",
    "secrets/vault-key.toml.bak.*",
    "secrets/vault.toml.bak.*",
    "secrets/.vault-transaction/",
    ".storage-transactions/",
    "backups/",
    ".storage.lock",
    "storage-manifest.toml",
    "logs/",
    "cache/",
    "temp/",
    "agents/sessions/",
    "workspace/session.json",
    "data/command.sqlite",
    "data/port-forwards/sessions.json",
    ".kerminal-format.toml",
    "kerminal-config.md",
    ".kerminal-migration/",
];

/// 即使用户显式 force-add，本地格式标记和迁移目录仍由路径策略保护。
/// 路径比较忽略大小写，并统一 Windows Git 输出中的分隔符。
pub(super) fn is_protected_path(path: &[u8]) -> bool {
    let mut normalized = String::from_utf8_lossy(path).replace('\\', "/");
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_owned();
    }
    let normalized = normalized.trim_start_matches('/').to_ascii_lowercase();

    const EXACT: &[&str] = &[
        "secrets/vault-key.toml",
        ".storage.lock",
        "storage-manifest.toml",
        "workspace/session.json",
        "data/command.sqlite",
        "data/port-forwards/sessions.json",
        ".kerminal-format.toml",
        "kerminal-config.md",
    ];
    if EXACT.contains(&normalized.as_str()) {
        return true;
    }

    const PREFIXES: &[&str] = &[
        "secrets/vault-key.toml.bak.",
        "secrets/vault.toml.bak.",
        "secrets/.vault-transaction/",
        ".storage-transactions/",
        "backups/",
        "logs/",
        "cache/",
        "temp/",
        "agents/sessions/",
        ".kerminal-migration/",
    ];
    if PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix) || normalized == prefix.trim_end_matches('/'))
    {
        return true;
    }

    false
}

/// 只比较托管忽略规则，不解析或重写用户自定义的忽略表达式。
///
/// @author kongweiguang
pub(super) fn missing_ignore_rules(root: &Path) -> io::Result<Vec<String>> {
    let path = root.join(".gitignore");
    let source = match fs::read(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let existing: Vec<&[u8]> = source
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).trim_ascii())
        .collect();
    Ok(REQUIRED_IGNORE_RULES
        .iter()
        .filter(|rule| !existing.contains(&rule.as_bytes()))
        .map(|rule| (*rule).to_owned())
        .collect())
}

/// 持锁快照本地仓库前拒绝进行中的片段导入，避免 Git 读写其部分提交状态。
pub(super) fn ensure_no_pending_snippet_import(root: &Path) -> AppResult<()> {
    let Some(root_metadata) = metadata_if_present(root)? else {
        return Ok(());
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(AppError::outcome_unknown(
            "配置目录状态不安全，Git 同步本地快照已暂停",
        ));
    }

    let transaction_root = root.join(".storage-transactions");
    let import_directory = transaction_root.join("snippet-import");
    for directory in [&transaction_root, &import_directory] {
        let Some(metadata) = metadata_if_present(directory)? else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::outcome_unknown(
                "片段导入事务目录状态不安全，Git 同步本地快照已暂停",
            ));
        }
    }

    let marker = import_directory.join("active.json");
    match metadata_if_present(&marker)? {
        Some(_) => Err(AppError::outcome_unknown(
            "片段导入恢复记录仍存在，Git 同步本地快照已暂停",
        )),
        None => Ok(()),
    }
}

/// 读取路径元数据而不跟随链接；检查失败时保守保留受保护仓库状态。
fn metadata_if_present(path: &Path) -> AppResult<Option<std::fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::outcome_unknown_with_source(
            "检查片段导入保护路径失败，Git 同步本地快照已暂停",
            error,
        )),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/configuration_sync_snippet_import_gate.rs"]
mod snippet_import_gate_tests;
