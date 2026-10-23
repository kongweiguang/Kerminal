//! @author kongweiguang

use super::support::{
    create_password_remote_host, create_password_remote_host_without_credentials,
    loopback::{
        start_loopback_sftp_only_server, start_loopback_sftp_server,
        start_loopback_sftp_server_with_symlinks, start_loopback_sftp_server_without_overwrite,
    },
    test_state,
};
use kerminal_lib::{
    error::AppError,
    models::{
        remote_host::{
            RemoteHostAuthType, RemoteHostCreateRequest, RemoteHostProtocol, SshOptions,
        },
        sftp::{
            SftpArchiveDownloadRequest, SftpChmodRequest, SftpDeleteRequest, SftpEntryKind,
            SftpListDirectoryRequest, SftpPathRequest, SftpPreviewRequest, SftpReadTextFileRequest,
            SftpRemoteCopyRequest, SftpRenameRequest, SftpTransferConflictPolicy, SftpTransferKind,
            SftpTransferRequest, SftpTransferStatus, SftpTrustHostKeyRequest,
            SftpWriteTextFileRequest,
        },
        ssh_command::SshCommandRequest,
    },
    services::ssh_runtime::{auth_broker::SshSessionSecretInput, SshAuthSecretKind},
    state::AppState,
};
use std::{fs::File as StdFile, io::Read};
use tempfile::tempdir;
use tokio::{
    fs,
    time::{sleep, Duration},
};

