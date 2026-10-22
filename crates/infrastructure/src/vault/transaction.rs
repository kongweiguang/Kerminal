//! vault/key 配对文件的可恢复写事务。
//!
//! @author kongweiguang

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use tracing::warn;
use uuid::Uuid;
use zeroize::Zeroizing;

use kerminal_application::connections::credentials::VaultKeyExportMode;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

use super::error::VaultError;

pub(super) const KEY_RELATIVE_PATH: &str = "secrets/vault-key.toml";
pub(super) const VAULT_RELATIVE_PATH: &str = "secrets/vault.toml";
const JOURNAL_RELATIVE_PATH: &str = "secrets/.vault-transaction";
const ACTIVE_JOURNAL_NAME: &str = "active";

/// 同一次变更中可分别更新 key 或 vault；journal 始终备份和恢复完整 pair。
pub(super) struct PairUpdate<'a> {
    pub key: Option<&'a [u8]>,
    pub vault: Option<&'a [u8]>,
}

/// 保留 key、vault 各自可缺失且独立零化的读取结果形状，不合并其生命周期。
///
/// @author kongweiguang
type OptionalVaultFilePair = (Option<Zeroizing<Vec<u8>>>, Option<Zeroizing<Vec<u8>>>);

/// 返回由 root 约束的 vault 文件绝对路径。
pub(super) fn key_path(storage: &StorageCoordinator) -> PathBuf {
    storage.root().join(KEY_RELATIVE_PATH)
}

/// 返回同步允许的加密 vault 文件绝对路径。
pub(super) fn vault_path(storage: &StorageCoordinator) -> PathBuf {
    storage.root().join(VAULT_RELATIVE_PATH)
}

/// 在调用方持有共享 coordinator lock 时检查路径并读取可选的常规文件。
pub(super) fn read_optional(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(VaultError::UnsafePath)
        }
        Ok(_) => fs::read(path)
            .map(|bytes| Some(Zeroizing::new(bytes)))
            .map_err(VaultError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 读取 key/vault 时统一拒绝半成对状态，确保缺 key 的密文不会被默认值覆盖。
/// 返回类型别名只收敛签名复杂度，不改变 key/vault 顺序或各自的零化边界。
///
/// @author kongweiguang
pub(super) fn read_pair(storage: &StorageCoordinator) -> Result<OptionalVaultFilePair, VaultError> {
    ensure_secrets_dir(storage.root())?;
    let key = read_optional(&key_path(storage))?;
    let vault = read_optional(&vault_path(storage))?;
    Ok((key, vault))
}

/// 启动诊断只读检查 pair；不存在的 secrets 目录保持不存在，避免 readiness 查询写用户目录。
/// 与写路径共享 pair 类型，但该分支不创建目录、不恢复事务或改动凭据文件。
///
/// @author kongweiguang
pub(super) fn read_pair_readonly(
    storage: &StorageCoordinator,
) -> Result<OptionalVaultFilePair, VaultError> {
    let directory = storage.root().join("secrets");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(VaultError::UnsafePath);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, None));
        }
        Err(error) => return Err(VaultError::from(error)),
    }
    Ok((
        read_optional(&key_path(storage))?,
        read_optional(&vault_path(storage))?,
    ))
}

