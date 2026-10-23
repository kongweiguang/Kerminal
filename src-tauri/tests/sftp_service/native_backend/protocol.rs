// @author kongweiguang

use super::*;

/// 旧任务残留不属于新任务；空、非空及超长残留均保留，长文件名也不能因隔离后缀而失败。
#[tokio::test]
async fn native_upload_isolates_untrusted_partial_without_changing_final_name() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    for (index, (policy, old_partial, contents)) in [
        (
            SftpTransferConflictPolicy::Overwrite,
            b"".as_slice(),
            b"new".as_slice(),
        ),
        (
            SftpTransferConflictPolicy::Overwrite,
            b"untrusted old bytes".as_slice(),
            b"new".as_slice(),
        ),
        (
            SftpTransferConflictPolicy::Skip,
            b"old".as_slice(),
            b"new".as_slice(),
        ),
        (
            SftpTransferConflictPolicy::Rename,
            b"old".as_slice(),
            b"new".as_slice(),
        ),
        (
            SftpTransferConflictPolicy::Overwrite,
            b"old".as_slice(),
            b"".as_slice(),
        ),
        (
            SftpTransferConflictPolicy::Overwrite,
            b"old".as_slice(),
            b"new".as_slice(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let name = if index == 5 {
            format!("{}.bin", "l".repeat(220))
        } else {
            format!("reject-fsetstat-orphan-{index}.bin")
        };
        let source = client_root.path().join(&name);
        let partial = server_root.path().join(format!("{name}.kerminal-part"));
        fs::write(&source, contents).await.expect("source");
        fs::write(&partial, old_partial)
            .await
            .expect("orphan partial");
        state
            .sftp()
            .upload(
                state.paths(),
                SftpTransferRequest {
                    host_id: host_id.clone(),
                    remote_path: format!("/{name}"),
                    local_path: source.to_string_lossy().into_owned(),
                    conflict_policy: policy,
                },
            )
            .await
            .expect("orphan must not block a fresh upload");
        assert_eq!(
            fs::read(server_root.path().join(&name))
                .await
                .expect("final"),
            contents
        );
        assert_eq!(
            fs::read(&partial).await.expect("preserved orphan"),
            old_partial
        );
    }
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut count = 0;
    while entries.next_entry().await.expect("entry").is_some() {
        count += 1;
    }
    assert_eq!(
        count, 12,
        "only six final files and six untouched orphans remain"
    );
}

/// 新文件与空文件只要求创建和写入；禁止 FSETSTAT 的服务端仍须完成大小确认与提交。
#[tokio::test]
async fn native_upload_does_not_require_fsetstat_for_new_partial() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    for (name, contents) in [
        (
            "reject-fsetstat.txt",
            b"upload without changing attributes".as_slice(),
        ),
        ("reject-fsetstat-empty.txt", b"".as_slice()),
    ] {
        let source = client_root.path().join(name);
        fs::write(&source, contents).await.expect("local source");
        fs::write(server_root.path().join(name), b"old final")
            .await
            .expect("existing final stays safe until commit");
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
            .expect("fresh upload needs no FSETSTAT");
        assert_eq!(
            fs::read(server_root.path().join(name))
                .await
                .expect("committed file"),
            contents
        );
        assert!(!server_root
            .path()
            .join(format!("{name}.kerminal-part"))
            .exists());
    }
}

/// 无 replace rename 与缺省属性下的实际 SFTP 上传兼容测试。
#[path = "protocol/commit_compat.rs"]
mod commit_compat;

/// 同样的服务端状态必须带上失败操作，不能要求用户从 0% 猜测 STAT 还是 OPEN 被拒绝。
#[tokio::test]
async fn native_upload_permission_error_identifies_remote_operation() {
    let server_root = tempdir().expect("server root");
    let client_root = tempdir().expect("client root");
    let source = client_root.path().join("source.txt");
    fs::write(&source, b"diagnostic probe")
        .await
        .expect("local source");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;

    for (name, operation) in [
        ("reject-upload-stat.txt", "检查正式目标文件（STAT）"),
        ("reject-upload-open.txt", "创建上传临时文件（OPEN）"),
    ] {
        let error = state
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
            .expect_err("server rejects the selected operation");
        let message = error.to_string();
        assert!(message.contains(operation), "{message}");
        assert!(message.contains("Permission denied"), "{message}");
        assert!(
            !message.contains(&host_id),
            "diagnostics must not expose host ids"
        );
        assert!(!server_root.path().join(name).exists());
    }
}