#[path = "native_backend/protocol.rs"]
mod protocol;
#[tokio::test]
async fn native_sftp_service_uses_session_only_password_from_auth_broker() {
    let server_root = tempdir().expect("server root");
    fs::write(
        server_root.path().join("session-only.txt"),
        b"session-only credential",
    )
    .await
    .expect("seed remote file");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id =
        create_password_remote_host_without_credentials(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let prompt_id = format!(
        "ssh-auth:target:deploy@127.0.0.1:{}:password",
        server.addr.port()
    );
    state
        .ssh_auth_broker()
        .remember_session_secret(SshSessionSecretInput {
            prompt_id,
            secret_kind: SshAuthSecretKind::Password,
            value: "secret".to_owned(),
        })
        .expect("remember session-only password");

    let listing = state
        .sftp()
        .list_directory(
            state.paths(),
            SftpListDirectoryRequest {
                host_id,
                path: "/".to_owned(),
            },
        )
        .await
        .expect("list with session-only password");

    assert!(listing
        .entries
        .iter()
        .any(|entry| entry.name == "session-only.txt" && entry.kind == SftpEntryKind::File));
}

#[tokio::test]
async fn native_sftp_service_reads_and_writes_text_files_with_revision_conflict() {
    let server_root = tempdir().expect("server root");
    fs::write(server_root.path().join("config.toml"), b"port = 8080\n")
        .await
        .expect("seed remote config");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let opened = state
        .sftp()
        .read_text_file(
            state.paths(),
            SftpReadTextFileRequest {
                host_id: host_id.clone(),
                path: "/config.toml".to_owned(),
                max_bytes: Some(4096),
            },
        )
        .await
        .expect("read remote text file");
    assert_eq!(opened.content, "port = 8080\n");
    assert_eq!(opened.line_ending, "lf");
    assert!(!opened.readonly);
    assert!(opened.revision.content_sha256.is_some());

    fs::write(server_root.path().join("config.toml"), b"port = 9090\n")
        .await
        .expect("simulate external remote write");

    let stale_save = state
        .sftp()
        .write_text_file(
            state.paths(),
            SftpWriteTextFileRequest {
                host_id: host_id.clone(),
                path: "/config.toml".to_owned(),
                content: "port = 7070\n".to_owned(),
                encoding: "utf-8".to_owned(),
                expected_revision: Some(opened.revision.clone()),
                create: false,
                overwrite_on_conflict: false,
            },
        )
        .await
        .expect_err("reject stale revision save");
    assert!(matches!(stale_save, AppError::Sftp(message) if message.contains("远端文件已变更")));
    assert_eq!(
        fs::read_to_string(server_root.path().join("config.toml"))
            .await
            .expect("read conflicted file"),
        "port = 9090\n"
    );

    let saved = state
        .sftp()
        .write_text_file(
            state.paths(),
            SftpWriteTextFileRequest {
                host_id: host_id.clone(),
                path: "/config.toml".to_owned(),
                content: "port = 7070\n".to_owned(),
                encoding: "utf-8".to_owned(),
                expected_revision: Some(opened.revision),
                create: false,
                overwrite_on_conflict: true,
            },
        )
        .await
        .expect("overwrite stale revision");
    assert_eq!(saved.bytes_written, "port = 7070\n".len());
    assert_eq!(
        fs::read_to_string(server_root.path().join("config.toml"))
            .await
            .expect("read overwritten file"),
        "port = 7070\n"
    );

    let stat = state
        .sftp()
        .stat_path(
            state.paths(),
            SftpPathRequest {
                host_id,
                path: "/config.toml".to_owned(),
            },
        )
        .await
        .expect("stat saved text file");
    assert_eq!(stat.kind, SftpEntryKind::File);
    assert_eq!(
        stat.revision
            .expect("file revision")
            .content_sha256
            .expect("stat sha"),
        saved.revision.content_sha256.expect("saved sha")
    );
}

#[tokio::test]
async fn native_sftp_service_transfers_large_files_with_default_settings() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let upload_source = client_root.path().join("large-upload.bin");
    let download_target = client_root.path().join("large-download.bin");
    let payload = (0..(900 * 1024))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    fs::write(&upload_source, &payload)
        .await
        .expect("write large upload source");

    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id: host_id.clone(),
                remote_path: "/large.bin".to_owned(),
                local_path: upload_source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("upload large file with default settings");

    state
        .sftp()
        .download(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/large.bin".to_owned(),
                local_path: download_target.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("download large file with default settings");

    assert_eq!(
        fs::read(download_target)
            .await
            .expect("read large downloaded file"),
        payload
    );
}

#[tokio::test]
async fn native_sftp_service_downloads_directory_file_with_short_read_sized_chunk() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let remote_dir = server_root.path().join("jdk-21.0.2/bin");
    fs::create_dir_all(&remote_dir)
        .await
        .expect("seed remote directory");
    let payload = (0..76_018)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    fs::write(remote_dir.join("java"), &payload)
        .await
        .expect("seed remote short-read-sized file");

    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let download_root = client_root.path().join("jdk-21.0.2");
    state
        .sftp()
        .download_directory(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/jdk-21.0.2".to_owned(),
                local_path: download_root.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("download directory containing a 64KiB-short-read-sized file");

    assert_eq!(
        fs::read(download_root.join("bin/java"))
            .await
            .expect("read downloaded short-read-sized file"),
        payload
    );
}

#[tokio::test]
async fn native_sftp_service_streams_remote_copy_between_hosts() {
    let source_root = tempdir().expect("source server root");
    let target_root = tempdir().expect("target server root");
    let short_read_payload = (0..76_018)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    fs::write(source_root.path().join("artifact.txt"), &short_read_payload)
        .await
        .expect("seed source remote file");
    fs::create_dir_all(source_root.path().join("release/nested"))
        .await
        .expect("seed source nested directory");
    fs::write(
        source_root.path().join("release/README.md"),
        b"release notes",
    )
    .await
    .expect("seed source release readme");
    fs::write(
        source_root.path().join("release/nested/app.log"),
        b"nested streamed remote copy",
    )
    .await
    .expect("seed source nested release file");

    let source_server = start_loopback_sftp_server(source_root.path().to_path_buf()).await;
    let target_server = start_loopback_sftp_server(target_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let source_host_id =
        create_password_remote_host(&state, "source loopback", source_server.addr.port());
    let target_host_id =
        create_password_remote_host(&state, "target loopback", target_server.addr.port());
    trust_loopback_host(&state, &source_host_id).await;
    trust_loopback_host(&state, &target_host_id).await;

    let summary = state
        .sftp()
        .enqueue_remote_copy(
            state.paths(),
            SftpRemoteCopyRequest {
                source_host_id: source_host_id.clone(),
                source_remote_path: "/artifact.txt".to_owned(),
                target_host_id: target_host_id.clone(),
                target_remote_path: "/copied.txt".to_owned(),
                kind: SftpTransferKind::File,
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
                view_scope: None,
                idle_timeout_seconds: None,
            },
        )
        .expect("enqueue remote copy over real SFTP");
    wait_for_transfer_success(&state, &summary.id).await;
    assert_eq!(
        fs::read(target_root.path().join("copied.txt"))
            .await
            .expect("read copied target file"),
        short_read_payload
    );

    fs::write(
        source_root.path().join("artifact.txt"),
        b"renamed remote copy",
    )
    .await
    .expect("update source artifact");
    let summary = state
        .sftp()
        .enqueue_remote_copy(
            state.paths(),
            SftpRemoteCopyRequest {
                source_host_id: source_host_id.clone(),
                source_remote_path: "/artifact.txt".to_owned(),
                target_host_id: target_host_id.clone(),
                target_remote_path: "/copied.txt".to_owned(),
                kind: SftpTransferKind::File,
                conflict_policy: SftpTransferConflictPolicy::Skip,
                view_scope: None,
                idle_timeout_seconds: None,
            },
        )
        .expect("enqueue skip conflicting remote copy target");
    wait_for_transfer_success(&state, &summary.id).await;
    assert_eq!(
        fs::read(target_root.path().join("copied.txt"))
            .await
            .expect("read skipped target file"),
        short_read_payload
    );

    let summary = state
        .sftp()
        .enqueue_remote_copy(
            state.paths(),
            SftpRemoteCopyRequest {
                source_host_id: source_host_id.clone(),
                source_remote_path: "/artifact.txt".to_owned(),
                target_host_id: target_host_id.clone(),
                target_remote_path: "/copied.txt".to_owned(),
                kind: SftpTransferKind::File,
                conflict_policy: SftpTransferConflictPolicy::Rename,
                view_scope: None,
                idle_timeout_seconds: None,
            },
        )
        .expect("enqueue rename conflicting remote copy target");
    wait_for_transfer_success(&state, &summary.id).await;
    assert_eq!(
        fs::read_to_string(target_root.path().join("copied (1).txt"))
            .await
            .expect("read renamed target file"),
        "renamed remote copy"
    );

    let summary = state
        .sftp()
        .enqueue_remote_copy(
            state.paths(),
            SftpRemoteCopyRequest {
                source_host_id,
                source_remote_path: "/release".to_owned(),
                target_host_id,
                target_remote_path: "/release-copy".to_owned(),
                kind: SftpTransferKind::Directory,
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
                view_scope: None,
                idle_timeout_seconds: None,
            },
        )
        .expect("enqueue remote directory copy over real SFTP");
    wait_for_transfer_success(&state, &summary.id).await;
    assert_eq!(
        fs::read_to_string(target_root.path().join("release-copy/README.md"))
            .await
            .expect("read copied directory readme"),
        "release notes"
    );
    assert_eq!(
        fs::read_to_string(target_root.path().join("release-copy/nested/app.log"))
            .await
            .expect("read copied nested target file"),
        "nested streamed remote copy"
    );
}

#[tokio::test]
async fn native_sftp_service_falls_back_to_directory_for_file_requests() {
    let source_root = tempdir().expect("source server root");
    let target_root = tempdir().expect("target server root");
    let client_root = tempdir().expect("client root");
    fs::create_dir_all(source_root.path().join("release/nested"))
        .await
        .expect("seed source nested directory");
    fs::write(
        source_root.path().join("release/README.md"),
        b"release notes",
    )
    .await
    .expect("seed source readme");
    fs::write(
        source_root.path().join("release/nested/app.log"),
        b"nested file request fallback",
    )
    .await
    .expect("seed source nested file");

    let source_server = start_loopback_sftp_server(source_root.path().to_path_buf()).await;
    let target_server = start_loopback_sftp_server(target_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let source_host_id =
        create_password_remote_host(&state, "source loopback", source_server.addr.port());
    let target_host_id =
        create_password_remote_host(&state, "target loopback", target_server.addr.port());
    trust_loopback_host(&state, &source_host_id).await;
    trust_loopback_host(&state, &target_host_id).await;

    let download_target = client_root.path().join("release-download");
    state
        .sftp()
        .download(
            state.paths(),
            SftpTransferRequest {
                host_id: source_host_id.clone(),
                remote_path: "/release".to_owned(),
                local_path: download_target.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("download directory through file request fallback");
    assert_eq!(
        fs::read_to_string(download_target.join("README.md"))
            .await
            .expect("read downloaded readme"),
        "release notes"
    );
    assert_eq!(
        fs::read_to_string(download_target.join("nested/app.log"))
            .await
            .expect("read downloaded nested file"),
        "nested file request fallback"
    );

    let summary = state
        .sftp()
        .enqueue_remote_copy(
            state.paths(),
            SftpRemoteCopyRequest {
                source_host_id,
                source_remote_path: "/release".to_owned(),
                target_host_id,
                target_remote_path: "/release-copy".to_owned(),
                kind: SftpTransferKind::File,
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
                view_scope: None,
                idle_timeout_seconds: None,
            },
        )
        .expect("enqueue remote copy directory through file request fallback");
    wait_for_transfer_success(&state, &summary.id).await;
    assert_eq!(
        fs::read_to_string(target_root.path().join("release-copy/README.md"))
            .await
            .expect("read copied readme"),
        "release notes"
    );
    assert_eq!(
        fs::read_to_string(target_root.path().join("release-copy/nested/app.log"))
            .await
            .expect("read copied nested file"),
        "nested file request fallback"
    );
}

#[tokio::test]
async fn native_sftp_service_downloads_directory_symlink_from_file_request() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    fs::create_dir_all(server_root.path().join("usr/lib32/pkg"))
        .await
        .expect("seed linked directory");
    fs::write(
        server_root.path().join("usr/lib32/pkg/libsample.so"),
        b"linked directory payload",
    )
    .await
    .expect("seed linked directory file");

    let server = start_loopback_sftp_server_with_symlinks(
        server_root.path().to_path_buf(),
        vec![("/lib32".to_owned(), "/usr/lib32".to_owned())],
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let download_target = client_root.path().join("lib32");
    state
        .sftp()
        .download(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/lib32".to_owned(),
                local_path: download_target.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("download directory symlink through file request fallback");
    assert_eq!(
        fs::read_to_string(download_target.join("pkg/libsample.so"))
            .await
            .expect("read downloaded linked directory file"),
        "linked directory payload"
    );
}

#[tokio::test]
async fn native_sftp_service_honors_directory_transfer_conflict_policies() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    fs::create_dir_all(server_root.path().join("release/nested"))
        .await
        .expect("seed remote directory");
    fs::write(server_root.path().join("release/nested/new.txt"), b"remote")
        .await
        .expect("seed remote file");
    let download_target = client_root.path().join("release");
    fs::create_dir_all(download_target.join("nested"))
        .await
        .expect("seed local directory");
    fs::write(download_target.join("nested/existing.txt"), b"local")
        .await
        .expect("seed local file");

    let upload_source = client_root.path().join("upload-release");
    fs::create_dir_all(upload_source.join("nested"))
        .await
        .expect("seed upload source directory");
    fs::write(upload_source.join("nested/new.txt"), b"new upload")
        .await
        .expect("seed upload source file");
    fs::create_dir_all(server_root.path().join("upload-release/nested"))
        .await
        .expect("seed existing remote directory");
    fs::write(
        server_root
            .path()
            .join("upload-release/nested/existing.txt"),
        b"existing remote",
    )
    .await
    .expect("seed existing remote file");

    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    state
        .sftp()
        .download_directory(
            state.paths(),
            SftpTransferRequest {
                host_id: host_id.clone(),
                remote_path: "/release".to_owned(),
                local_path: download_target.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Skip,
            },
        )
        .await
        .expect("skip existing local directory root");
    assert_eq!(
        fs::read_to_string(download_target.join("nested/existing.txt"))
            .await
            .expect("read existing local file"),
        "local"
    );
    assert!(!download_target.join("nested/new.txt").exists());

    state
        .sftp()
        .upload_directory(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/upload-release".to_owned(),
                local_path: upload_source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Rename,
            },
        )
        .await
        .expect("upload directory with renamed remote root");
    assert_eq!(
        fs::read_to_string(
            server_root
                .path()
                .join("upload-release/nested/existing.txt")
        )
        .await
        .expect("read existing remote file"),
        "existing remote"
    );
    assert_eq!(
        fs::read_to_string(server_root.path().join("upload-release (1)/nested/new.txt"))
            .await
            .expect("read renamed remote upload"),
        "new upload"
    );
}

#[tokio::test]
async fn archive_download_file_request_zips_directory_after_fallback() {
    let server_root = tempdir().expect("server root");
    let target_root = tempdir().expect("archive target root");
    fs::create_dir_all(server_root.path().join("release/nested"))
        .await
        .expect("seed remote nested directory");
    fs::write(
        server_root.path().join("release/README.md"),
        b"release notes",
    )
    .await
    .expect("seed remote readme");
    fs::write(
        server_root.path().join("release/nested/app.log"),
        b"archive fallback nested file",
    )
    .await
    .expect("seed remote nested file");

    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let target_zip = target_root.path().join("release.zip");
    let summary = state
        .sftp()
        .enqueue_archive_download(
            state.paths(),
            SftpArchiveDownloadRequest {
                host_id,
                source_remote_path: "/release".to_owned(),
                target_local_path: target_zip.to_string_lossy().into_owned(),
                kind: SftpTransferKind::File,
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
                view_scope: None,
            },
        )
        .expect("enqueue archive download");

    wait_for_transfer_success(&state, &summary.id).await;

    let file = StdFile::open(&target_zip).expect("open archive zip");
    let mut archive = zip::ZipArchive::new(file).expect("read archive zip");
    let mut readme = String::new();
    archive
        .by_name("release/README.md")
        .expect("read archived readme")
        .read_to_string(&mut readme)
        .expect("read archived readme contents");
    assert_eq!(readme, "release notes");
    assert!(archive.by_name("release/nested/app.log").is_ok());
}

async fn trust_loopback_host(state: &AppState, host_id: &str) {
    state
        .sftp()
        .trust_host_key(
            state.paths(),
            SftpTrustHostKeyRequest {
                host_id: host_id.to_owned(),
            },
        )
        .await
        .expect("trust loopback host key");
}

async fn wait_for_transfer_success(state: &AppState, transfer_id: &str) {
    for _ in 0..100 {
        let tasks = state.sftp().list_transfers().expect("list transfers");
        if let Some(task) = tasks.iter().find(|task| task.id == transfer_id) {
            match task.status {
                SftpTransferStatus::Succeeded => return,
                SftpTransferStatus::Failed | SftpTransferStatus::Canceled => {
                    panic!(
                        "transfer {transfer_id} finished as {:?}: {:?}",
                        task.status, task.error
                    );
                }
                SftpTransferStatus::Queued | SftpTransferStatus::Running => {}
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("transfer {transfer_id} did not finish");
}
