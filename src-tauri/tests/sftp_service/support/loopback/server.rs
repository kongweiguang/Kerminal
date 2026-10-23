// @author kongweiguang

use super::*;

pub(crate) async fn start_loopback_sftp_server(root: PathBuf) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_shell_policy(root, Vec::new(), true).await
}

/// Expose a real loopback server that deterministically rejects renames over an existing path.
/// This is the production-compatibility fixture for servers without POSIX rename support.
pub(crate) async fn start_loopback_sftp_server_without_overwrite(
    root: PathBuf,
) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_shell_policy_and_rename_policy(
        root,
        Vec::new(),
        true,
        LoopbackSftpRenamePolicy::RejectExisting,
    )
    .await
}

pub(crate) async fn start_loopback_sftp_only_server(root: PathBuf) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_shell_policy(root, Vec::new(), false).await
}

pub(crate) async fn start_loopback_sftp_server_on_port_with_private_key(
    root: PathBuf,
    port: u16,
    private_key: PrivateKey,
) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_symlinks_on_port_and_private_key(
        root,
        Vec::new(),
        port,
        private_key,
    )
    .await
}

pub(crate) async fn start_loopback_sftp_server_with_symlinks(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_shell_policy(root, symlinks, true).await
}

/// Keep existing callers on the historical rename behavior unless a test opts into a policy.
async fn start_loopback_sftp_server_with_shell_policy(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
    allow_shell: bool,
) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_shell_policy_and_rename_policy(
        root,
        symlinks,
        allow_shell,
        LoopbackSftpRenamePolicy::ReplaceExisting,
    )
    .await
}

/// Keep the common shell and rename policy in one constructor so test fixtures cannot diverge.
async fn start_loopback_sftp_server_with_shell_policy_and_rename_policy(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
    allow_shell: bool,
    rename_policy: LoopbackSftpRenamePolicy,
) -> LoopbackSftpServer {
    start_loopback_sftp_server_with_symlinks_on_port_and_policy(
        root,
        symlinks,
        0,
        allow_shell,
        rename_policy,
    )
    .await
}

/// Bind and launch one isolated server with the requested rename semantics.
async fn start_loopback_sftp_server_with_symlinks_on_port_and_policy(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
    port: u16,
    allow_shell: bool,
    rename_policy: LoopbackSftpRenamePolicy,
) -> LoopbackSftpServer {
    let listener = bind_loopback_sftp_listener(port).await;
    let private_key = PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)
        .expect("generate loopback host key");
    start_loopback_sftp_server_with_listener_symlinks_and_private_key(
        root,
        symlinks,
        listener,
        private_key,
        allow_shell,
        rename_policy,
    )
    .await
}

/// Restart fixtures with their prior host key while keeping the default rename behavior stable.
async fn start_loopback_sftp_server_with_symlinks_on_port_and_private_key(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
    port: u16,
    private_key: PrivateKey,
) -> LoopbackSftpServer {
    let listener = bind_loopback_sftp_listener(port).await;
    start_loopback_sftp_server_with_listener_symlinks_and_private_key(
        root,
        symlinks,
        listener,
        private_key,
        true,
        LoopbackSftpRenamePolicy::ReplaceExisting,
    )
    .await
}

/// Share launch setup so policy-specific fixtures retain identical SSH, auth, and path isolation.
async fn start_loopback_sftp_server_with_listener_symlinks_and_private_key(
    root: PathBuf,
    symlinks: Vec<(String, String)>,
    listener: TcpListener,
    private_key: PrivateKey,
    allow_shell: bool,
    rename_policy: LoopbackSftpRenamePolicy,
) -> LoopbackSftpServer {
    let addr = listener.local_addr().expect("loopback SFTP address");
    let config = russh::server::Config {
        auth_rejection_time: Duration::from_millis(0),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        keys: vec![private_key.clone()],
        maximum_packet_size: 65_535,
        ..Default::default()
    };
    let symlinks = Arc::new(
        symlinks
            .into_iter()
            .map(|(link, target)| {
                (
                    normalize_loopback_remote_path(&link),
                    normalize_loopback_remote_path(&target),
                )
            })
            .collect(),
    );
    let auth_successes = Arc::new(AtomicUsize::new(0));
    let server_auth_successes = Arc::clone(&auth_successes);
    let task = tokio::spawn(async move {
        let mut server = LoopbackSshServer {
            allow_shell,
            rename_policy,
            auth_successes: server_auth_successes,
            root,
            symlinks,
        };
        let running = server.run_on_socket(Arc::new(config), &listener);
        let _ = running.await;
    });

    LoopbackSftpServer {
        addr,
        auth_successes,
        private_key,
        task,
    }
}

async fn bind_loopback_sftp_listener(port: u16) -> TcpListener {
    let mut last_error = None;
    for _ in 0..20 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return listener,
            Err(error) if port != 0 => {
                last_error = Some(error);
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("bind loopback SFTP server: {error}"),
        }
    }
    panic!(
        "bind loopback SFTP server on port {port}: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown bind error".to_owned())
    );
}

pub(crate) async fn start_loopback_sftp_jump_server(
    target_addr: SocketAddr,
) -> LoopbackSftpJumpServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback SFTP jump server");
    let addr = listener.local_addr().expect("loopback SFTP jump address");
    let private_key = PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)
        .expect("generate loopback SFTP jump host key");
    let config = russh::server::Config {
        auth_rejection_time: Duration::from_millis(0),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        keys: vec![private_key],
        maximum_packet_size: 65_535,
        ..Default::default()
    };
    let direct_tcpip_requests = Arc::new(AtomicUsize::new(0));
    let counters = Arc::clone(&direct_tcpip_requests);
    let task = tokio::spawn(async move {
        let mut server = LoopbackSftpJumpServerState {
            direct_tcpip_requests: counters,
            target_addr,
        };
        let running = server.run_on_socket(Arc::new(config), &listener);
        let _ = running.await;
    });

    LoopbackSftpJumpServer {
        addr,
        direct_tcpip_requests,
        task,
    }
}
