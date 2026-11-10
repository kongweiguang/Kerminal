//! OpenSSH known_hosts 持久化适配器，复用应用存储锁与原子替换边界。
//!
//! @author kongweiguang

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{HostKeyIdentity, HostKeyStatus, KnownHostsRepository},
};
use kerminal_domain::connections::HostEndpoint;
use russh::keys::{HashAlg, known_hosts, ssh_key::PublicKey};

use crate::storage::{
    AtomicCommitError, CommitMode, StorageCoordinator, StorageLockError, commit_staged_file,
};

const KNOWN_HOSTS_FILE: &str = "known_hosts";
const MAX_KNOWN_HOSTS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_STAGED_FILE_ATTEMPTS: usize = 16;
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// 将 OpenSSH known_hosts 文件绑定到统一用户数据目录；只读模式供沙箱或受限 host 使用。
pub struct KnownHostsStore {
    storage: StorageCoordinator,
    writable: bool,
}

impl KnownHostsStore {
    /// 由 composition root 注入共用的存储协调器，known_hosts 固定使用其根目录下的标准文件名。
    pub fn new(storage: StorageCoordinator, writable: bool) -> Self {
        Self { storage, writable }
    }
}

#[async_trait]
impl KnownHostsRepository for KnownHostsStore {
    /// 在共享存储锁保护下读取并分类 key；russh 负责 OpenSSH 主机名与哈希匹配。
    async fn inspect(
        &self,
        endpoint: &HostEndpoint,
        identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        let endpoint = endpoint.clone();
        let identity = identity.clone();
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            let _storage_lock = storage.lock().map_err(storage_lock_error)?;
            let path = storage.root().join(KNOWN_HOSTS_FILE);
            // 分类本身交给 russh 的 matcher，但仍先按共享读取边界拒绝符号链接和超大文件。
            read_known_hosts(&path)?;
            classify_key(&endpoint, &identity, &path)
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("SSH known_hosts 读取任务失败", error)
        })?
    }

    /// 幂等 upsert 观察到的公钥：已知直接成功，未知追加，同算法变化时原子替换该端点的旧记录。
    ///
    /// 连接层已默认接受主机密钥，known_hosts 只承担后台记录职责；调用方把读取或写入
    /// 失败降级为 warning，不会阻断握手或认证。
    /// 已知判定必须先于 writable 判断：只读数据根中已保存的主机仍应允许连接，
    /// 否则恢复态或受限运行环境会因无法写入而误拒已受信主机。
    async fn record(&self, endpoint: &HostEndpoint, identity: &HostKeyIdentity) -> AppResult<()> {
        let endpoint = endpoint.clone();
        let identity = identity.clone();
        let storage = self.storage.clone();
        let writable = self.writable;
        tokio::task::spawn_blocking(move || {
            let _storage_lock = storage.lock().map_err(storage_lock_error)?;
            let path = storage.root().join(KNOWN_HOSTS_FILE);
            let existing = read_known_hosts(&path)?;
            match classify_key(&endpoint, &identity, &path)? {
                HostKeyStatus::Known => Ok(()),
                HostKeyStatus::Changed {
                    previous_fingerprint_sha256,
                } => {
                    if !writable {
                        return Err(AppError::unsupported(
                            "SSH host key rotation storage is read-only",
                        ));
                    }
                    // 并发编辑只让本次后台替换失败；调用方记录 warning，
                    // 连接仍然完成，原 known_hosts 内容保持不变。
                    let updated = replace_known_host_contents(
                        &endpoint,
                        &previous_fingerprint_sha256,
                        &identity,
                        &path,
                        &existing,
                    )?;
                    if updated == existing {
                        return Ok(());
                    }

                    let staging_path = write_staging_file(&path, &updated)?;
                    let stage_validation = validate_replaced_host_key(
                        &endpoint,
                        &previous_fingerprint_sha256,
                        &identity,
                        &staging_path,
                    );
                    if let Err(error) = stage_validation {
                        let _ = fs::remove_file(&staging_path);
                        return Err(error);
                    }
                    commit_staged_known_hosts_replacement(&path, &staging_path, &existing, &updated)
                }
                HostKeyStatus::Unknown => {
                    if !writable {
                        return Err(AppError::unsupported(
                            "SSH host key record storage is read-only",
                        ));
                    }
                    let updated = append_known_host(&endpoint, &identity, &existing)?;
                    let staging_path = write_staging_file(&path, &updated)?;
                    if path.exists() {
                        return commit_staged_known_hosts_replacement(
                            &path,
                            &staging_path,
                            &existing,
                            &updated,
                        );
                    }

                    let commit = commit_staged_file(&staging_path, &path, CommitMode::CreateNew);
                    if let Err(error) = commit {
                        let _ = fs::remove_file(&staging_path);
                        return resolve_commit_error(&path, &updated, error);
                    }
                    Ok(())
                }
            }
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("SSH known_hosts 写入任务失败", error)
        })?
    }
}