/// 使用 journal 在两个文件之间建立 crash-recoverable commit point。
pub(super) fn commit_pair_updates(
    storage: &StorageCoordinator,
    update: PairUpdate<'_>,
) -> Result<(), VaultError> {
    let root = storage.root();
    ensure_secrets_dir(root)?;
    recover_pending(storage)?;
    let (old_key, old_vault) = read_pair(storage)?;
    let journal_root = root.join(JOURNAL_RELATIVE_PATH);
    ensure_directory(&journal_root)?;
    let active = journal_root.join(ACTIVE_JOURNAL_NAME);
    match fs::symlink_metadata(&active) {
        Ok(_) => return Err(VaultError::RecoveryRequired),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(VaultError::from(error)),
    }

    let token = Uuid::new_v4().simple().to_string();
    let preparing = journal_root.join(format!("preparing-{token}"));
    fs::create_dir(&preparing)?;
    write_new_file(
        &preparing.join("manifest"),
        &[u8::from(old_key.is_some()), u8::from(old_vault.is_some())],
    )?;
    if let Some(bytes) = &old_key {
        write_new_file(&preparing.join("key.backup"), bytes.as_slice())?;
    }
    if let Some(bytes) = &old_vault {
        write_new_file(&preparing.join("vault.backup"), bytes.as_slice())?;
    }
    sync_directory(&preparing)?;
    fs::rename(&preparing, &active)?;
    if let Err(error) = sync_directory(&journal_root) {
        return match recover_active(storage) {
            Ok(()) => Err(VaultError::CommitNotCommitted(Box::new(error))),
            Err(_) => Err(VaultError::RecoveryRequired),
        };
    }

    let apply = (|| {
        if let Some(bytes) = update.key {
            atomic_write(storage, &key_path(storage), bytes)?;
        }
        if let Some(bytes) = update.vault {
            atomic_write(storage, &vault_path(storage), bytes)?;
        }
        Ok::<(), VaultError>(())
    })();
    if let Err(error) = apply {
        return match recover_active(storage) {
            Ok(()) => Err(as_not_committed(error)),
            Err(_) => Err(VaultError::RecoveryRequired),
        };
    }

    // journal 从 active 原子转为 committed 后，新 pair 才成为可见的完整提交。
    let committed = journal_root.join(format!("committed-{token}"));
    if let Err(error) = fs::rename(&active, &committed) {
        return match recover_active(storage) {
            Ok(()) => Err(VaultError::CommitNotCommitted(Box::new(VaultError::from(
                error,
            )))),
            Err(_) => Err(VaultError::RecoveryRequired),
        };
    }
    if let Err(error) = sync_directory(&journal_root) {
        return Err(VaultError::CommitOutcomeUnknown(Box::new(error)));
    }
    if remove_tree_if_present(&committed).is_err() {
        // 目录项已同步，业务修改已耐久提交；保留标记供下次恢复清理，不误报为失败。
        warn!("vault transaction cleanup pending after durable commit");
    }
    Ok(())
}

/// 恢复未完成的旧 pair，并清理已提交或尚未进入 commit point 的 staging 目录。
pub(super) fn recover_pending(storage: &StorageCoordinator) -> Result<(), VaultError> {
    match fs::symlink_metadata(storage.root().join("secrets")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(VaultError::from(error)),
        Ok(_) => {}
    }
    ensure_secrets_dir(storage.root())?;
    let journal_root = storage.root().join(JOURNAL_RELATIVE_PATH);
    if !journal_root.exists() {
        return Ok(());
    }
    ensure_directory(&journal_root)?;
    recover_active(storage)?;
    for item in fs::read_dir(&journal_root)? {
        let item = item?;
        let name = item.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("preparing-") || name.starts_with("committed-") {
            remove_tree_if_present(&item.path())?;
        }
    }
    Ok(())
}

