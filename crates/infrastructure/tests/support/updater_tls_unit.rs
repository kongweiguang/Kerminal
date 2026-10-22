//! updater 本地 HTTPS 测试的私有信任根与 TLS listener。
//!
//! @author kongweiguang

use std::{io::Cursor, net::SocketAddr, sync::Arc};

use cargo_packager_updater::reqwest::{Certificate, Url};
use tokio::net::TcpListener;
use tokio_rustls::{TlsAcceptor, rustls};

const ROOT_CA_PEM: &[u8] = include_bytes!("../../../../tests/fixtures/updater/tls/root-ca.pem");
const SERVER_CERTIFICATE_PEM: &[u8] =
    include_bytes!("../../../../tests/fixtures/updater/tls/server.pem");
const SERVER_PRIVATE_KEY_PEM: &[u8] =
    include_bytes!("../../../../tests/fixtures/updater/tls/server-key.pem");

/// 只将合成的测试 CA 注入当前 reqwest client，不改进程或系统信任库。
pub(crate) fn trusted_test_root() -> Certificate {
    Certificate::from_pem(ROOT_CA_PEM).expect("parse isolated updater test root certificate")
}

/// 使用明确的 ring provider 构造服务器，避免工作区其他 crate 的 feature 合并改变 provider 选择。
pub(crate) fn test_tls_acceptor() -> TlsAcceptor {
    let mut certificate_reader = Cursor::new(SERVER_CERTIFICATE_PEM);
    let certificates = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse isolated updater server certificate");
    let mut key_reader = Cursor::new(SERVER_PRIVATE_KEY_PEM);
    let private_key = rustls_pemfile::private_key(&mut key_reader)
        .expect("parse isolated updater server private key")
        .expect("test TLS server key exists");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("select supported test TLS protocol versions")
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .expect("pair synthetic TLS certificate and key");
    TlsAcceptor::from(Arc::new(config))
}

/// 绑定 IPv4 loopback，确保测试 listener 不暴露到外部网卡。
pub(crate) async fn bind_loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated updater TLS listener");
    let address = listener
        .local_addr()
        .expect("read isolated updater TLS address");
    (listener, address)
}

/// 将动态 loopback socket 转成仍满足产品 HTTPS URL 策略的测试地址。
pub(crate) fn https_url(address: SocketAddr, path: &str) -> Url {
    Url::parse(&format!("https://{address}{path}")).expect("format isolated updater HTTPS URL")
}
