//! 本机文件服务上传源类型检查回归测试。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{
    AppResult,
    files::{FileAccessProvider, FileService},
};
use kerminal_domain::files::{FileEndpoint, FileEntryKind};
use kerminal_infrastructure::files::{JsonFileBookmarkRepository, LocalFileProvider};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    /// 每个测试使用独立随机目录，并在 Drop 时只清理自身临时数据。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kerminal-file-service-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    /// 保证测试提前失败时也只回收本测试创建的临时目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 组合真实本机 metadata provider，借用最小路径接口并将书签限制在测试根目录内。
///
/// @author kongweiguang
fn file_service(root: &Path) -> AppResult<FileService> {
    FileService::new(
        vec![Arc::new(LocalFileProvider)],
        Arc::new(JsonFileBookmarkRepository::new(root.join("config"))),
    )
}

/// 目录枚举通过真实 metadata 报告点文件隐藏状态，不让 Files UI 从名称自行猜测。
///
/// @author kongweiguang
#[tokio::test]
async fn local_directory_listing_marks_dotfiles_hidden() -> AppResult<()> {
    let root = TestDirectory::new();
    fs::write(root.0.join(".hidden"), b"hidden").expect("write hidden fixture");
    fs::write(root.0.join("visible.txt"), b"visible").expect("write visible fixture");

    let listing = LocalFileProvider
        .list_directory(
            CancellationToken::new(),
            FileEndpoint::Local {
                path: root.0.clone(),
            },
        )
        .await?;

    let hidden = listing
        .entries
        .iter()
        .find(|entry| entry.name == ".hidden")
        .expect("hidden fixture is listed");
    let visible = listing
        .entries
        .iter()
        .find(|entry| entry.name == "visible.txt")
        .expect("visible fixture is listed");
    assert!(hidden.is_hidden);
    assert!(!visible.is_hidden);
    Ok(())
}

/// Windows 隐藏属性需独立于点文件名保留，测试只修改本测试创建的临时文件。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test]
async fn local_directory_listing_reads_windows_hidden_attribute() -> AppResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

    let root = TestDirectory::new();
    let path = root.0.join("windows-hidden.txt");
    fs::write(&path, b"hidden by attribute").expect("write hidden fixture");
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: path_wide is NUL-terminated and remains alive for the synchronous Win32 call.
    let updated = unsafe { SetFileAttributesW(path_wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
    assert_ne!(
        updated, 0,
        "set the hidden attribute on the isolated fixture"
    );

    let listing = LocalFileProvider
        .list_directory(
            CancellationToken::new(),
            FileEndpoint::Local {
                path: root.0.clone(),
            },
        )
        .await?;
    let entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == "windows-hidden.txt")
        .expect("hidden fixture is listed");

    assert!(entry.is_hidden);
    Ok(())
}

/// 通过应用端口返回真实条目类型，确保文件和目录的上传决策不由 UI 读磁盘。
#[tokio::test]
async fn inspect_local_transfer_source_returns_file_and_directory_kinds() -> AppResult<()> {
    let root = TestDirectory::new();
    let file = root.0.join("payload.txt");
    let directory = root.0.join("folder");
    fs::write(&file, b"payload").expect("write isolated fixture");
    fs::create_dir(&directory).expect("create isolated fixture directory");
    let service = file_service(&root.0)?;

    let file_entry = service
        .inspect_local_transfer_source(CancellationToken::new(), file)
        .await?;
    let directory_entry = service
        .inspect_local_transfer_source(CancellationToken::new(), directory)
        .await?;

    assert_eq!(file_entry.kind, FileEntryKind::File);
    assert_eq!(directory_entry.kind, FileEntryKind::Directory);
    Ok(())
}

/// 不存在的源通过应用错误返回，不能被上传界面静默略过。
#[tokio::test]
async fn inspect_local_transfer_source_reports_missing_path() -> AppResult<()> {
    let root = TestDirectory::new();
    let service = file_service(&root.0)?;
    let missing = root.0.join("missing.txt");

    let error = service
        .inspect_local_transfer_source(CancellationToken::new(), missing)
        .await
        .expect_err("missing upload source must be visible");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::NotFound
    );
    Ok(())
}

/// 符号链接保持自己的条目类型，调用方据此拒绝跟随链接上传。
#[cfg(unix)]
#[tokio::test]
async fn inspect_local_transfer_source_preserves_symlink_identity() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = TestDirectory::new();
    let target = root.0.join("target.txt");
    let link = root.0.join("link.txt");
    fs::write(&target, b"payload").expect("write isolated fixture");
    symlink(&target, &link).expect("create isolated symlink fixture");
    let service = file_service(&root.0)?;

    let entry = service
        .inspect_local_transfer_source(CancellationToken::new(), link)
        .await?;

    assert_eq!(entry.kind, FileEntryKind::Symlink);
    Ok(())
}