/// 把 key TOML 原子导出到用户选定的外部路径，并要求覆盖意图由调用方明确传入。
pub(super) fn export_key_file(
    destination: &Path,
    bytes: &[u8],
    mode: VaultKeyExportMode,
) -> Result<(), VaultError> {
    let name = destination.file_name().ok_or(VaultError::UnsafePath)?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = fs::canonicalize(parent)?;
    let destination = canonical_parent.join(name);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(VaultError::UnsafePath);
        }
        Ok(_) if mode == VaultKeyExportMode::CreateNew => {
            return Err(VaultError::ExportTargetExists);
        }
        Ok(_) => set_private_file_permissions(&destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if mode == VaultKeyExportMode::ReplaceExisting {
                return Err(VaultError::ExportTargetMissing);
            }
        }
        Err(error) => return Err(VaultError::from(error)),
    }

    let staged = canonical_parent.join(format!(
        ".{}.pending-{}",
        name.to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    if let Err(error) = write_new_file(&staged, bytes) {
        let _ = remove_file_if_present(&staged);
        return Err(error);
    }
    let commit_mode = match mode {
        VaultKeyExportMode::CreateNew => CommitMode::CreateNew,
        VaultKeyExportMode::ReplaceExisting => CommitMode::ReplaceExisting,
    };
    let commit_result = match commit_staged_file(&staged, &destination, commit_mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(error)) => Err(VaultError::CommitNotCommitted(
            Box::new(VaultError::from(error)),
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => Err(VaultError::CommitOutcomeUnknown(
            Box::new(VaultError::from(error)),
        )),
    };
    if commit_result.is_ok() {
        if remove_file_if_present(&staged).is_err() {
            tracing::warn!("vault key export staging cleanup pending");
        }
    } else if remove_file_if_present(&staged).is_err() {
        tracing::warn!("vault key export staging cleanup pending after failed commit");
    }
    commit_result
}

/// 在同步端持有整段存储锁前先完成 vault 恢复，避免上传或下载到 key/vault 中间态。
pub fn lock_for_sync(
    storage: &StorageCoordinator,
) -> Result<crate::storage::StorageLock<'_>, VaultError> {
    let guard = storage.lock().map_err(|_| VaultError::LockUnavailable)?;
    recover_pending(storage)?;
    Ok(guard)
}

/// 对一份 legacy key/vault 文件创建不覆盖的 .bak 时间戳备份。
pub(super) fn create_legacy_backup(
    storage: &StorageCoordinator,
    source_path: &Path,
    bytes: &[u8],
) -> Result<PathBuf, VaultError> {
    let token = format!("{}-{}", super::format::timestamp(), Uuid::new_v4());
    let name = source_path
        .file_name()
        .ok_or(VaultError::UnsafePath)?
        .to_string_lossy();
    let backup = source_path.with_file_name(format!("{name}.bak.{token}"));
    atomic_create(storage, &backup, bytes)?;
    Ok(backup)
}

/// 若 active journal 存在则按完整 pair 回滚；重复恢复可以安全重试。
fn recover_active(storage: &StorageCoordinator) -> Result<(), VaultError> {
    let journal_root = storage.root().join(JOURNAL_RELATIVE_PATH);
    let active = journal_root.join(ACTIVE_JOURNAL_NAME);
    let metadata = match fs::symlink_metadata(&active) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(VaultError::from(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(VaultError::UnsafePath);
    }
    let manifest = fs::read(active.join("manifest"))?;
    if manifest.len() != 2 || manifest.iter().any(|value| *value > 1) {
        return Err(VaultError::RecoveryRequired);
    }
    restore_target(
        storage,
        &active.join("key.backup"),
        &key_path(storage),
        manifest[0] == 1,
    )?;
    restore_target(
        storage,
        &active.join("vault.backup"),
        &vault_path(storage),
        manifest[1] == 1,
    )?;
    remove_tree_if_present(&active)?;
    sync_directory(&journal_root)?;
    Ok(())
}

/// pair 已被回滚到旧值后，统一报告业务变更确定没有提交。
fn as_not_committed(error: VaultError) -> VaultError {
    match error {
        VaultError::CommitNotCommitted(source) | VaultError::CommitOutcomeUnknown(source) => {
            VaultError::CommitNotCommitted(source)
        }
        error => VaultError::CommitNotCommitted(Box::new(error)),
    }
}

/// 将备份原子还原，或删除原本不存在的新文件；操作可重复执行。
fn restore_target(
    storage: &StorageCoordinator,
    backup: &Path,
    target: &Path,
    existed_before: bool,
) -> Result<(), VaultError> {
    if existed_before {
        let bytes = read_optional(backup)?.ok_or(VaultError::RecoveryRequired)?;
        atomic_write(storage, target, bytes.as_slice())
    } else {
        remove_file_if_present(target)
    }
}

/// 创建私有 staging 文件并通过共享存储原子 primitive 替换目标。
fn atomic_write(
    storage: &StorageCoordinator,
    target: &Path,
    bytes: &[u8],
) -> Result<(), VaultError> {
    ensure_secrets_dir(storage.root())?;
    ensure_private_existing_file(target)?;
    let parent = target.parent().ok_or(VaultError::UnsafePath)?;
    let name = target
        .file_name()
        .ok_or(VaultError::UnsafePath)?
        .to_string_lossy();
    let staged = parent.join(format!(".{name}.pending-{}", Uuid::new_v4().simple()));
    if let Err(error) = write_new_file(&staged, bytes) {
        let _ = remove_file_if_present(&staged);
        return Err(error);
    }
    let mode = if target.exists() {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::CreateNew
    };
    let result = match commit_staged_file(&staged, target, mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(error)) => Err(VaultError::CommitNotCommitted(
            Box::new(VaultError::from(error)),
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => Err(VaultError::CommitOutcomeUnknown(
            Box::new(VaultError::from(error)),
        )),
    };
    let cleanup = remove_file_if_present(&staged);
    result.and(cleanup)
}

/// 不覆盖已存在的备份名，并保持 Unix key/ciphertext 文件只对当前用户可读。
fn atomic_create(
    storage: &StorageCoordinator,
    target: &Path,
    bytes: &[u8],
) -> Result<(), VaultError> {
    ensure_secrets_dir(storage.root())?;
    let parent = target.parent().ok_or(VaultError::UnsafePath)?;
    let name = target
        .file_name()
        .ok_or(VaultError::UnsafePath)?
        .to_string_lossy();
    let staged = parent.join(format!(".{name}.pending-{}", Uuid::new_v4().simple()));
    if let Err(error) = write_new_file(&staged, bytes) {
        let _ = remove_file_if_present(&staged);
        return Err(error);
    }
    let result = match commit_staged_file(&staged, target, CommitMode::CreateNew) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(error)) => Err(VaultError::CommitNotCommitted(
            Box::new(VaultError::from(error)),
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => Err(VaultError::CommitOutcomeUnknown(
            Box::new(VaultError::from(error)),
        )),
    };
    let cleanup = remove_file_if_present(&staged);
    result.and(cleanup)
}

/// 使用 create_new 防止 staging/backup 被并发覆盖，并在写入后 flush 到磁盘。
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_creation_mode(&mut options);
    let mut file = options.open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// 确保 secrets 目录不可由其它本机用户枚举，并拒绝 symlink 目录替换。
fn ensure_secrets_dir(root: &Path) -> Result<(), VaultError> {
    let secrets = root.join("secrets");
    match fs::symlink_metadata(&secrets) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(VaultError::UnsafePath);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(&secrets)?;
        }
        Err(error) => return Err(VaultError::from(error)),
    }
    set_private_directory_permissions(&secrets)?;
    secure_existing_tree(&secrets)?;
    Ok(())
}

/// 创建 journal 子目录并拒绝将本地 recovery 状态重定向到外部路径。
fn ensure_directory(path: &Path) -> Result<(), VaultError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(VaultError::UnsafePath)
        }
        Ok(_) => secure_existing_tree(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path)
        }
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 删除恢复后已无用的事务目录，不跟随目录 symlink。
fn remove_tree_if_present(path: &Path) -> Result<(), VaultError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(VaultError::from)
        }
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).map_err(VaultError::from),
        Ok(_) => fs::remove_file(path).map_err(VaultError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 删除原本不存在的文件回滚产物；symlink 不会被跟随或误当成常规文件。
fn remove_file_if_present(path: &Path) -> Result<(), VaultError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(VaultError::UnsafePath)
        }
        Ok(_) => fs::remove_file(path).map_err(VaultError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 对旧目标文件先收紧 Unix 权限，确保原子替换失败也不会继续暴露旧 key。
fn ensure_private_existing_file(path: &Path) -> Result<(), VaultError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(VaultError::UnsafePath)
        }
        Ok(_) => set_private_file_permissions(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(VaultError::from(error)),
    }
}

