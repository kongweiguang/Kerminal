//! RSA 签名算法协商的回归测试：验证系统 agent 身份按当前 peer 公布的
//! `server-sig-algs` 选择 SHA-2，并在服务端只承认旧算法时保留原有签名行为。
//!
//! 直接观察 agent 线协议才能区分“认证成功”与“确实用了 SHA-2”；本文件因此用最小
//! loopback server 建立真实 russh handle，再让假 agent 记录客户端请求的哈希标志。
//! 共用集成测试的 support 模块会引入产品 crate，无法编进 lib 测试，所以只复用 key 夹具。
//!
//! @author kongweiguang

use std::{
    borrow::Cow,
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    connections::{HostKeyIdentity, HostKeyStatus, KnownHostsRepository},
};
use kerminal_domain::connections::HostEndpoint;
use russh::{
    client,
    keys::{
        Algorithm, EcdsaCurve, HashAlg, PrivateKey, agent::client::AgentClient,
        signature::Signer as _, ssh_key::private::KeypairData,
    },
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{
    ForwardRouter, RusshHandler, RusshPeerSession, authenticate_agent_identities,
    make_peer_session, ssh_test_keys,
};

const AGENT_REQUEST_IDENTITIES: u8 = 11;
const AGENT_IDENTITIES_ANSWER: u8 = 12;
const AGENT_SIGN_REQUEST: u8 = 13;
const AGENT_SIGN_RESPONSE: u8 = 14;
const TEST_TIMEOUT_MS: u64 = 5_000;
const TEST_BOUND: Duration = Duration::from_secs(2);

/// 默认广告列表包含 rsa-sha2-512，agent 必须收到 SHA-512 标志（4），否则现代 sshd
/// 会拒绝旧 ssh-rsa 签名，正是本次用户报告的现象。
#[tokio::test]
async fn agent_rsa_identity_uses_sha512_when_server_advertises_it() {
    let (result, flags) = run_agent_authentication(None).await;
    result.expect("RSA agent authentication completes");
    assert_eq!(
        flags,
        vec![4],
        "agent must be asked for an rsa-sha2-512 signature"
    );
}

/// 服务端只公布 rsa-sha2-256 时按 russh 的优先级降到 SHA-256（标志 2），
/// 不能回退到旧 ssh-rsa，否则仍会被只接受 SHA-2 的服务器拒绝。
#[tokio::test]
async fn agent_rsa_identity_uses_sha256_when_only_sha256_is_advertised() {
    let (result, flags) = run_agent_authentication(Some(vec![
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
        Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        },
    ]))
    .await;
    result.expect("RSA agent authentication completes with SHA-256");
    assert_eq!(
        flags,
        vec![2],
        "agent must be asked for an rsa-sha2-256 signature"
    );
}

/// 服务端公布扩展但只列出旧 ssh-rsa 时沿用 None（标志 0），保持升级前对老服务器的行为。
#[tokio::test]
async fn agent_rsa_identity_keeps_legacy_signature_without_sha2_support() {
    let (result, flags) = run_agent_authentication(Some(vec![
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
        Algorithm::Rsa { hash: None },
    ]))
    .await;
    result.expect("RSA agent authentication completes with legacy ssh-rsa");
    assert_eq!(
        flags,
        vec![0],
        "agent must keep the legacy ssh-rsa signature"
    );
}

/// 建立真实 peer 并让假 agent 记录签名标志；结束前显式 abort 并 join 超时任务，
/// 避免测试通过后仍留下等待帧的后台 future。
async fn run_agent_authentication(
    preferred_key_algorithms: Option<Vec<Algorithm>>,
) -> (AppResult<()>, Vec<u32>) {
    let server = LoopbackServer::spawn(preferred_key_algorithms).await;
    let session = connect_test_peer(&server).await;

    let recorded = Arc::new(StdMutex::new(Vec::new()));
    let (client_stream, agent_stream) = tokio::io::duplex(64 * 1024);
    let mut agent_task = tokio::spawn(serve_fake_agent(
        agent_stream,
        decode_test_rsa_key(),
        Arc::clone(&recorded),
    ));
    let mut agent = AgentClient::connect(client_stream);

    let result = authenticate_agent_identities(
        &session,
        &mut agent,
        &CancellationToken::new(),
        TEST_TIMEOUT_MS,
    )
    .await;
    let flags = recorded
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();

    let agent_finished = match timeout(TEST_BOUND, &mut agent_task).await {
        Ok(joined) => {
            joined.expect("fake agent task does not panic");
            true
        }
        Err(_) => {
            agent_task.abort();
            let _ = agent_task.await;
            false
        }
    };
    drop(session);
    server.shutdown().await;
    assert!(
        agent_finished,
        "fake agent must finish within the bounded test wait"
    );
    (result, flags)
}