/// 校验 OpenSSH 公钥文本与独立算法/指纹字段一致，避免上层拼造信任记录。
fn parse_identity(identity: &HostKeyIdentity) -> AppResult<PublicKey> {
    let key = PublicKey::from_openssh(&identity.public_key_openssh)
        .map_err(|error| AppError::infrastructure_with_source("SSH 主机公钥格式无效", error))?;
    if key.algorithm().to_string() != identity.algorithm
        || key.fingerprint(HashAlg::Sha256).to_string() != identity.fingerprint_sha256
    {
        return Err(AppError::invalid("SSH 主机公钥字段与指纹不一致"));
    }
    Ok(key)
}

/// 按 endpoint 的已有公钥分类；只有与新 key 同算法的记录才能作为轮换前值。
///
/// 不同算法（如同时保存的 rsa 与 ed25519）是 OpenSSH 的正常多密钥布局，
/// 不能把另一算法的 key 误当成“变化”，否则自动记录会删掉仍有效的旧算法记录。
/// @revoked 行只表示历史撤销事实，不再参与连接判定或阻止重新记录。
fn classify_key(
    endpoint: &HostEndpoint,
    identity: &HostKeyIdentity,
    path: &Path,
) -> AppResult<HostKeyStatus> {
    let key = parse_identity(identity)?;
    let records = matching_keys(endpoint, path)?;
    if records.iter().any(|recorded| recorded == &key) {
        return Ok(HostKeyStatus::Known);
    }
    let previous = records
        .iter()
        .find(|recorded| recorded.algorithm() == key.algorithm())
        .map(|recorded| recorded.fingerprint(HashAlg::Sha256).to_string());
    match previous {
        Some(previous_fingerprint_sha256) => Ok(HostKeyStatus::Changed {
            previous_fingerprint_sha256,
        }),
        None => Ok(HostKeyStatus::Unknown),
    }
}

/// 用 russh 的 OpenSSH parser 匹配明文或哈希主机名，避免另建指纹数据库失去旧记录兼容。
fn matching_keys(endpoint: &HostEndpoint, path: &Path) -> AppResult<Vec<PublicKey>> {
    known_hosts::known_host_keys_path(&endpoint.host, endpoint.port, path)
        .map(|records| records.into_iter().map(|(_, key)| key).collect())
        .map_err(|error| AppError::infrastructure_with_source("SSH known_hosts 解析失败", error))
}

/// russh 解析器跳过 `#` 注释时不递增行号，直接用它删行会在注释后错位。
/// 在同一份读取字节上重建“parser 行号 → 真实行号”，只跳过注释行且保留真实行号。
fn real_line_numbers(text: &str) -> Vec<usize> {
    text.split_inclusive('\n')
        .enumerate()
        .filter(|(_, segment)| segment.as_bytes().first() != Some(&b'#'))
        .map(|(index, _)| index + 1)
        .collect()
}

/// 构造与旧 Kerminal 一致的 host 或 `[host]:port` 字段，并拒绝会变成多列的地址。
fn host_pattern(endpoint: &HostEndpoint) -> AppResult<String> {
    if endpoint.host.trim().is_empty()
        || endpoint.host.chars().any(|character| {
            character.is_control() || character.is_whitespace() || matches!(character, ',' | '*')
        })
    {
        return Err(AppError::invalid("SSH known_hosts host field is invalid"));
    }
    if endpoint.port == 22 {
        Ok(endpoint.host.clone())
    } else {
        Ok(format!("[{}]:{}", endpoint.host, endpoint.port))
    }
}

/// 只为 Unknown 写入完整的 OpenSSH 公钥行，不保存 fingerprint-only 的无效条目。
fn append_known_host(
    endpoint: &HostEndpoint,
    identity: &HostKeyIdentity,
    existing: &[u8],
) -> AppResult<Vec<u8>> {
    let key = parse_identity(identity)?;
    let key_line = key
        .to_openssh()
        .map_err(|error| AppError::infrastructure_with_source("SSH 主机公钥编码失败", error))?;
    let pattern = host_pattern(endpoint)?;
    let mut output = Vec::with_capacity(existing.len() + pattern.len() + key_line.len() + 2);
    output.extend_from_slice(existing);
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        output.push(b'\n');
    }
    output.extend_from_slice(pattern.as_bytes());
    output.push(b' ');
    output.extend_from_slice(key_line.as_bytes());
    output.push(b'\n');
    if output.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(AppError::invalid("SSH known_hosts exceeds its size limit"));
    }
    Ok(output)
}

