//! 使用隔离临时目录验证本机到本机复制复用统一传输队列。
//!
//! @author kongweiguang

use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest,
        RemoteConnectionLease, RemoteConnectionService,
    },
    transfers::{TransferBackend, TransferPhase, TransferQueueOptions, TransferService},
};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest,
        TransferStatus,
    },
    identity::HostId,
};
use kerminal_infrastructure::files::SftpTransferBackend;
use tokio::{
    runtime::Handle,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

struct UnexpectedRemoteAccess {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RemoteConnectionService for UnexpectedRemoteAccess {
    /// 测试本机队列路径不应触发 SSH 连接；若误触发则计数并返回明确失败。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("本机传输测试不允许访问 SSH"))
    }

    /// 本机传输不会读取远端主机密钥，若误路由则记录调用并失败。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("本机传输测试不允许访问 SSH"))
    }

    /// 本机传输不会写远端信任状态，若误路由则记录调用并失败。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("本机传输测试不允许访问 SSH"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

/// 为单个测试构造真实本机后端与统一传输队列，不建立占位 SSH 会话。
///
/// @author kongweiguang
fn local_transfer_service() -> (TransferService, Arc<AtomicUsize>, Arc<SftpTransferBackend>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let remote: Arc<dyn RemoteConnectionService> = Arc::new(UnexpectedRemoteAccess {
        calls: calls.clone(),
    });
    let backend = Arc::new(SftpTransferBackend::new(remote));
    let service = TransferService::new(
        vec![backend.clone()],
        Handle::current(),
        TransferQueueOptions::default(),
    )
    .expect("valid transfer queue options");
    (service, calls, backend)
}

/// 将两个实际临时路径封装为不带秘密或协议状态的领域传输请求。
///
/// @author kongweiguang
fn request(
    source: PathBuf,
    destination: PathBuf,
    kind: TransferKind,
    conflict_policy: ConflictPolicy,
) -> TransferRequest {
    TransferRequest {
        source: FileEndpoint::Local { path: source },
        destination: FileEndpoint::Local { path: destination },
        kind,
        conflict_policy,
        idle_timeout_seconds: None,
    }
}

/// 等待队列给出权威终态，避免把 enqueue 回执误当成文件已经复制完成。
///
/// @author kongweiguang
async fn await_terminal(
    service: &TransferService,
    id: &kerminal_domain::identity::TransferId,
) -> kerminal_domain::files::TransferSummary {
    timeout(Duration::from_secs(10), async {
        loop {
            let summary = service
                .get(id)
                .await
                .expect("queued task remains queryable");
            if matches!(
                summary.status,
                TransferStatus::Succeeded
                    | TransferStatus::Skipped
                    | TransferStatus::Unchanged
                    | TransferStatus::Failed
                    | TransferStatus::Cancelled
            ) {
                return summary;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("transfer reaches a terminal state within the test budget")
}

/// 经 TransferService 受理请求并等待 worker 完成后返回实际任务投影。
///
/// @author kongweiguang
async fn run_transfer(
    service: &TransferService,
    request: TransferRequest,
) -> kerminal_domain::files::TransferSummary {
    let queued = service
        .enqueue(request)
        .await
        .expect("local transfer is admitted");
    await_terminal(service, &queued.id).await
}

/// 递归记录物理目录项并累计普通文件字节，避免默认隐藏的 sidecar 被 UI 过滤后漏测。
///
/// @author kongweiguang
async fn directory_snapshot(root: &Path) -> io::Result<(Vec<String>, u64)> {
    let mut pending = vec![(root.to_path_buf(), PathBuf::new())];
    let mut entries = Vec::new();
    let mut total_bytes = 0_u64;
    while let Some((directory, relative_directory)) = pending.pop() {
        let mut reader = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = reader.next_entry().await? {
            let relative = relative_directory.join(entry.file_name());
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                entries.push(format!(
                    "{}/",
                    relative.to_string_lossy().replace('\\', "/")
                ));
                pending.push((entry.path(), relative));
            } else {
                if file_type.is_file() {
                    total_bytes = total_bytes.saturating_add(entry.metadata().await?.len());
                }
                entries.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    entries.sort();
    Ok((entries, total_bytes))
}

/// 在隔离测试根内创建目录链接别名；Windows junction 命令必须无窗口，避免文件用例干扰桌面。
///
/// @author kongweiguang
fn create_directory_alias(alias: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let output = std::process::Command::new("cmd.exe")
            .creation_flags(0x0800_0000)
            .arg("/D")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(alias)
            .arg(target)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "创建临时 junction 失败: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, alias)
    }
}

/// 只移除测试创建的链接本身，不递归访问其指向的源目录。
///
/// @author kongweiguang
async fn remove_directory_alias(alias: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        tokio::fs::remove_dir(alias).await
    }
    #[cfg(unix)]
    {
        tokio::fs::remove_file(alias).await
    }
}

/// 确认本机文件与空文件复制、目录复制、冲突选择及取消都走统一队列且不触碰 SSH。
/// 失败回执由格式化参数直接借用；payload 长度按 UTF-8 字节计量。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_local_file_directory_conflict_and_cancel_use_one_queue() {
    let temp = tempfile::tempdir().expect("temporary test root is available");
    let source_root = temp.path().join("source");
    let destination_root = temp.path().join("destination");
    tokio::fs::create_dir_all(&source_root)
        .await
        .expect("source root is created");
    tokio::fs::create_dir_all(&destination_root)
        .await
        .expect("destination root is created");

    let (service, ssh_calls, backend) = local_transfer_service();
    let empty_source = source_root.join("空文件.bin");
    let empty_target = destination_root.join("空文件.bin");
    tokio::fs::write(&empty_source, [])
        .await
        .expect("empty source file is created");
    let empty_request = request(
        empty_source,
        empty_target.clone(),
        TransferKind::File,
        ConflictPolicy::Overwrite,
    );
    assert!(backend.supports(&empty_request));
    assert!(!backend.uses_sftp_policy(&empty_request));
    let empty = run_transfer(&service, empty_request).await;
    assert_eq!(empty.status, TransferStatus::Succeeded);
    assert_eq!(tokio::fs::metadata(&empty_target).await.unwrap().len(), 0);
    assert_eq!(empty.commit_summary.committed_entries(), 1);
    assert!(
        service
            .enqueue(request(
                empty_target.clone(),
                empty_target.clone(),
                TransferKind::File,
                ConflictPolicy::Overwrite,
            ))
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::metadata(&empty_target).await.unwrap().len(), 0);

    let overwrite_source = source_root.join("overwrite.txt");
    let overwrite_target = destination_root.join("overwrite.txt");
    tokio::fs::write(&overwrite_source, b"replacement")
        .await
        .expect("overwrite source is created");
    tokio::fs::write(&overwrite_target, b"old")
        .await
        .expect("conflicting destination is created");
    let overwritten = run_transfer(
        &service,
        request(
            overwrite_source,
            overwrite_target.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    assert_eq!(overwritten.status, TransferStatus::Succeeded);
    assert_eq!(
        tokio::fs::read(&overwrite_target).await.unwrap(),
        b"replacement"
    );

    let skip_source = source_root.join("skip.txt");
    let skip_target = destination_root.join("skip.txt");
    tokio::fs::write(&skip_source, b"new")
        .await
        .expect("skip source is created");
    tokio::fs::write(&skip_target, b"keep")
        .await
        .expect("skip destination is created");
    let skipped = run_transfer(
        &service,
        request(
            skip_source,
            skip_target.clone(),
            TransferKind::File,
            ConflictPolicy::Skip,
        ),
    )
    .await;
    assert_eq!(skipped.status, TransferStatus::Skipped);
    assert_eq!(tokio::fs::read(&skip_target).await.unwrap(), b"keep");

    let rename_source = source_root.join("rename.txt");
    let rename_target = destination_root.join("rename.txt");
    tokio::fs::write(&rename_source, b"renamed copy")
        .await
        .expect("rename source is created");
    tokio::fs::write(&rename_target, b"preserve")
        .await
        .expect("rename destination is created");
    let renamed = run_transfer(
        &service,
        request(
            rename_source,
            rename_target.clone(),
            TransferKind::File,
            ConflictPolicy::Rename,
        ),
    )
    .await;
    assert_eq!(
        renamed.status,
        TransferStatus::Succeeded,
        "Rename 传输未完成: safe_error={:?}, failure_kind={:?}, actual_destination={:?}, commit_summary={:?}, cleanup_pending={}",
        renamed.safe_error,
        renamed.failure_kind,
        renamed.actual_destination,
        renamed.commit_summary,
        renamed.cleanup_pending,
    );
    assert_eq!(tokio::fs::read(&rename_target).await.unwrap(), b"preserve");
    let actual_rename = match renamed.actual_destination.as_ref().unwrap() {
        FileEndpoint::Local { path } => path.clone(),
        _ => panic!("local transfer retains a local destination"),
    };
    assert_eq!(actual_rename.file_name().unwrap(), "rename (1).txt");
    assert_eq!(
        tokio::fs::read(actual_rename).await.unwrap(),
        b"renamed copy"
    );

    let directory_source = source_root.join("tree");
    let directory_target = destination_root.join("tree-copy");
    tokio::fs::create_dir_all(directory_source.join("nested"))
        .await
        .expect("nested source directory is created");
    tokio::fs::write(
        directory_source.join("nested").join("data.txt"),
        b"directory data",
    )
    .await
    .expect("nested source file is created");
    tokio::fs::write(directory_source.join("empty"), [])
        .await
        .expect("empty nested file is created");
    let directory = run_transfer(
        &service,
        request(
            directory_source.clone(),
            directory_target.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    assert_eq!(directory.status, TransferStatus::Succeeded);
    assert_eq!(
        tokio::fs::read(directory_target.join("nested").join("data.txt"))
            .await
            .unwrap(),
        b"directory data"
    );
    assert_eq!(
        tokio::fs::metadata(directory_target.join("empty"))
            .await
            .unwrap()
            .len(),
        0
    );
    let (actual_tree, actual_tree_bytes) = directory_snapshot(&directory_target)
        .await
        .expect("the published tree and every physical entry are readable");
    assert_eq!(
        actual_tree,
        vec!["empty", "nested/", "nested/data.txt"],
        "the atomic root publication contains only source entries, with no temporary or owner sidecars"
    );
    assert!(actual_tree.iter().all(|entry| {
        !entry.contains(".kerminal-owner-")
            && !entry.contains(".kerminal-file-")
            && !entry.ends_with(".partial")
    }));
    assert_eq!(
        actual_tree_bytes,
        b"directory data".len() as u64,
        "directory payload byte totals exclude all internal marker artifacts"
    );

    let self_copy_target = directory_source.join("child-copy");
    let self_copy = run_transfer(
        &service,
        request(
            directory_source,
            self_copy_target.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    assert_eq!(self_copy.status, TransferStatus::Failed);
    assert!(!Path::new(&self_copy_target).exists());

    let cancel_source = source_root.join("cancel-large.bin");
    let cancel_target = destination_root.join("cancel-large.bin");
    let source_file = tokio::fs::File::create(&cancel_source)
        .await
        .expect("large sparse source is created");
    source_file
        .set_len(512 * 1024 * 1024)
        .await
        .expect("large sparse source has a bounded logical size");
    let queued = service
        .enqueue(request(
            cancel_source,
            cancel_target.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ))
        .await
        .expect("large local transfer is admitted");
    timeout(Duration::from_secs(5), async {
        loop {
            let summary = service.get(&queued.id).await.unwrap();
            if summary.phase == Some(TransferPhase::Verifying) {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("source verification begins before cancellation");
    service
        .cancel(&queued.id)
        .await
        .expect("active copy is cancellable");
    let cancelled = await_terminal(&service, &queued.id).await;
    assert_eq!(cancelled.status, TransferStatus::Cancelled);
    assert!(!cancel_target.exists());
    assert_eq!(ssh_calls.load(Ordering::Acquire), 0);

    service
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
        .expect("all local workers release before queue shutdown");
}

/// 确认本机目录的 junction/symlink 别名会在 staging 创建前拒绝，同时不误拒绝文本前缀相似的兄弟目录。
///
/// @author kongweiguang
#[cfg(any(unix, windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_directory_copy_rejects_canonical_alias_before_staging() {
    let temp = tempfile::tempdir().expect("temporary test root is available");
    let source = temp.path().join("source");
    tokio::fs::create_dir_all(&source)
        .await
        .expect("source directory is created");
    tokio::fs::write(source.join("payload.txt"), b"contained")
        .await
        .expect("source fixture is created inside the temporary root");

    let canonical_temp = tokio::fs::canonicalize(temp.path())
        .await
        .expect("temporary root canonicalizes");
    let canonical_source = tokio::fs::canonicalize(&source)
        .await
        .expect("source canonicalizes");
    assert!(canonical_source.starts_with(&canonical_temp));

    let (service, ssh_calls, _) = local_transfer_service();

    // 同前缀兄弟目录不能因字符串前缀关系被当成源目录子树。
    let sibling = temp.path().join("source-copy");
    let sibling_copy = run_transfer(
        &service,
        request(
            source.clone(),
            sibling.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    assert_eq!(sibling_copy.status, TransferStatus::Succeeded);
    assert_eq!(
        tokio::fs::read(sibling.join("payload.txt")).await.unwrap(),
        b"contained"
    );

    let alias = temp.path().join("source-alias");
    create_directory_alias(&alias, &source).expect("directory alias is created in the temp root");
    assert_eq!(
        tokio::fs::canonicalize(&alias)
            .await
            .expect("alias resolves to its target"),
        canonical_source
    );
    let aliased_destination = alias.join("nested-copy");
    let rejected = run_transfer(
        &service,
        request(
            source.clone(),
            aliased_destination.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    assert_eq!(rejected.status, TransferStatus::Failed);
    assert_eq!(
        rejected.failure_kind,
        Some(TransferFailureKind::InvalidSource)
    );
    assert_eq!(
        rejected.safe_error.as_deref(),
        Some("目录不能复制到自身或其子目录")
    );
    assert_eq!(rejected.commit_summary.committed_entries(), 0);
    assert!(!rejected.cleanup_pending);
    assert!(
        tokio::fs::symlink_metadata(&aliased_destination)
            .await
            .is_err()
    );

    // 精确自拷贝错误来自 staging 前的 guard；源下只有 fixture 文件，不能出现 staging 目录或 marker。
    let mut source_entries = tokio::fs::read_dir(&source)
        .await
        .expect("source directory remains readable");
    let mut names = Vec::new();
    while let Some(entry) = source_entries
        .next_entry()
        .await
        .expect("source entries are readable")
    {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(names, vec!["payload.txt"]);
    assert_eq!(ssh_calls.load(Ordering::Acquire), 0);

    remove_directory_alias(&alias)
        .await
        .expect("only the temporary alias is removed");
    service
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
        .expect("local transfer workers are released");
}

/// 验证只读源目录在 marker 退役前不会让 staging 失去可写权限，发布与 merge 后再准确恢复源 mode。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_directory_transfer_preserves_read_only_source_modes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temporary test root is available");
    let source = temp.path().join("source");
    let source_existing = source.join("existing");
    let source_new = source.join("new");
    let source_nested = source_new.join("nested");
    tokio::fs::create_dir_all(&source_existing)
        .await
        .expect("existing source branch is created");
    tokio::fs::create_dir_all(&source_nested)
        .await
        .expect("new source branch is created");
    tokio::fs::write(source_existing.join("payload.txt"), b"existing branch")
        .await
        .expect("existing source payload is created");
    tokio::fs::write(source_nested.join("payload.txt"), b"new nested branch")
        .await
        .expect("new source payload is created");

    let published = temp.path().join("published");
    let merged = temp.path().join("merged");
    let merged_existing = merged.join("existing");
    tokio::fs::create_dir_all(&merged_existing)
        .await
        .expect("merge target and existing branch are created");
    tokio::fs::write(merged_existing.join("preserved.txt"), b"keep")
        .await
        .expect("preexisting destination payload is created");

    for (directory, mode) in [
        (&source_nested, 0o500),
        (&source_existing, 0o500),
        (&source_new, 0o550),
        (&source, 0o555),
        (&merged, 0o750),
        (&merged_existing, 0o711),
    ] {
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .await
            .expect("fixture directory mode is applied");
    }

    let (service, ssh_calls, _) = local_transfer_service();
    let published_summary = run_transfer(
        &service,
        request(
            source.clone(),
            published.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    let published_snapshot = directory_snapshot(&published).await;
    let published_existing_payload = tokio::fs::read(published.join("existing/payload.txt"))
        .await
        .ok();
    let published_new_payload = tokio::fs::read(published.join("new/nested/payload.txt"))
        .await
        .ok();
    let published_root_mode = std::fs::metadata(&published)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let published_existing_mode = std::fs::metadata(published.join("existing"))
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let published_new_mode = std::fs::metadata(published.join("new"))
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let published_nested_mode = std::fs::metadata(published.join("new").join("nested"))
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);

    let merged_summary = run_transfer(
        &service,
        request(
            source.clone(),
            merged.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;
    let merged_snapshot = directory_snapshot(&merged).await;
    let merged_existing_payload = tokio::fs::read(merged.join("existing/payload.txt"))
        .await
        .ok();
    let merged_new_payload = tokio::fs::read(merged.join("new/nested/payload.txt"))
        .await
        .ok();
    let merged_preserved_payload = tokio::fs::read(merged.join("existing/preserved.txt"))
        .await
        .ok();
    let merged_root_mode = std::fs::metadata(&merged)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let merged_existing_mode = std::fs::metadata(&merged_existing)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let merged_new_mode = std::fs::metadata(merged.join("new"))
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let merged_nested_mode = std::fs::metadata(merged.join("new").join("nested"))
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);

    // 测试退出前先恢复权限，保证临时根可由普通 TempDir 清理逻辑递归移除。
    for directory in [
        source.clone(),
        source_existing.clone(),
        source_new.clone(),
        source_nested.clone(),
        published.clone(),
        published.join("existing"),
        published.join("new"),
        published.join("new").join("nested"),
        merged.clone(),
        merged_existing.clone(),
        merged.join("new"),
        merged.join("new").join("nested"),
    ] {
        if directory.is_dir() {
            tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .await
                .expect("test directories are writable before temp cleanup");
        }
    }
    service
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
        .expect("directory transfer workers release before shutdown");

    assert_eq!(published_summary.status, TransferStatus::Succeeded);
    let (published_entries, published_bytes) =
        published_snapshot.expect("published tree is traversable");
    assert_eq!(
        published_entries,
        vec![
            "existing/",
            "existing/payload.txt",
            "new/",
            "new/nested/",
            "new/nested/payload.txt",
        ]
    );
    assert_eq!(
        published_existing_payload.as_deref(),
        Some(&b"existing branch"[..])
    );
    assert_eq!(
        published_new_payload.as_deref(),
        Some(&b"new nested branch"[..])
    );
    assert_eq!(
        published_bytes,
        (b"existing branch".len() + b"new nested branch".len()) as u64
    );
    assert_eq!(published_root_mode, Some(0o555));
    assert_eq!(published_existing_mode, Some(0o500));
    assert_eq!(published_new_mode, Some(0o550));
    assert_eq!(published_nested_mode, Some(0o500));

    assert_eq!(merged_summary.status, TransferStatus::Succeeded);
    let (merged_entries, merged_bytes) = merged_snapshot.expect("merged tree is traversable");
    assert_eq!(
        merged_entries,
        vec![
            "existing/",
            "existing/payload.txt",
            "existing/preserved.txt",
            "new/",
            "new/nested/",
            "new/nested/payload.txt",
        ]
    );
    assert_eq!(
        merged_existing_payload.as_deref(),
        Some(&b"existing branch"[..])
    );
    assert_eq!(
        merged_new_payload.as_deref(),
        Some(&b"new nested branch"[..])
    );
    assert_eq!(merged_preserved_payload.as_deref(), Some(&b"keep"[..]));
    assert_eq!(
        merged_bytes,
        (b"existing branch".len() + b"new nested branch".len() + b"keep".len()) as u64
    );
    assert_eq!(merged_root_mode, Some(0o750));
    assert_eq!(merged_existing_mode, Some(0o711));
    assert_eq!(merged_new_mode, Some(0o550));
    assert_eq!(merged_nested_mode, Some(0o500));
    assert_eq!(ssh_calls.load(Ordering::Acquire), 0);
}

/// 用真实目录任务覆盖 Windows 旧 MAX_PATH 边界，并按实际 payload 长度核对已提交字节。
/// 新目标树通过一次根目录原子重命名发布，因此提交回执计数应为一个，而非树内文件数。
///
/// @author kongweiguang
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_directory_transfer_commits_nested_files_over_legacy_max_path() {
    use std::os::windows::ffi::OsStrExt;

    const ROOT_PAYLOAD: &str = "01234567890123456789";
    const NESTED_PAYLOAD: &str = "nested payload bytes 123";
    let temp = tempfile::tempdir().expect("temporary test root is available");
    let mut fixture_root = temp.path().to_path_buf();
    let component = "深层路径组件-0123456789abcdef";
    // 用与生产相同长度的 owner/随机 UUID，并包含目录任务根 staging；只投影路径长度，不创建或碰触这些路径。
    let projected_staged_file = |parent: &Path| {
        parent
            .join(".kerminal-transfer-00000000-0000-0000-0000-000000000000-00000000000000000000000000000000.partial")
            .join("子目录")
            .join(".kerminal-file-00000000000000000000000000000000.partial")
    };
    while projected_staged_file(&fixture_root)
        .as_os_str()
        .encode_wide()
        .count()
        <= 300
    {
        fixture_root.push(component);
    }

    let source = fixture_root.join("来源").join("目录甲");
    let destination_parent = fixture_root.join("目标父目录");
    let destination = destination_parent.join("目录甲");
    tokio::fs::create_dir_all(source.join("子目录"))
        .await
        .expect("nested source directories are created");
    tokio::fs::create_dir_all(&destination_parent)
        .await
        .expect("the empty destination parent exists before enqueue");
    tokio::fs::write(source.join("同名.txt"), ROOT_PAYLOAD.as_bytes())
        .await
        .expect("root file is created");
    tokio::fs::write(
        source.join("子目录").join("嵌套.txt"),
        NESTED_PAYLOAD.as_bytes(),
    )
    .await
    .expect("nested file is created");

    let destination_file = projected_staged_file(&destination_parent);
    assert!(
        destination_file.as_os_str().encode_wide().count() > 300,
        "the staged nested path must exceed the legacy Win32 path limit"
    );

    let (service, ssh_calls, _) = local_transfer_service();
    let summary = run_transfer(
        &service,
        request(
            source,
            destination.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await;

    assert_eq!(
        summary.status,
        TransferStatus::Succeeded,
        "deep directory transfer failed: safe_error={:?}, failure_kind={:?}, actual_destination={:?}, committed_entries={}, committed_bytes={}, cleanup_pending={}",
        summary.safe_error,
        summary.failure_kind,
        summary.actual_destination,
        summary.commit_summary.committed_entries(),
        summary.commit_summary.committed_bytes(),
        summary.cleanup_pending,
    );
    assert_eq!(
        tokio::fs::read(destination.join("同名.txt")).await.unwrap(),
        ROOT_PAYLOAD.as_bytes()
    );
    assert_eq!(
        tokio::fs::read(destination.join("子目录").join("嵌套.txt"))
            .await
            .unwrap(),
        NESTED_PAYLOAD.as_bytes()
    );
    let expected_transfer_bytes = (ROOT_PAYLOAD.len() + NESTED_PAYLOAD.len()) as u64;
    assert_eq!(summary.total_bytes, Some(expected_transfer_bytes));
    assert_eq!(
        summary.commit_summary.committed_bytes(),
        expected_transfer_bytes
    );
    assert_eq!(
        summary.commit_summary.committed_entries(),
        1,
        "a new directory tree publishes through one atomic root commit; summary={:?}",
        summary.commit_summary
    );
    assert_eq!(ssh_calls.load(Ordering::Acquire), 0);
    service
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
        .expect("all local workers release before queue shutdown");
}
