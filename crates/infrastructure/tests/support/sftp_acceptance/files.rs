//! SFTP 文件浏览、编辑并发和目录 merge 的真实协议场景。
//!
//! @author kongweiguang

use std::{
    fs, io,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

use kerminal_application::{
    PublicErrorCode, connections::management::HostSftpOptions, files::editor::FileEditorService,
};
use kerminal_domain::files::{
    FileEndpoint, FileEntry, FileEntryKind, FileRevision, LineEnding, WriteTextRequest,
    WriteTextStrategy, editor::SaveResolution,
};
use tokio_util::sync::CancellationToken;

use super::{
    AcceptanceResult, SftpAcceptanceFixture, create_owned_remote_symlink, destination_path,
    remote_child, remote_endpoint, request, require, run_stage, run_transfer, wait_for_terminal,
    write_remote_text,
};
use kerminal_domain::files::{ConflictPolicy, TransferKind, TransferStatus};
/// 通过实际 SFTP transfer 校验零字节、Unicode 名称、二进制读取及原子覆盖。
///
/// @author kongweiguang
pub(super) async fn exercise_empty_binary_and_atomic_overwrite(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    write_remote_text(fixture, &fixture.workspace, ".kerminal-hidden", "hidden").await?;

    let empty_source = fixture.local_root.path().join("empty.bin");
    fs::write(&empty_source, b"")?;
    let empty_destination = remote_child(&fixture.host_id, &fixture.workspace, "空文件.bin")?;
    let empty = run_transfer(
        fixture,
        "files.empty_file_upload",
        request(
            FileEndpoint::Local { path: empty_source },
            empty_destination.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        empty.status == TransferStatus::Succeeded && empty.commit_summary.committed_entries() == 1,
        "empty file transfer did not receive its commit receipt",
    )?;
    let empty_preview = fixture
        .files
        .preview_file(CancellationToken::new(), empty_destination, 32)
        .await?;
    require(
        empty_preview.bytes.is_empty(),
        "remote empty file is not empty",
    )?;

    let binary_source = fixture.local_root.path().join("unicode-λ-文件.bin");
    let binary = (0..256 * 1024)
        .map(|index| ((index * 31) % 256) as u8)
        .collect::<Vec<_>>();
    fs::write(&binary_source, &binary)?;
    let binary_destination = remote_child(&fixture.host_id, &fixture.workspace, "二进制-λ.bin")?;
    let uploaded = run_transfer(
        fixture,
        "files.binary_upload",
        request(
            FileEndpoint::Local {
                path: binary_source.clone(),
            },
            binary_destination.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        uploaded.status == TransferStatus::Succeeded,
        "binary upload failed",
    )?;
    let listing = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    let binary_entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == "二进制-λ.bin");
    require(
        binary_entry.is_some(),
        "Unicode filename is absent from SFTP listing",
    )?;
    require(
        binary_entry.is_some_and(|entry| entry.raw.is_some()),
        "SFTP attribute projection is missing",
    )?;
    let hidden_entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == ".kerminal-hidden");
    require(
        hidden_entry.is_some_and(|entry| entry.is_hidden),
        "SFTP dotfile is not marked hidden",
    )?;
    require(
        binary_entry.is_some_and(|entry| !entry.is_hidden),
        "SFTP visible file was marked hidden",
    )?;
    let preview = fixture
        .files
        .preview_file(CancellationToken::new(), binary_destination.clone(), 128)
        .await?;
    require(preview.binary, "binary file was classified as text")?;
    require(
        preview.bytes.as_slice() == &binary[..128],
        "SFTP preview does not preserve binary bytes",
    )?;

    let downloaded = fixture.local_root.path().join("downloaded.bin");
    let download = run_transfer(
        fixture,
        "files.binary_download",
        request(
            binary_destination.clone(),
            FileEndpoint::Local {
                path: downloaded.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        download.status == TransferStatus::Succeeded,
        "binary download failed",
    )?;
    require(
        fs::read(&downloaded)? == binary,
        "downloaded binary content differs from source",
    )?;

    let replacement_source = fixture.local_root.path().join("replacement.bin");
    let replacement = b"atomic replacement payload\0with binary tail";
    fs::write(&replacement_source, replacement)?;
    let overwritten = run_transfer(
        fixture,
        "files.atomic_overwrite",
        request(
            FileEndpoint::Local {
                path: replacement_source,
            },
            binary_destination.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        overwritten.status == TransferStatus::Succeeded,
        "atomic overwrite failed",
    )?;
    let replacement_preview = fixture
        .files
        .preview_file(
            CancellationToken::new(),
            binary_destination,
            replacement.len() + 1,
        )
        .await?;
    require(
        replacement_preview.bytes.as_slice() == replacement,
        "atomic overwrite exposed stale or partial content",
    )?;
    Ok(())
}

/// 每个主机策略在准入时冻结；无论断言或网络步骤如何失败，返回前都显式还原内存配置。
///
/// @author kongweiguang
pub(super) async fn exercise_mtime_and_remote_source_symlink_policy(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    let exercise = exercise_mtime_and_remote_source_symlink_policy_inner(fixture).await;
    let restore = restore_default_sftp_options(fixture).await;
    match (exercise, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(primary), Err(restore)) => Err(io::Error::other(format!(
            "SFTP acceptance failed ({primary}); fixture option restore also failed ({restore})"
        ))
        .into()),
    }
}

/// 将依赖本机链接权限的断言隔离到独立测试入口；仅测试 crate 可见，失败仍还原两台 fixture 的策略。
///
/// @author kongweiguang
pub(crate) async fn exercise_local_source_symlink_policy(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    let exercise = exercise_local_source_symlink_policy_inner(fixture).await;
    let restore = restore_default_sftp_options(fixture).await;
    match (exercise, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(primary), Err(restore)) => Err(io::Error::other(format!(
            "local source symlink acceptance failed ({primary}); fixture option restore also failed ({restore})"
        ))
        .into()),
    }
}

/// 两台 fixture 主机都恢复默认值；即使第一份写入失败，仍尝试释放第二份临时策略。
///
/// @author kongweiguang
async fn restore_default_sftp_options(fixture: &SftpAcceptanceFixture) -> AcceptanceResult {
    let target = fixture
        .host_management
        .set_sftp_options(&fixture.host_id, HostSftpOptions::default())
        .await;
    let source = fixture
        .host_management
        .set_sftp_options(&fixture.source_host_id, HostSftpOptions::default())
        .await;
    match (target, source) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(Box::new(error)),
        (Err(target), Err(source)) => Err(io::Error::other(format!(
            "target host option restore failed ({target}); source host option restore failed ({source})"
        ))
        .into()),
    }
}

/// 通过真实队列验证秒级 mtime 策略、远端源链接策略与目录根链接 containment；夹具阶段便于定位协议失败。
///
/// @author kongweiguang
async fn exercise_mtime_and_remote_source_symlink_policy_inner(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    restore_default_sftp_options(fixture).await?;
    let source_seconds = 1_700_000_123_u64;
    let timestamp = UNIX_EPOCH + Duration::from_secs(source_seconds);
    let mtime_source = fixture.local_root.path().join("mtime-source.txt");
    let mtime_payload = b"mtime acceptance payload";
    fs::write(&mtime_source, mtime_payload)?;
    fs::OpenOptions::new()
        .write(true)
        .open(&mtime_source)?
        .set_times(fs::FileTimes::new().set_modified(timestamp))?;
    let observed_source_seconds = fs::metadata(&mtime_source)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs();
    require(
        observed_source_seconds == source_seconds,
        "local fixture filesystem did not retain the selected whole-second timestamp",
    )?;

    let remote_mtime = remote_child(&fixture.host_id, &fixture.workspace, "mtime-roundtrip.txt")?;
    run_transfer(
        fixture,
        "files.mtime.upload_default_preserve",
        request(
            FileEndpoint::Local {
                path: mtime_source.clone(),
            },
            remote_mtime.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    let uploaded_entry = remote_file_entry(fixture, "mtime-roundtrip.txt").await?;
    require(
        uploaded_entry.kind == FileEntryKind::File,
        "mtime upload did not create a regular remote file",
    )?;
    let expected_remote_mtime = format!("{source_seconds}000000000");
    require(
        uploaded_entry.modified.as_deref() == Some(expected_remote_mtime.as_str()),
        "OpenSSH stat did not confirm the uploaded file's second-precision mtime",
    )?;
    let remote_readback = fixture
        .files
        .preview_file(
            CancellationToken::new(),
            remote_mtime.clone(),
            mtime_payload.len() + 1,
        )
        .await?;
    require(
        remote_readback.bytes.as_slice() == mtime_payload,
        "mtime upload readback content differs from its local source",
    )?;

    let local_mtime = fixture.local_root.path().join("mtime-download.txt");
    run_transfer(
        fixture,
        "files.mtime.download_default_preserve",
        request(
            remote_mtime.clone(),
            FileEndpoint::Local {
                path: local_mtime.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    let downloaded_seconds = fs::metadata(&local_mtime)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs();
    require(
        downloaded_seconds == source_seconds,
        "downloaded local file did not retain the remote whole-second mtime",
    )?;
    require(
        fs::read(local_mtime)?.as_slice() == mtime_payload,
        "mtime download readback content differs from its remote source",
    )?;

    let no_preserve = HostSftpOptions {
        preserve_mtime: false,
        ..HostSftpOptions::default()
    };
    fixture
        .host_management
        .set_sftp_options(&fixture.host_id, no_preserve)
        .await?;
    let no_preserve_remote = remote_child(
        &fixture.host_id,
        &fixture.workspace,
        "mtime-not-preserved.txt",
    )?;
    run_transfer(
        fixture,
        "files.mtime.upload_preserve_disabled",
        request(
            FileEndpoint::Local {
                path: mtime_source.clone(),
            },
            no_preserve_remote,
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    let no_preserve_entry = remote_file_entry(fixture, "mtime-not-preserved.txt").await?;
    let uploaded_without_preserve = remote_mtime_seconds(&no_preserve_entry)?;
    require(
        uploaded_without_preserve != source_seconds,
        "upload preserved source mtime even though the destination host option is disabled",
    )?;

    let no_preserve_local = fixture
        .local_root
        .path()
        .join("mtime-download-not-preserved.txt");
    run_transfer(
        fixture,
        "files.mtime.download_preserve_disabled",
        request(
            remote_mtime.clone(),
            FileEndpoint::Local {
                path: no_preserve_local.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    let downloaded_without_preserve = fs::metadata(&no_preserve_local)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs();
    require(
        downloaded_without_preserve != source_seconds,
        "download preserved remote mtime even though the source host option is disabled",
    )?;
    require(
        fs::read(no_preserve_local)?.as_slice() == mtime_payload,
        "non-preserving download changed payload bytes",
    )?;

    write_remote_text(
        fixture,
        &fixture.source_workspace,
        "remote-inside.txt",
        "remote source symlink payload",
    )
    .await?;
    let remote_inside_target = fixture
        .source_workspace
        .join_name("remote-inside.txt")
        .map_err(|_| io::Error::other("invalid owned remote symlink target"))?;
    let remote_default_link = fixture
        .source_workspace
        .join_name("remote-default-link.txt")
        .map_err(|_| io::Error::other("invalid owned remote symlink path"))?;
    run_stage(
        "files.symlink.remote_fixture.default",
        create_owned_remote_symlink(
            fixture,
            &fixture.source_host_id,
            &remote_default_link,
            &remote_inside_target,
        ),
    )
    .await?;
    fixture
        .host_management
        .set_sftp_options(&fixture.source_host_id, HostSftpOptions::default())
        .await?;
    require_rejected_remote_source_without_local_write(
        fixture,
        "files.symlink.remote_default_rejected",
        remote_endpoint(&fixture.source_host_id, &remote_default_link),
        fixture.local_root.path().join("remote-default-denied.txt"),
    )
    .await?;

    let remote_follow = HostSftpOptions {
        follow_source_symlinks: true,
        ..HostSftpOptions::default()
    };
    fixture
        .host_management
        .set_sftp_options(&fixture.source_host_id, remote_follow)
        .await?;
    let remote_allowed_link = fixture
        .source_workspace
        .join_name("remote-follow-link.txt")
        .map_err(|_| io::Error::other("invalid owned remote symlink path"))?;
    run_stage(
        "files.symlink.remote_fixture.allowed",
        create_owned_remote_symlink(
            fixture,
            &fixture.source_host_id,
            &remote_allowed_link,
            &remote_inside_target,
        ),
    )
    .await?;
    let remote_allowed_local = fixture.local_root.path().join("remote-followed.txt");
    let remote_allowed = run_transfer(
        fixture,
        "files.symlink.remote_in_scope_opt_in",
        request(
            remote_endpoint(&fixture.source_host_id, &remote_allowed_link),
            FileEndpoint::Local {
                path: remote_allowed_local.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        remote_allowed.commit_summary.committed_entries() == 1,
        "in-scope remote source symlink did not receive one commit receipt",
    )?;
    require(
        fs::read(remote_allowed_local)?.as_slice() == b"remote source symlink payload",
        "in-scope remote source symlink did not copy the referenced file content",
    )?;

    write_remote_text(
        fixture,
        &fixture.workspace,
        "remote-outside-source-root.txt",
        "outside selected source root",
    )
    .await?;
    let outside_remote_target = fixture
        .workspace
        .join_name("remote-outside-source-root.txt")
        .map_err(|_| io::Error::other("invalid outside-root symlink target"))?;
    let remote_outside_link = fixture
        .source_workspace
        .join_name("remote-outside-link.txt")
        .map_err(|_| io::Error::other("invalid owned remote symlink path"))?;
    run_stage(
        "files.symlink.remote_fixture.outside",
        create_owned_remote_symlink(
            fixture,
            &fixture.source_host_id,
            &remote_outside_link,
            &outside_remote_target,
        ),
    )
    .await?;
    require_rejected_remote_source_without_local_write(
        fixture,
        "files.symlink.remote_out_of_scope_rejected",
        remote_endpoint(&fixture.source_host_id, &remote_outside_link),
        fixture.local_root.path().join("remote-outside-denied.txt"),
    )
    .await?;
    exercise_remote_directory_root_link_cannot_copy_into_source(fixture).await?;
    Ok(())
}

/// 用真实远端目录根链接覆盖 canonical source 与目标子树相交场景，并核对拒绝前后没有 staging 或目标写入。
///
/// @author kongweiguang
async fn exercise_remote_directory_root_link_cannot_copy_into_source(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    let source_directory = fixture
        .source_workspace
        .join_name("directory-root-real")
        .map_err(|_| io::Error::other("invalid owned remote source directory"))?;
    let source_endpoint = remote_endpoint(&fixture.source_host_id, &source_directory);
    fixture
        .files
        .create_directory(CancellationToken::new(), source_endpoint.clone())
        .await?;
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: remote_child(&fixture.source_host_id, &source_directory, "payload.txt")?,
                content: "directory root containment payload".to_owned(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await?;
    let source_link = fixture
        .source_workspace
        .join_name("directory-root-alias")
        .map_err(|_| io::Error::other("invalid owned remote source link"))?;
    run_stage(
        "files.symlink.remote_directory_root_fixture",
        create_owned_remote_symlink(
            fixture,
            &fixture.source_host_id,
            &source_link,
            &source_directory,
        ),
    )
    .await?;
    fixture
        .host_management
        .set_sftp_options(
            &fixture.source_host_id,
            HostSftpOptions {
                follow_source_symlinks: true,
                ..HostSftpOptions::default()
            },
        )
        .await?;

    let destination_path = source_directory
        .join_name("copy-inside-source")
        .map_err(|_| io::Error::other("invalid owned remote destination"))?;
    let destination = remote_endpoint(&fixture.source_host_id, &destination_path);
    let parent = remote_endpoint(&fixture.source_host_id, &source_directory);
    let mut before = fixture
        .files
        .list_directory(CancellationToken::new(), parent.clone())
        .await?
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    before.sort();

    let initial = fixture
        .transfers
        .enqueue(request(
            remote_endpoint(&fixture.source_host_id, &source_link),
            destination,
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ))
        .await?;
    let summary = wait_for_terminal(
        &fixture.transfers,
        &initial.id,
        "files.symlink.remote_directory_root_self_copy_rejected",
    )
    .await?;
    if summary.cleanup_pending {
        fixture
            .transfers
            .retry_cleanup_until(
                &summary.id,
                tokio::time::Instant::now() + Duration::from_secs(20),
            )
            .await?;
    }
    let summary = fixture.transfers.get(&initial.id).await?;
    require(
        summary.status == TransferStatus::Failed
            && summary.commit_summary.committed_entries() == 0
            && !summary.cleanup_pending
            && summary.safe_error.as_deref() == Some("目录不能复制到自身或其子目录"),
        "remote directory root alias was committed or retained an unresolved stage owner",
    )?;
    let mut after = fixture
        .files
        .list_directory(CancellationToken::new(), parent)
        .await?
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    after.sort();
    require(
        after == before,
        "remote directory containment rejection created a stage or destination entry",
    )?;
    require(
        !after.iter().any(|name| name == "copy-inside-source"),
        "remote directory containment rejection wrote its requested destination",
    )
}

/// 只测本机作为源端时的符号链接策略；真实权限错误会失败并由外层 fixture 清理，不改成跳过。
///
/// @author kongweiguang
async fn exercise_local_source_symlink_policy_inner(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    restore_default_sftp_options(fixture).await?;
    let allowed_root = fixture.local_root.path().join("symlink-source-root");
    fs::create_dir(&allowed_root)?;
    let allowed_target = allowed_root.join("inside.txt");
    let symlink_payload = b"in-scope symlink payload";
    fs::write(&allowed_target, symlink_payload)?;
    let default_link = allowed_root.join("default-link.txt");
    create_file_symlink(&allowed_target, &default_link)?;
    fixture
        .host_management
        .set_sftp_options(&fixture.host_id, HostSftpOptions::default())
        .await?;
    require_rejected_source_without_remote_write(
        fixture,
        "files.symlink.default_rejected",
        default_link,
        "symlink-default-denied.txt",
    )
    .await?;

    let opted_in = HostSftpOptions {
        follow_source_symlinks: true,
        ..HostSftpOptions::default()
    };
    fixture
        .host_management
        .set_sftp_options(&fixture.host_id, opted_in)
        .await?;
    let allowed_link = allowed_root.join("opt-in-link.txt");
    create_file_symlink(&allowed_target, &allowed_link)?;
    let followed_destination =
        remote_child(&fixture.host_id, &fixture.workspace, "symlink-opt-in.txt")?;
    let followed = run_transfer(
        fixture,
        "files.symlink.in_scope_opt_in",
        request(
            FileEndpoint::Local { path: allowed_link },
            followed_destination.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        followed.commit_summary.committed_entries() == 1,
        "in-scope opted-in source symlink did not receive one commit receipt",
    )?;
    let followed_readback = fixture
        .files
        .preview_file(
            CancellationToken::new(),
            followed_destination,
            symlink_payload.len() + 1,
        )
        .await?;
    require(
        followed_readback.bytes.as_slice() == symlink_payload,
        "in-scope source symlink did not copy the referenced file content",
    )?;

    let outside_target = fixture.local_root.path().join("outside-scope.txt");
    fs::write(&outside_target, b"outside allowed root")?;
    let outside_link = allowed_root.join("outside-link.txt");
    create_file_symlink(&outside_target, &outside_link)?;
    require_rejected_source_without_remote_write(
        fixture,
        "files.symlink.out_of_scope_rejected",
        outside_link,
        "symlink-out-of-scope-denied.txt",
    )
    .await?;

    let dangling_link = allowed_root.join("dangling-link.txt");
    create_file_symlink(&allowed_root.join("missing-target.txt"), &dangling_link)?;
    require_rejected_source_without_remote_write(
        fixture,
        "files.symlink.dangling_rejected",
        dangling_link,
        "symlink-dangling-denied.txt",
    )
    .await?;

    let cycle_a = allowed_root.join("cycle-a.txt");
    let cycle_b = allowed_root.join("cycle-b.txt");
    create_file_symlink(&cycle_b, &cycle_a)?;
    create_file_symlink(&cycle_a, &cycle_b)?;
    require_rejected_source_without_remote_write(
        fixture,
        "files.symlink.cycle_rejected",
        cycle_a,
        "symlink-cycle-denied.txt",
    )
    .await
}

/// 从真实远端目录 stat 结果读取单个 fixture 文件，缺少条目时不伪造时间证据。
///
/// @author kongweiguang
async fn remote_file_entry(
    fixture: &SftpAcceptanceFixture,
    name: &str,
) -> AcceptanceResult<FileEntry> {
    let listing = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    match listing.entries.into_iter().find(|entry| entry.name == name) {
        Some(entry) => Ok(entry),
        None => Err(io::Error::other("remote SFTP fixture file is missing").into()),
    }
}

/// 解析 SFTP FileEntry 中由服务端 stat 返回的纳秒表示；缺失时间不能算作策略通过。
///
/// @author kongweiguang
fn remote_mtime_seconds(entry: &FileEntry) -> AcceptanceResult<u64> {
    let modified = entry
        .modified
        .as_deref()
        .ok_or_else(|| io::Error::other("OpenSSH stat did not return mtime"))?;
    let nanoseconds = modified.parse::<u64>()?;
    Ok(nanoseconds / 1_000_000_000)
}

/// 失败传输必须零提交且不新增目标目录项，防止错误回执掩盖远端 staging/最终写入。
///
/// @author kongweiguang
async fn require_rejected_source_without_remote_write(
    fixture: &SftpAcceptanceFixture,
    stage: &'static str,
    source: std::path::PathBuf,
    destination_name: &str,
) -> AcceptanceResult {
    let destination = remote_child(&fixture.host_id, &fixture.workspace, destination_name)?;
    let before = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    let initial = fixture
        .transfers
        .enqueue(request(
            FileEndpoint::Local { path: source },
            destination,
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ))
        .await?;
    let summary = wait_for_terminal(&fixture.transfers, &initial.id, stage).await?;
    if summary.cleanup_pending {
        fixture
            .transfers
            .retry_cleanup_until(
                &summary.id,
                tokio::time::Instant::now() + Duration::from_secs(20),
            )
            .await?;
    }
    let summary = fixture.transfers.get(&initial.id).await?;
    require(
        summary.status == TransferStatus::Failed
            && summary.commit_summary.committed_entries() == 0
            && !summary.cleanup_pending,
        "rejected source symlink was committed or retained an unresolved cleanup owner",
    )?;
    let after = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    require(
        after
            .entries
            .iter()
            .all(|entry| before.iter().any(|name| name == &entry.name)),
        "rejected source symlink created a remote staging or destination entry",
    )?;
    require(
        !after
            .entries
            .iter()
            .any(|entry| entry.name == destination_name),
        "rejected source symlink wrote its requested remote destination",
    )
}

/// 远端源链接拒绝时核对提交回执、精确本机目标和整个夹具目录，避免遗漏本机 staging 残留。
///
/// @author kongweiguang
async fn require_rejected_remote_source_without_local_write(
    fixture: &SftpAcceptanceFixture,
    stage: &'static str,
    source: FileEndpoint,
    destination: std::path::PathBuf,
) -> AcceptanceResult {
    let before = fs::read_dir(fixture.local_root.path())?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    let initial = fixture
        .transfers
        .enqueue(request(
            source,
            FileEndpoint::Local {
                path: destination.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ))
        .await?;
    let summary = wait_for_terminal(&fixture.transfers, &initial.id, stage).await?;
    if summary.cleanup_pending {
        fixture
            .transfers
            .retry_cleanup_until(
                &summary.id,
                tokio::time::Instant::now() + Duration::from_secs(20),
            )
            .await?;
    }
    let summary = fixture.transfers.get(&initial.id).await?;
    require(
        summary.status == TransferStatus::Failed
            && summary.commit_summary.committed_entries() == 0
            && !summary.cleanup_pending,
        "rejected remote source symlink was committed or retained an unresolved cleanup owner",
    )?;
    require(
        !destination.exists(),
        "rejected remote source symlink wrote its requested local destination",
    )?;
    let after = fs::read_dir(fixture.local_root.path())?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    require(
        after
            .iter()
            .all(|name| before.iter().any(|existing| existing == name)),
        "rejected remote source symlink left a new local staging artifact",
    )
}

/// 按平台创建真实本机文件链接；权限不足直接使验收失败，不跳过策略覆盖。
///
/// @author kongweiguang
fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the SFTP acceptance platform cannot create source symlinks",
        ))
    }
}

/// 覆盖已有目录中的同名文件，同时确认 merge 保留权限、目标端独有项且不泄漏内部 marker。
///
/// @author kongweiguang
pub(super) async fn exercise_directory_merge(fixture: &SftpAcceptanceFixture) -> AcceptanceResult {
    let source = fixture.local_root.path().join("source-tree");
    fs::create_dir_all(source.join("nested"))?;
    fs::write(source.join("replace.txt"), b"new tree value")?;
    fs::write(source.join("nested").join("new.txt"), b"nested value")?;

    let destination = remote_child(&fixture.host_id, &fixture.workspace, "merged-tree")?;
    fixture
        .files
        .create_directory(CancellationToken::new(), destination.clone())
        .await?;
    let destination_path = destination_path(&destination)?;
    write_remote_text(fixture, &destination_path, "preserve.txt", "target-only").await?;
    write_remote_text(fixture, &destination_path, "replace.txt", "old value").await?;
    let existing_listing = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    let existing_directory_permissions = existing_listing
        .entries
        .iter()
        .find(|entry| entry.name == "merged-tree")
        .and_then(|entry| entry.permissions)
        .ok_or_else(|| {
            io::Error::other("SFTP fixture did not return existing directory permissions")
        })?;
    let existing_file_permissions = fixture
        .files
        .list_directory(CancellationToken::new(), destination.clone())
        .await?
        .entries
        .into_iter()
        .find(|entry| entry.name == "replace.txt")
        .and_then(|entry| entry.permissions)
        .ok_or_else(|| io::Error::other("SFTP fixture did not return existing file permissions"))?;

    let summary = run_transfer(
        fixture,
        "files.directory_merge.transfer",
        request(
            FileEndpoint::Local { path: source },
            destination.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    if summary.status != TransferStatus::Succeeded {
        return Err(io::Error::other(format!(
            "directory merge ended as {:?}; failure={:?}; retryable={}; resumable={}; bytes={}/{:?}; confirmed_entries={}; confirmed_bytes={}; cleanup_pending={}; safe_error={}",
            summary.status,
            summary.failure_kind,
            summary.retryable,
            summary.resumable,
            summary.bytes_transferred,
            summary.total_bytes,
            summary.commit_summary.committed_entries(),
            summary.commit_summary.committed_bytes(),
            summary.cleanup_pending,
            summary.safe_error.as_deref().unwrap_or("<none>")
        ))
        .into());
    }
    require(
        summary.commit_summary.committed_entries() >= 2,
        "directory merge did not report committed entries",
    )?;
    require(
        summary.total_bytes == Some(26)
            && summary.bytes_transferred == 26
            && summary.commit_summary.committed_bytes() == 26,
        "directory progress or commit receipts did not use the cumulative tree byte count",
    )?;
    let top = fixture
        .files
        .list_directory(CancellationToken::new(), destination.clone())
        .await?;
    let current_listing = fixture
        .files
        .list_directory(
            CancellationToken::new(),
            remote_endpoint(&fixture.host_id, &fixture.workspace),
        )
        .await?;
    let current_directory_permissions = current_listing
        .entries
        .iter()
        .find(|entry| entry.name == "merged-tree")
        .and_then(|entry| entry.permissions);
    require(
        current_directory_permissions == Some(existing_directory_permissions),
        "directory merge changed permissions on its existing target directory",
    )?;
    require(
        top.entries.iter().any(|entry| entry.name == "preserve.txt"),
        "directory merge removed a target-only file",
    )?;
    require(
        !top.entries.iter().any(|entry| {
            entry.name.contains(".kerminal-owner-") || entry.name.ends_with(".partial")
        }),
        "directory merge exposed an internal staging artifact",
    )?;
    let replaced_entry = top
        .entries
        .iter()
        .find(|entry| entry.name == "replace.txt")
        .ok_or_else(|| io::Error::other("merged replacement file is missing from listing"))?;
    require(
        replaced_entry.permissions == Some(existing_file_permissions),
        "directory merge changed permissions on an existing replacement file",
    )?;
    let replaced = fixture
        .files
        .preview_file(
            CancellationToken::new(),
            remote_child(&fixture.host_id, &destination_path, "replace.txt")?,
            64,
        )
        .await?;
    require(
        replaced.bytes.as_slice() == b"new tree value",
        "directory merge did not replace the matching file",
    )?;
    let nested = remote_child(&fixture.host_id, &destination_path, "nested")?;
    let nested_entries = fixture
        .files
        .list_directory(CancellationToken::new(), nested)
        .await?;
    let nested_directory = top
        .entries
        .iter()
        .find(|entry| entry.name == "nested")
        .ok_or_else(|| io::Error::other("new nested directory is missing from listing"))?;
    require(
        nested_directory
            .permissions
            .is_some_and(|mode| mode & 0o111 != 0),
        "new directory lost POSIX traversal permissions",
    )?;
    require(
        !nested_entries.entries.iter().any(|entry| {
            entry.name.contains(".kerminal-owner-") || entry.name.ends_with(".partial")
        }),
        "nested merge exposed an internal staging artifact",
    )?;
    require(
        nested_entries
            .entries
            .iter()
            .any(|entry| entry.name == "new.txt"),
        "nested directory content was not transferred",
    )?;
    Ok(())
}

/// 打开真实远端文档后制造 revision 变化，确认编辑保存拒绝覆盖并保留外部内容。
pub(super) async fn exercise_editor_revision_conflict(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    let document_endpoint = remote_child(&fixture.host_id, &fixture.workspace, "editor.txt")?;
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: document_endpoint.clone(),
                content: "baseline\n".to_owned(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await?;
    let editor = FileEditorService::new(fixture.files.clone());
    let mut session = editor
        .open(document_endpoint.clone(), CancellationToken::new())
        .await?;
    let stale_revision: FileRevision = session.revision().clone();
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: document_endpoint.clone(),
                content: "external change\n".to_owned(),
                strategy: WriteTextStrategy::ExplicitOverwrite,
            },
        )
        .await?;
    let command = session
        .begin_save("editor change\n", SaveResolution::CompareRevision)
        .map_err(|_| io::Error::other("editor did not create a compare-revision command"))?;
    require(
        matches!(command.strategy, WriteTextStrategy::CompareRevision(ref revision) if revision == &stale_revision),
        "editor save did not retain the opened revision",
    )?;
    let error = match editor.save(command, CancellationToken::new()).await {
        Err(error) => error,
        Ok(_) => return Err(io::Error::other("stale editor save unexpectedly succeeded").into()),
    };
    require(
        error.public_code() == PublicErrorCode::Conflict,
        "stale editor save returned an unexpected error category",
    )?;
    let current = fixture
        .files
        .read_text_file(CancellationToken::new(), document_endpoint, 1024)
        .await?;
    require(
        current.content == "external change\n",
        "revision conflict overwrote the external edit",
    )?;
    require(
        !current.truncated,
        "small remote text was unexpectedly truncated",
    )?;
    require(
        current.line_ending == LineEnding::Lf,
        "LF file was reported with another line ending",
    )?;
    Ok(())
}
