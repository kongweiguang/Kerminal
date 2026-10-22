//! proxy 私有单元测试。
//
// @author kongweiguang

use super::*;

use async_trait::async_trait;
use kerminal_application::connections::ResolvedCredential;
use kerminal_domain::connections::{CredentialRef, HostEndpoint, ProxyProtocol};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct TestCredentials;

#[async_trait]
impl CredentialResolver for TestCredentials {
    /// 返回一次性 SOCKS5 测试密码，确保代理 adapter 通过 vault port 取密。
    async fn resolve(
        &self,
        _reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("SOCKS5 测试凭据解析已取消"));
        }
        Ok(ResolvedCredential::Password(Zeroizing::new(
            "proxy-secret".to_owned(),
        )))
    }
}

/// 真实执行 SOCKS5 用户名密码握手，再验证 tunnel payload 双向到达 loopback 服务。
#[tokio::test]
async fn socks5_password_authentication_tunnels_bytes_to_target() {
    let target_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("target listener starts");
    let target_address = target_listener
        .local_addr()
        .expect("target address resolves");
    let target_task = tokio::spawn(async move {
        let (mut stream, _) = target_listener.accept().await.expect("target accepts");
        let mut request = [0_u8; 4];
        stream
            .read_exact(&mut request)
            .await
            .expect("target reads tunnel bytes");
        stream
            .write_all(b"pong")
            .await
            .expect("target responds over tunnel");
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy listener starts");
    let proxy_address = proxy_listener.local_addr().expect("proxy address resolves");
    let proxy_task = tokio::spawn(async move {
        let (mut client, _) = proxy_listener.accept().await.expect("proxy accepts");
        let mut greeting = [0_u8; 2];
        client
            .read_exact(&mut greeting)
            .await
            .expect("proxy reads greeting");
        let mut methods = vec![0; usize::from(greeting[1])];
        client
            .read_exact(&mut methods)
            .await
            .expect("proxy reads auth methods");
        let has_password_method = greeting[0] == 5 && methods.contains(&2);
        if !has_password_method {
            return false;
        }
        client
            .write_all(&[5, 2])
            .await
            .expect("proxy selects username/password auth");

        let mut auth_header = [0_u8; 2];
        client
            .read_exact(&mut auth_header)
            .await
            .expect("proxy reads auth header");
        let mut username = vec![0; usize::from(auth_header[1])];
        client
            .read_exact(&mut username)
            .await
            .expect("proxy reads username");
        let mut password_length = [0_u8; 1];
        client
            .read_exact(&mut password_length)
            .await
            .expect("proxy reads password length");
        let mut password = vec![0; usize::from(password_length[0])];
        client
            .read_exact(&mut password)
            .await
            .expect("proxy reads password");
        let credentials_match = auth_header[0] == 1
            && username.as_slice() == b"proxy-user"
            && password.as_slice() == b"proxy-secret";
        client
            .write_all(&[1, if credentials_match { 0 } else { 1 }])
            .await
            .expect("proxy reports auth result");
        if !credentials_match {
            return false;
        }

        let mut connect_header = [0_u8; 4];
        client
            .read_exact(&mut connect_header)
            .await
            .expect("proxy reads connect header");
        let mut destination = [0_u8; 6];
        client
            .read_exact(&mut destination)
            .await
            .expect("proxy reads IPv4 target");
        let request_is_ipv4_connect = connect_header == [5, 1, 0, 1];
        let destination_matches = request_is_ipv4_connect
            && destination[..4] == [127, 0, 0, 1]
            && u16::from_be_bytes([destination[4], destination[5]]) == target_address.port();
        if !destination_matches {
            return false;
        }
        let mut target = TcpStream::connect(target_address)
            .await
            .expect("proxy connects to loopback target");
        client
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .expect("proxy accepts connect request");
        tokio::io::copy_bidirectional(&mut client, &mut target)
            .await
            .expect("proxy forwards tunnel data");
        true
    });

    let proxy = ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: proxy_address.ip().to_string(),
            port: proxy_address.port(),
            username: "proxy-user".to_owned(),
        },
        password: Some(CredentialRef::new("test:proxy-password").expect("valid ref")),
    };
    let target = HostEndpoint {
        host: target_address.ip().to_string(),
        port: target_address.port(),
        username: "target-user".to_owned(),
    };
    let mut tunnel =
        connect_via_proxy(&proxy, &target, &TestCredentials, &CancellationToken::new())
            .await
            .expect("SOCKS5 adapter returns a connected stream");
    tunnel.write_all(b"ping").await.expect("tunnel sends bytes");
    let mut response = [0_u8; 4];
    tunnel
        .read_exact(&mut response)
        .await
        .expect("tunnel reads response");
    assert_eq!(&response, b"pong");
    drop(tunnel);
    assert!(proxy_task.await.expect("proxy task joins"));
    target_task.await.expect("target task joins");
}