#[tokio::test]
async fn sftp_only_host_browses_files_and_rejects_command_before_transport() {
    let server_root = tempdir().expect("server root");
    fs::write(server_root.path().join("sftp-only.txt"), b"files only")
        .await
        .expect("seed SFTP-only file");
    fs::create_dir_all(server_root.path().join("delete-me/nested"))
        .await
        .expect("seed nested SFTP-only directory");
    fs::write(
        server_root.path().join("delete-me/nested/file.txt"),
        b"delete through SFTP",
    )
    .await
    .expect("seed nested SFTP-only file");
    let server = start_loopback_sftp_only_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host = state
        .remote_hosts()
        .create_host(RemoteHostCreateRequest {
            auth_type: RemoteHostAuthType::Password,
            credential_ref: None,
            credential_secret: Some("secret".to_owned()),
            group_id: None,
            host: "127.0.0.1".to_owned(),
            name: "sftp-only".to_owned(),
            port: server.addr.port(),
            protocol: RemoteHostProtocol::Sftp,
            ssh_options: SshOptions::default(),
            tags: vec!["sftp-only".to_owned()],
            username: "deploy".to_owned(),
        })
        .expect("create SFTP-only host");

    trust_loopback_host(&state, &host.id).await;
    let listing = state
        .sftp()
        .list_directory(
            state.paths(),
            SftpListDirectoryRequest {
                host_id: host.id.clone(),
                path: "/".to_owned(),
            },
        )
        .await
        .expect("browse SFTP-only host");
    assert!(listing
        .entries
        .iter()
        .any(|entry| entry.name == "sftp-only.txt"));

    state
        .sftp()
        .delete(
            state.paths(),
            SftpDeleteRequest {
                directory: true,
                host_id: host.id.clone(),
                path: "/delete-me".to_owned(),
            },
        )
        .await
        .expect("delete nested directory without shell exec");
    assert!(!server_root.path().join("delete-me").exists());

    let authenticated_before_command = server
        .auth_successes
        .load(std::sync::atomic::Ordering::SeqCst);
    let error = state
        .ssh_commands()
        .execute_native(
            state.paths(),
            SshCommandRequest {
                host_id: host.id,
                command: "true".to_owned(),
                timeout_seconds: Some(1),
                max_output_bytes: Some(256),
            },
        )
        .await
        .expect_err("SFTP-only host rejects SSH command");
    assert!(error.to_string().contains("host_capability_not_supported"));
    assert_eq!(
        server
            .auth_successes
            .load(std::sync::atomic::Ordering::SeqCst),
        authenticated_before_command,
        "capability rejection must happen before another SSH transport is opened"
    );
}