/// 替换当前 endpoint 上所有与新 key 同算法的旧记录，并保留其它算法和其它主机行。
///
/// 只删除同算法旧行：OpenSSH 允许同一 endpoint 同时保存不同算法的 key，
/// 轮换 ed25519 时删除 rsa 会让仍有效的备选算法失效。
fn replace_known_host_contents(
    endpoint: &HostEndpoint,
    expected_previous_fingerprint_sha256: &str,
    identity: &HostKeyIdentity,
    path: &Path,
    existing: &[u8],
) -> AppResult<Vec<u8>> {
    let new_key = parse_identity(identity)?;
    if expected_previous_fingerprint_sha256.is_empty()
        || expected_previous_fingerprint_sha256 == identity.fingerprint_sha256
    {
        return Err(AppError::invalid(
            "SSH host key rotation fingerprints are invalid",
        ));
    }
    let text = std::str::from_utf8(existing).map_err(|error| {
        AppError::infrastructure_with_source("SSH known_hosts encoding is invalid", error)
    })?;
    let records = known_hosts::known_host_keys_path(&endpoint.host, endpoint.port, path).map_err(
        |error| AppError::infrastructure_with_source("SSH known_hosts parsing failed", error),
    )?;
    let parser_lines = real_line_numbers(text);
    let mut old_lines = BTreeSet::new();
    let mut expected_previous_matched = false;
    let mut new_key_is_present = false;
    for (parser_line, recorded) in records {
        if recorded == new_key {
            new_key_is_present = true;
        }
        if recorded.algorithm() != new_key.algorithm() {
            continue;
        }
        if recorded.fingerprint(HashAlg::Sha256).to_string() == expected_previous_fingerprint_sha256
        {
            expected_previous_matched = true;
        }
        // parser 行号从 1 开始并跳过注释；映射回真实行号后才能删除正确记录。
        let real_line = parser_lines
            .get(parser_line.saturating_sub(1))
            .copied()
            .unwrap_or(parser_line);
        old_lines.insert(real_line);
    }
    if old_lines.is_empty() {
        if new_key_is_present {
            return Ok(existing.to_vec());
        }
        return Err(AppError::conflict(
            "SSH host key changed again; the previously recorded key no longer matches",
        ));
    }
    if !expected_previous_matched {
        return Err(AppError::conflict(
            "SSH host key changed while the replacement was prepared",
        ));
    }

    let endpoint_pattern = host_pattern(endpoint)?;
    let mut updated = Vec::with_capacity(existing.len() + 128);
    for (index, segment) in text.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        if !old_lines.contains(&line_number) {
            updated.extend_from_slice(segment.as_bytes());
            continue;
        }
        let has_lf = segment.ends_with('\n');
        let without_lf = segment.strip_suffix('\n').unwrap_or(segment);
        let has_crlf = without_lf.ends_with('\r');
        let line = without_lf.strip_suffix('\r').unwrap_or(without_lf);
        if let Some(rewritten) = remove_endpoint_from_known_host_line(line, &endpoint_pattern)? {
            updated.extend_from_slice(rewritten.as_bytes());
            if has_crlf {
                updated.extend_from_slice(b"\r\n");
            } else if has_lf {
                updated.push(b'\n');
            }
        }
    }
    if new_key_is_present {
        Ok(updated)
    } else {
        append_known_host(endpoint, identity, &updated)
    }
}

/// hashed endpoint 只能删整条哈希记录；明文逗号列表则仅移除当前精确 host:port。
fn remove_endpoint_from_known_host_line(
    line: &str,
    endpoint_pattern: &str,
) -> AppResult<Option<String>> {
    let separator = line
        .find(char::is_whitespace)
        .ok_or_else(|| AppError::conflict("SSH known_hosts record cannot be safely edited"))?;
    let host_field = &line[..separator];
    if host_field.starts_with("|1|") {
        if host_field.contains(',') {
            return Err(AppError::conflict(
                "SSH hashed known_hosts record contains multiple hosts and cannot be safely replaced",
            ));
        }
        return Ok(None);
    }

    let retained = host_field
        .split(',')
        .filter(|candidate| *candidate != endpoint_pattern)
        .collect::<Vec<_>>();
    if retained.len() == host_field.split(',').count() {
        return Err(AppError::conflict(
            "SSH known_hosts record no longer matches the confirmed endpoint",
        ));
    }
    if retained.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "{}{}",
            retained.join(","),
            &line[separator..]
        )))
    }
}