/// 只完成握手并包装成生产同款 session，使协商逻辑面对真实 russh handle 与扩展信息。
async fn connect_test_peer(server: &LoopbackServer) -> Arc<RusshPeerSession> {
    let endpoint = HostEndpoint {
        host: "127.0.0.1".to_owned(),
        port: server.address.port(),
        username: "kerminal".to_owned(),
    };
    let stream = TcpStream::connect(server.address)
        .await
        .expect("loopback SSH socket connects");
    let handler = RusshHandler {
        endpoint: endpoint.clone(),
        known_hosts: Arc::new(TrustAllKnownHosts),
        decision_error: Arc::new(StdMutex::new(None)),
        forward_router: Arc::new(ForwardRouter::default()),
    };
    let config = client::Config {
        nodelay: true,
        ..Default::default()
    };
    let handle = client::connect_stream(Arc::new(config), stream, handler)
        .await
        .expect("loopback SSH handshake completes");
    make_peer_session(handle, &endpoint, CancellationToken::new())
}

/// 一次只服务一个连接的 loopback server；preferred.key 同时决定 host key 协商和
/// server-sig-algs 广告，从而精确模拟不同 OpenSSH 版本的 RSA 支持范围。
struct LoopbackServer {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl LoopbackServer {
    /// 用 RSA 夹具充当 host key；关闭时先取消 accept，超时则 abort 并 join，杜绝后台遗留。
    async fn spawn(preferred_key_algorithms: Option<Vec<Algorithm>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback SSH listener");
        let address = listener
            .local_addr()
            .expect("read loopback SSH listener address");
        let mut config = server::Config::default();
        config.keys.push(decode_test_rsa_key());
        config.inactivity_timeout = None;
        if let Some(algorithms) = preferred_key_algorithms {
            config.preferred.key = Cow::Owned(algorithms);
        }
        let config = Arc::new(config);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = task_shutdown.cancelled() => {}
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        let _ = server::run_stream(config, stream, AcceptAnyPublicKey).await;
                    }
                }
            }
        });
        Self {
            address,
            shutdown,
            task,
        }
    }

    /// 先请求取消以唤醒 accept；连接仍存活时用 abort + join 确保测试资源真正回收。
    async fn shutdown(mut self) {
        self.shutdown.cancel();
        if timeout(TEST_BOUND, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

/// 测试 fixture 不校验 authorized_keys，只确认公钥协议流按协商算法完成签名验证。
struct AcceptAnyPublicKey;

impl server::Handler for AcceptAnyPublicKey {
    type Error = russh::Error;

    /// 所有公钥都被接受；签名本身仍由 russh 按客户端声明的算法验证。
    ///
    /// @author kongweiguang
    async fn auth_publickey(
        &mut self,
        _user: &str,
        _public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }
}

/// known_hosts 在单元测试里只承担握手放行；返回 Known 避免任何磁盘或状态依赖。
struct TrustAllKnownHosts;

#[async_trait]
impl KnownHostsRepository for TrustAllKnownHosts {
    async fn inspect(
        &self,
        _endpoint: &HostEndpoint,
        _identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        Ok(HostKeyStatus::Known)
    }

    async fn record(&self, _endpoint: &HostEndpoint, _identity: &HostKeyIdentity) -> AppResult<()> {
        Ok(())
    }
}

/// 假 agent 按客户端请求的哈希标志生成真实签名；这样除记录标志外，认证也能走完整链路，
/// 避免测试只验证到请求发出却漏掉客户端对返回签名类型的校验。
async fn serve_fake_agent(
    mut stream: DuplexStream,
    key: PrivateKey,
    recorded: Arc<StdMutex<Vec<u32>>>,
) {
    let frame = read_frame(&mut stream).await;
    assert_eq!(
        frame.first(),
        Some(&AGENT_REQUEST_IDENTITIES),
        "agent must receive an identity request first"
    );
    let public_key = key
        .public_key()
        .to_bytes()
        .expect("RSA fixture public key encodes");
    let mut response = vec![AGENT_IDENTITIES_ANSWER, 0, 0, 0, 1];
    push_string(&mut response, &public_key);
    push_string(&mut response, b"kerminal-test-rsa");
    write_frame(&mut stream, &response).await;

    let frame = read_frame(&mut stream).await;
    assert_eq!(
        frame.first(),
        Some(&AGENT_SIGN_REQUEST),
        "agent must receive a sign request"
    );
    let mut reader = &frame[1..];
    let requested_key = take_string(&mut reader);
    assert_eq!(
        requested_key,
        public_key.as_slice(),
        "client must ask the agent to sign with the advertised identity"
    );
    let data = take_string(&mut reader).to_vec();
    let flags = u32::from_be_bytes(
        reader
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .expect("agent sign request carries the hash flag"),
    );
    recorded
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(flags);

    let (algorithm, signature) = sign_with_flags(&key, flags, &data);
    // agent 响应先包一层 string 承载完整 SSH 签名块，russh 的 write_signature 再从该块里
    // 解出算法名和签名；少这层包装会被当成协议错误并映射为 “SSH agent signing failed”。
    let mut signature_blob = Vec::new();
    push_string(&mut signature_blob, algorithm.as_bytes());
    push_string(&mut signature_blob, &signature);
    let mut response = vec![AGENT_SIGN_RESPONSE];
    push_string(&mut response, &signature_blob);
    write_frame(&mut stream, &response).await;
}

/// agent 协议用 0/2/4 表示 ssh-rsa、rsa-sha2-256、rsa-sha2-512；按标志签名才能让
/// russh 的返回类型校验通过，真实暴露“客户端选了哪种算法”。
fn sign_with_flags(key: &PrivateKey, flags: u32, data: &[u8]) -> (String, Vec<u8>) {
    let hash = match flags {
        4 => Some(HashAlg::Sha512),
        2 => Some(HashAlg::Sha256),
        _ => None,
    };
    let KeypairData::Rsa(keypair) = key.key_data() else {
        panic!("RSA negotiation fixture must contain an RSA private key");
    };
    let signature = (keypair, hash)
        .try_sign(data)
        .expect("test RSA key signs the agent payload");
    (
        signature.algorithm().as_str().to_owned(),
        signature.as_ref().to_vec(),
    )
}

/// 解码共享 key 夹具；每次调用都产生独立所有权，避免 server 与 fake agent 互相借用。
fn decode_test_rsa_key() -> PrivateKey {
    russh::keys::decode_secret_key(
        std::str::from_utf8(&ssh_test_keys::test_rsa_private_key()).expect("RSA fixture is UTF-8"),
        None,
    )
    .expect("RSA fixture decodes")
}

/// agent 帧以 u32 大端长度开头；手写编解码可避免依赖 russh 内部未公开的 msg 模块。
async fn read_frame(stream: &mut DuplexStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .await
        .expect("agent frame length is readable");
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream
        .read_exact(&mut payload)
        .await
        .expect("agent frame body is readable");
    payload
}

/// 写回完整帧并立即 flush，保证客户端不会因缓冲等待而超时。
async fn write_frame(stream: &mut DuplexStream, payload: &[u8]) {
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .expect("agent frame length is writable");
    stream
        .write_all(payload)
        .await
        .expect("agent frame body is writable");
    stream.flush().await.expect("agent frame flushes");
}

/// SSH string 使用 u32 大端长度前缀，agent 协议复用同一编码。
fn push_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// 读取一个 SSH string；签名请求字段固定顺序，越界即说明 fixture 编码有误。
fn take_string<'a>(reader: &mut &'a [u8]) -> &'a [u8] {
    let input = *reader;
    let length = input
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .expect("agent frame contains a string length") as usize;
    let body = input
        .get(4..4 + length)
        .expect("agent frame contains the declared string body");
    *reader = &input[4 + length..];
    body
}
