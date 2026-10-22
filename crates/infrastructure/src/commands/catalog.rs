//! 片段与工作流共享的安全文件操作和 revision 规则。
//!
//! @author kongweiguang

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use kerminal_application::{AppError, AppResult};
use kerminal_domain::preferences::ConfigRevision;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::{
    AtomicCommitError, CommitMode, StorageCoordinator, StorageLock, commit_staged_file,
};

/// 片段与工作流共享配置根、互斥锁和只读恢复门禁。
#[derive(Clone)]
pub struct FileCommandCatalogRepository {
    pub(super) storage: StorageCoordinator,
    pub(super) writable: bool,
}

impl FileCommandCatalogRepository {
    /// 复用应用级存储协调器，恢复态在进程启动时固定而不由单个仓储猜测。
    pub fn new(storage: StorageCoordinator, writable: bool) -> Self {
        Self { storage, writable }
    }

    /// 在同步磁盘访问前取得同一 `.storage.lock`，防止本应用其它配置仓储并发替换。
    pub(super) fn lock(&self) -> AppResult<StorageLock<'_>> {
        self.storage
            .lock()
            .map_err(|source| AppError::infrastructure_with_source("无法取得配置存储锁", source))
    }

    /// 只读恢复态阻断片段和工作流写入，保留原始文件供用户恢复。
    pub(super) fn ensure_writable(&self) -> AppResult<()> {
        if self.writable {
            Ok(())
        } else {
            Err(AppError::conflict("配置处于只读恢复状态，命令目录不能修改"))
        }
    }

    /// 仅允许固定目录下的 ASCII 文件 ID，避免分隔符、路径段和 Unicode 碰撞。
    pub(super) fn document_path(&self, directory: &str, id: &str) -> AppResult<PathBuf> {
        if id.is_empty()
            || id.len() > 160
            || id == "."
            || id == ".."
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(AppError::invalid("命令目录标识无效"));
        }
        Ok(self
            .storage
            .root()
            .join(directory)
            .join(format!("{id}.toml")))
    }

    /// 列出指定目录中的普通 TOML 文件，不跟随目录或文档符号链接。
    pub(super) fn list_paths(&self, directory: &str) -> AppResult<Vec<PathBuf>> {
        let directory_path = self.storage.root().join(directory);
        match fs::symlink_metadata(&directory_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_failure("读取命令目录失败", error)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::conflict("命令目录不是普通目录，已阻止读取"));
            }
            Ok(_) => {}
        }
        let entries = fs::read_dir(&directory_path)
            .map_err(|source| io_failure("读取命令目录失败", source))?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| io_failure("枚举命令目录失败", source))?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "toml") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| io_failure("读取命令目录项失败", source))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(AppError::conflict(
                    "命令目录包含不安全的 TOML 项，已阻止读取",
                ));
            }
            paths.push(path);
        }
        paths.sort();
        Ok(paths)
    }

    /// 读取普通 UTF-8 文档并绑定原始字节 revision，解析失败不会触碰原文件。
    pub(super) fn read_document(&self, path: &Path) -> AppResult<Option<(String, ConfigRevision)>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_failure("读取命令文档失败", error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::conflict("命令文档不是普通文件，已阻止读取"));
        }
        let bytes = fs::read(path).map_err(|source| io_failure("读取命令文档失败", source))?;
        let revision = revision(&bytes);
        let text = String::from_utf8(bytes)
            .map_err(|source| AppError::infrastructure_with_source("命令文档不是 UTF-8", source))?;
        Ok(Some((text, revision)))
    }

    /// 按原始内容 revision 原子创建或替换文档；未知提交结果必须回读目标确认。
    pub(super) fn write_document(
        &self,
        path: &Path,
        source: &str,
        expected_revision: Option<&ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        self.ensure_writable()?;
        let parent = path
            .parent()
            .ok_or_else(|| AppError::invalid("命令文档路径无效"))?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::conflict("命令目录不是普通目录，已阻止写入"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(parent).map_err(|source| io_failure("创建命令目录失败", source))?;
            }
            Err(error) => return Err(io_failure("检查命令目录失败", error)),
        }
        let latest = self.read_document(path)?;
        let mode = match (expected_revision, latest.as_ref()) {
            (None, None) => CommitMode::CreateNew,
            (Some(expected), Some((_, actual))) if expected == actual => {
                CommitMode::ReplaceExisting
            }
            _ => {
                return Err(AppError::conflict(
                    "命令文档已被其它操作修改，请重新读取后保存",
                ));
            }
        };

        let staging = parent.join(format!(".command-{}.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|source| io_failure("创建命令文档暂存文件失败", source))?;
        file.write_all(source.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| {
                let _ = fs::remove_file(&staging);
                io_failure("写入命令文档暂存文件失败", source)
            })?;
        drop(file);

        match commit_staged_file(&staging, path, mode) {
            Ok(()) => Ok(revision(source.as_bytes())),
            Err(AtomicCommitError::NotCommitted(source)) => {
                let _ = fs::remove_file(&staging);
                Err(io_failure("原子保存命令文档失败", source))
            }
            Err(AtomicCommitError::CommitUnknown(error)) => {
                let committed = self
                    .read_document(path)?
                    .is_some_and(|(actual, _)| actual == source);
                if committed {
                    Ok(revision(source.as_bytes()))
                } else {
                    Err(AppError::infrastructure_with_source(
                        "命令文档提交结果未知，请重新读取",
                        error,
                    ))
                }
            }
        }
    }

    /// 按内容 revision 删除用户文档；Unix 目录同步失败时回读并保留结果未知，Windows 不尝试不支持的目录 fsync。
    pub(super) fn delete_document(
        &self,
        path: &Path,
        expected_revision: Option<&ConfigRevision>,
    ) -> AppResult<bool> {
        self.ensure_writable()?;
        let Some((_, latest)) = self.read_document(path)? else {
            return Ok(false);
        };
        if expected_revision.is_some_and(|expected| expected != &latest) {
            return Err(AppError::conflict(
                "命令文档已被其它操作修改，请重新读取后删除",
            ));
        }
        if let Some((_, confirmed)) = self.read_document(path)? {
            if confirmed != latest {
                return Err(AppError::conflict("命令文档已被外部编辑，请重新读取后删除"));
            }
        } else {
            return Ok(false);
        }
        fs::remove_file(path).map_err(|source| io_failure("删除命令文档失败", source))?;
        #[cfg(unix)]
        if let Some(parent) = path.parent()
            && let Err(source) = fs::File::open(parent).and_then(|directory| directory.sync_all())
        {
            let target_is_gone = matches!(
                fs::symlink_metadata(path),
                Err(ref error) if error.kind() == io::ErrorKind::NotFound
            );
            let context = if target_is_gone {
                "命令文档已删除但目录同步失败，持久性未知，请重新读取确认"
            } else {
                "删除命令文档提交结果未知，请重新读取确认"
            };
            return Err(AppError::outcome_unknown_with_source(context, source));
        }
        Ok(true)
    }
}

/// 把内部 I/O 失败分类为稳定的基础设施错误，并保留 OS cause 供诊断。
pub(super) fn io_failure(context: &'static str, source: io::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}

/// 对原始 UTF-8 字节计算配置仓储兼容的 SHA-256 revision。
fn revision(bytes: &[u8]) -> ConfigRevision {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    ConfigRevision::from_sha256(hex)
}

/// 判断目录项扩展名时保持原始 TOML 文件名，不对 ID 做 trim 或大小写归一化。
pub(super) fn document_id(path: &Path) -> AppResult<String> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| AppError::invalid("命令文件名不是有效 UTF-8"))?;
    if id.is_empty()
        || id == "."
        || id == ".."
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AppError::invalid("命令文件名包含无效标识"));
    }
    Ok(id.to_owned())
}