/// 创建用户私有目录；Windows 使用用户配置目录继承 ACL，Unix 显式设 0700。
fn create_private_directory(path: &Path) -> Result<(), VaultError> {
    fs::create_dir(path)?;
    set_private_directory_permissions(path)
}

/// 同步目录项提交，令 journal 建立和清理顺序具备崩溃恢复边界。
fn sync_directory(path: &Path) -> Result<(), VaultError> {
    #[cfg(unix)]
    std::fs::File::open(path)?
        .sync_all()
        .map_err(VaultError::from)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// 在支持的平台上为新 staging 文件设置 owner-only 权限。
fn set_private_creation_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// Unix secrets 路径限制为 owner-only；Windows 继承用户 profile 的 ACL。
fn set_private_directory_permissions(path: &Path) -> Result<(), VaultError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    set_windows_private_acl(path, true)?;
    #[cfg(not(any(unix, windows)))]
    let _ = path;
    Ok(())
}

/// Unix vault/key 文件只允许 owner 读写，Windows 保留 profile ACL 继承语义。
fn set_private_file_permissions(path: &Path) -> Result<(), VaultError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    set_windows_private_acl(path, false)?;
    #[cfg(not(any(unix, windows)))]
    let _ = path;
    Ok(())
}

/// 递归收紧 secrets 中旧 key、vault、备份及 journal 文件的访问权限。
fn secure_existing_tree(directory: &Path) -> Result<(), VaultError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(VaultError::UnsafePath);
        }
        if metadata.is_dir() {
            set_private_directory_permissions(&path)?;
            secure_existing_tree(&path)?;
        } else if metadata.is_file() {
            set_private_file_permissions(&path)?;
        } else {
            return Err(VaultError::UnsafePath);
        }
    }
    Ok(())
}

