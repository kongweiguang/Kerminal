//! 无 replace rename 与缺省属性下的实际 SFTP 上传兼容测试。
//!
//! @author kongweiguang

use super::*;

/// Exercise the compatibility path against an actual loopback SFTP server that refuses overwrite.
#[tokio::test]
async fn native_upload_overwrites_without_posix_rename_and_preserves_old_part() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let name = "replace-on-no-overwrite-server.bin";
    let source = client_root.path().join(name);
    let payload = b"replacement payload from the local source";
    fs::write(&source, payload).await.expect("source");
    fs::write(server_root.path().join(name), b"original remote contents")
        .await
        .expect("seed original target");
    let old_partial = server_root.path().join(format!("{name}.kerminal-part"));
    fs::write(&old_partial, b"untrusted partial from another task")
        .await
        .expect("seed orphan partial");

    let server =
        start_loopback_sftp_server_without_overwrite(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "no-overwrite", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: format!("/{name}"),
                local_path: source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("backup fallback must support a server without replace rename");

    assert_eq!(
        fs::read(server_root.path().join(name)).await.unwrap(),
        payload
    );
    assert_eq!(
        fs::read(&old_partial).await.unwrap(),
        b"untrusted partial from another task"
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(
        names,
        vec![name.to_owned(), format!("{name}.kerminal-part")]
    );
}

/// Keep a near-limit remote basename valid by selecting a short task-owned partial before STAT.
#[tokio::test]
async fn native_upload_near_max_filename_uses_short_partial_on_no_overwrite_server() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let name = format!("{}.txt", "x".repeat(248));
    assert_eq!(
        name.len(),
        252,
        "the final basename remains below 255 bytes"
    );
    let source = client_root.path().join("replacement.txt");
    let payload = b"replacement for a near-limit remote filename";
    fs::write(&source, payload).await.expect("source");
    fs::write(server_root.path().join(&name), b"original remote contents")
        .await
        .expect("seed original target");

    let server =
        start_loopback_sftp_server_without_overwrite(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "long-name-no-overwrite", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: format!("/{name}"),
                local_path: source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("short UUID partial must permit fallback near the server filename limit");

    assert_eq!(
        fs::read(server_root.path().join(&name)).await.unwrap(),
        payload
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    assert_eq!(names, vec![name], "only the committed final should remain");
}

/// Servers may omit POSIX permissions entirely; a task-owned partial must still support creation,
/// empty-file completion, and direct overwrite while byte-size verification remains authoritative.
#[tokio::test]
async fn native_upload_accepts_missing_permission_attributes() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "omit-mode", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    for (name, payload, previous) in [
        ("new-omit-mode.txt", b"new file bytes".as_slice(), None),
        ("empty-omit-mode.txt", b"".as_slice(), None),
        (
            "overwrite-omit-mode.txt",
            b"replacement bytes".as_slice(),
            Some(b"original bytes".as_slice()),
        ),
    ] {
        let source = client_root.path().join(name);
        fs::write(&source, payload).await.expect("write source");
        if let Some(previous) = previous {
            fs::write(server_root.path().join(name), previous)
                .await
                .expect("seed overwrite target");
        }

        state
            .sftp()
            .upload(
                state.paths(),
                SftpTransferRequest {
                    host_id: host_id.clone(),
                    remote_path: format!("/{name}"),
                    local_path: source.to_string_lossy().into_owned(),
                    conflict_policy: SftpTransferConflictPolicy::Overwrite,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("upload with omitted mode failed for {name}: {error}"));

        assert_eq!(
            fs::read(server_root.path().join(name))
                .await
                .expect("read committed file"),
            payload,
            "omitted mode must not prevent verified commit for {name}"
        );
        assert!(
            !server_root
                .path()
                .join(format!("{name}.kerminal-part"))
                .exists(),
            "successful upload removes only its owned partial"
        );
    }
}

/// Permission bits without S_IFREG cannot prove an existing target is a regular file; a server
/// that refuses replace rename must fail safely without moving the target or deleting the partial.
#[tokio::test]
async fn native_upload_preserves_target_when_type_bits_are_missing() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let name = "existing-mode-only.txt";
    let source = client_root.path().join("replacement.txt");
    let payload = b"replacement content that must remain recoverable";
    fs::write(&source, payload).await.expect("write source");
    fs::write(server_root.path().join(name), b"original target contents")
        .await
        .expect("seed original target");

    let server =
        start_loopback_sftp_server_without_overwrite(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "mode-only", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let error = state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: format!("/{name}"),
                local_path: source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect_err("unknown target type must not enter a backup transaction");

    assert!(error.to_string().contains("无法确认目标"), "{error}");
    assert!(
        error.to_string().contains("原目标与 partial 保留"),
        "{error}"
    );
    assert_eq!(
        fs::read(server_root.path().join(name)).await.unwrap(),
        b"original target contents",
        "lack of type metadata must preserve the original target"
    );
    assert_eq!(
        fs::read(server_root.path().join(format!("{name}.kerminal-part")))
            .await
            .unwrap(),
        payload,
        "the complete partial remains available for recovery"
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(
        names,
        vec![name.to_owned(), format!("{name}.kerminal-part")],
        "unknown type must not create or clean a backup path"
    );
}

/// A directory is never an overwrite candidate; the uploader must leave it and the completed partial intact.
#[tokio::test]
async fn native_upload_rejects_directory_target_without_mutation() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let source = client_root.path().join("source.txt");
    fs::write(&source, b"new file contents")
        .await
        .expect("source");
    let directory = server_root.path().join("target-directory");
    fs::create_dir(&directory).await.expect("target directory");
    fs::write(directory.join("keep.txt"), b"keep")
        .await
        .expect("directory sentinel");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "directory-target", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let error = state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/target-directory".to_owned(),
                local_path: source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect_err("directory targets are not regular files");

    assert!(error.to_string().contains("目标"), "{error}");
    assert_eq!(fs::read(directory.join("keep.txt")).await.unwrap(), b"keep");
    assert_eq!(
        fs::read(server_root.path().join("target-directory.kerminal-part"))
            .await
            .unwrap(),
        b"new file contents"
    );
}

/// SFTP lstat must distinguish a symlink from its target before commit; this fixture maps the link virtually.
#[tokio::test]
async fn native_upload_rejects_symlink_target_without_mutating_link_target() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let source = client_root.path().join("source.txt");
    fs::write(&source, b"new symlink content")
        .await
        .expect("source");
    fs::write(server_root.path().join("actual.txt"), b"protected target")
        .await
        .expect("symlink target");
    let server = start_loopback_sftp_server_with_symlinks(
        server_root.path().to_path_buf(),
        vec![("/target-link".to_owned(), "/actual.txt".to_owned())],
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "symlink-target", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    let error = state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id,
                remote_path: "/target-link".to_owned(),
                local_path: source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect_err("symlink targets must be rejected before rename");

    assert!(error.to_string().contains("目标"), "{error}");
    assert_eq!(
        fs::read(server_root.path().join("actual.txt"))
            .await
            .unwrap(),
        b"protected target"
    );
    assert_eq!(
        fs::read(server_root.path().join("target-link.kerminal-part"))
            .await
            .unwrap(),
        b"new symlink content"
    );
}