#[tokio::test]
/// 协议冒烟只验证全新目标的首次上传，不预置孤儿 partial 来暗示它可被信任为断点数据。
async fn native_sftp_service_uses_real_ssh_sftp_protocol() {
    let server_root = tempdir().expect("server root");
    fs::write(
        server_root.path().join("hello.txt"),
        b"hello from native loopback",
    )
    .await
    .expect("seed remote file");
    let server = start_loopback_sftp_server(server_root.path().to_path_buf()).await;
    let (_home, state) = test_state();
    let host_id = create_password_remote_host(&state, "loopback", server.addr.port());
    let known_hosts_path = state.paths().root.join("known_hosts");

    let strict_error = state
        .sftp()
        .list_directory(
            state.paths(),
            SftpListDirectoryRequest {
                host_id: host_id.clone(),
                path: "/".to_owned(),
            },
        )
        .await
        .expect_err("strict host key policy rejects unknown loopback key");
    assert!(matches!(strict_error, AppError::Sftp(_)));
    assert!(
        !known_hosts_path.exists(),
        "strict SFTP connection must not learn an unknown host key"
    );

    trust_loopback_host(&state, &host_id).await;
    assert!(
        known_hosts_path.exists(),
        "explicit trust should persist the loopback host key"
    );

    let listing = state
        .sftp()
        .list_directory(
            state.paths(),
            SftpListDirectoryRequest {
                host_id: host_id.clone(),
                path: "/".to_owned(),
            },
        )
        .await
        .expect("list real SFTP directory");
    assert!(listing
        .entries
        .iter()
        .any(|entry| entry.name == "hello.txt" && entry.kind == SftpEntryKind::File));

    let preview = state
        .sftp()
        .preview_file(
            state.paths(),
            SftpPreviewRequest {
                host_id: host_id.clone(),
                path: "/hello.txt".to_owned(),
                max_bytes: Some(256),
            },
        )
        .await
        .expect("preview real SFTP file");
    assert_eq!(preview.content, "hello from native loopback");
    assert!(!preview.truncated);

    state
        .sftp()
        .create_directory(
            state.paths(),
            SftpPathRequest {
                host_id: host_id.clone(),
                path: "/managed".to_owned(),
            },
        )
        .await
        .expect("create real SFTP directory");
    assert!(server_root.path().join("managed").is_dir());
    fs::create_dir_all(server_root.path().join("managed/nested"))
        .await
        .expect("create nested remote directory");
    fs::write(
        server_root.path().join("managed/nested/app.log"),
        b"nested content",
    )
    .await
    .expect("write nested remote file");

    let client_root = tempdir().expect("client root");
    let upload_source = client_root.path().join("upload-source.txt");
    fs::write(&upload_source, b"uploaded over native SFTP")
        .await
        .expect("write local upload source");
    state
        .sftp()
        .upload(
            state.paths(),
            SftpTransferRequest {
                host_id: host_id.clone(),
                remote_path: "/uploaded.txt".to_owned(),
                local_path: upload_source.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("upload over real SFTP");
    assert_eq!(
        fs::read_to_string(server_root.path().join("uploaded.txt"))
            .await
            .expect("read uploaded file"),
        "uploaded over native SFTP"
    );
    assert!(
        !server_root
            .path()
            .join("uploaded.txt.kerminal-part")
            .exists(),
        "successful upload should commit and remove the remote partial file"
    );

    state
        .sftp()
        .rename(
            state.paths(),
            SftpRenameRequest {
                host_id: host_id.clone(),
                from_path: "/uploaded.txt".to_owned(),
                to_path: "/renamed.txt".to_owned(),
            },
        )
        .await
        .expect("rename real SFTP file");
    state
        .sftp()
        .chmod(
            state.paths(),
            SftpChmodRequest {
                host_id: host_id.clone(),
                path: "/renamed.txt".to_owned(),
                mode: "600".to_owned(),
            },
        )
        .await
        .expect("chmod real SFTP file");

    let download_target = client_root.path().join("downloaded.txt");
    fs::write(&download_target, b"old local content")
        .await
        .expect("seed existing local download target");
    state
        .sftp()
        .download(
            state.paths(),
            SftpTransferRequest {
                host_id: host_id.clone(),
                remote_path: "/hello.txt".to_owned(),
                local_path: download_target.to_string_lossy().into_owned(),
                conflict_policy: SftpTransferConflictPolicy::Overwrite,
            },
        )
        .await
        .expect("download over real SFTP");
    assert_eq!(
        fs::read_to_string(download_target)
            .await
            .expect("read downloaded file"),
        "hello from native loopback"
    );
    assert!(
        !client_root
            .path()
            .join("downloaded.txt.kerminal-part")
            .exists(),
        "successful download should commit and remove the local partial file"
    );

    state
        .sftp()
        .delete(
            state.paths(),
            SftpDeleteRequest {
                host_id: host_id.clone(),
                path: "/renamed.txt".to_owned(),
                directory: false,
            },
        )
        .await
        .expect("delete real SFTP file");
    state
        .sftp()
        .delete(
            state.paths(),
            SftpDeleteRequest {
                host_id,
                path: "/managed".to_owned(),
                directory: true,
            },
        )
        .await
        .expect("delete non-empty remote directory with rm -rf");
    assert!(!server_root.path().join("renamed.txt").exists());
    assert!(!server_root.path().join("managed").exists());
}