/// 提交前回读 staging 文件，确认新 key 可匹配且旧指纹已从该 endpoint 移除。
fn validate_replaced_host_key(
    endpoint: &HostEndpoint,
    previous_fingerprint: &str,
    identity: &HostKeyIdentity,
    staging_path: &Path,
) -> AppResult<()> {
    let new_key = parse_identity(identity)?;
    let records = known_hosts::known_host_keys_path(&endpoint.host, endpoint.port, staging_path)
        .map_err(|error| {
            AppError::infrastructure_with_source("SSH staged known_hosts parsing failed", error)
        })?;
    let has_new_key = records.iter().any(|(_, recorded)| recorded == &new_key);
    let still_has_old_key = records.iter().any(|(_, recorded)| {
        recorded.fingerprint(HashAlg::Sha256).to_string() == previous_fingerprint
    });
    if has_new_key && !still_has_old_key {
        Ok(())
    } else {
        Err(AppError::conflict(
            "SSH host key replacement did not produce the confirmed endpoint state",
        ))
    }
}

/// 在发布 staging 前复核完整原文件，拒绝覆盖 stage 期间可见的外部编辑。
///
/// 同一 `.storage.lock` 的进程由 `StorageCoordinator` 串行化；提交器没有跨进程原子 CAS，
/// 因此该检查只能缩小非合作写者在检查后、rename 前修改文件的竞态窗口。
fn commit_staged_known_hosts_replacement(
    target: &Path,
    staging_path: &Path,
    expected_original: &[u8],
    updated: &[u8],
) -> AppResult<()> {
    let current = match read_known_hosts(target) {
        Ok(current) => current,
        Err(error) => {
            let _ = fs::remove_file(staging_path);
            return Err(error);
        }
    };
    if current != expected_original {
        let _ = fs::remove_file(staging_path);
        return Err(AppError::conflict(
            "SSH known_hosts changed while the confirmed replacement was staged; inspect it again before retrying",
        ));
    }

    if let Err(error) = commit_staged_file(staging_path, target, CommitMode::ReplaceExisting) {
        let _ = fs::remove_file(staging_path);
        return resolve_commit_error(target, updated, error);
    }
    Ok(())
}

/// 通过同目录 create_new staging 保证原文件保持完整，最终 rename 由共享原子提交器负责。
fn write_staging_file(destination: &Path, contents: &[u8]) -> AppResult<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::infrastructure("SSH known_hosts has no parent directory"))?;
    for _ in 0..MAX_STAGED_FILE_ATTEMPTS {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| AppError::infrastructure("SSH known_hosts filename is invalid"))?;
        let staging = parent.join(format!(
            ".{file_name}.{}.{}.staging",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&staging) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
                    let _ = fs::remove_file(&staging);
                    return Err(AppError::infrastructure_with_source(
                        "SSH known_hosts staging write failed",
                        error,
                    ));
                }
                return Ok(staging);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::infrastructure_with_source(
                    "SSH known_hosts staging create failed",
                    error,
                ));
            }
        }
    }
    Err(AppError::conflict("SSH known_hosts staging name collision"))
}

/// 在提交器报告未知结果时回读目标；字节完全匹配才把操作视为已完成。
fn resolve_commit_error(target: &Path, expected: &[u8], error: AtomicCommitError) -> AppResult<()> {
    match error {
        AtomicCommitError::NotCommitted(error) => Err(AppError::infrastructure_with_source(
            "SSH known_hosts atomic commit failed",
            error,
        )),
        AtomicCommitError::CommitUnknown(error) => {
            if read_known_hosts(target).ok().as_deref() == Some(expected) {
                Ok(())
            } else {
                Err(AppError::outcome_unknown_with_source(
                    "SSH known_hosts commit result requires readback",
                    error,
                ))
            }
        }
    }
}

/// 检查文件类型和 4 MiB 上限后有界读取，拒绝把损坏或异常文件当成空信任库。
fn read_known_hosts(path: &Path) -> AppResult<Vec<u8>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::infrastructure_with_source(
                "SSH known_hosts metadata read failed",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::infrastructure(
            "SSH known_hosts is not a regular file",
        ));
    }
    if metadata.len() > MAX_KNOWN_HOSTS_BYTES {
        return Err(AppError::invalid("SSH known_hosts exceeds its size limit"));
    }
    let file = File::open(path).map_err(|error| {
        AppError::infrastructure_with_source("SSH known_hosts open failed", error)
    })?;
    let mut content = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|error| {
            AppError::infrastructure_with_source("SSH known_hosts read failed", error)
        })?;
    if content.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(AppError::invalid("SSH known_hosts exceeds its size limit"));
    }
    Ok(content)
}

/// 仅返回固定存储错误上下文；锁路径和系统错误不进入公开消息。
fn storage_lock_error(error: StorageLockError) -> AppError {
    AppError::infrastructure_with_source("SSH known_hosts storage lock failed", error)
}

#[cfg(test)]
#[path = "../../tests/unit/known_hosts.rs"]
mod tests;