/// 在 Windows 上把 DACL 限定为当前用户 SID，并关闭父目录权限继承。
#[cfg(windows)]
fn set_windows_private_acl(path: &Path, is_directory: bool) -> Result<(), VaultError> {
    use std::{mem::size_of, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::Authorization::{
            EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
            SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
        },
        Security::{
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetTokenInformation,
            OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        /// 关闭当前进程 token 句柄，防止频繁访问 vault 时泄漏内核对象。
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    struct AclHandle(*mut windows_sys::Win32::Security::ACL);
    impl Drop for AclHandle {
        /// 释放 SetEntriesInAclW 分配的 ACL 缓冲区。
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(VaultError::from(std::io::Error::last_os_error()));
    }
    let _token = TokenHandle(token);
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(VaultError::from(std::io::Error::last_os_error()));
    }
    let mut token_user_storage = vec![0_u64; (required as usize).div_ceil(size_of::<u64>())];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_user_storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(VaultError::from(std::io::Error::last_os_error()));
    }
    let token_user = unsafe { &*token_user_storage.as_ptr().cast::<TOKEN_USER>() };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0x001F_01FF,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if is_directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: token_user.User.Sid.cast(),
        },
    };
    let mut acl = ptr::null_mut();
    let status = unsafe { SetEntriesInAclW(1, &entry, ptr::null(), &mut acl) };
    if status != ERROR_SUCCESS {
        return Err(VaultError::from(std::io::Error::from_raw_os_error(
            status as i32,
        )));
    }
    let _acl = AclHandle(acl);
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(VaultError::from(std::io::Error::from_raw_os_error(
            status as i32,
        )));
    }
    Ok(())
}
