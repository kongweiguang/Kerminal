//! 连接应用服务的凭据门禁与平台委托契约。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CredentialResolver, HostConnectionRepository, RdpLaunchReceipt, RdpLaunchRequest,
        RdpLaunchService, RdpLaunchTarget, RdpPlatformLauncher, ResolvedCredential,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshRoute,
    },
    identity::HostId,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

struct StaticHostRepository(HostConnectionProfile);

#[async_trait]
impl HostConnectionRepository for StaticHostRepository {
    /// 仅允许用例读取指定 HostId 的测试 profile，避免默认目标回退。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        if &self.0.host_id == host_id {
            Ok(self.0.clone())
        } else {
            Err(AppError::not_found("测试主机不存在"))
        }
    }
}

struct StaticCredentials {
    resolve_count: AtomicUsize,
    value: ResolvedCredential,
}

#[async_trait]
impl CredentialResolver for StaticCredentials {
    /// 测试解析仅返回短生命周期、零化包装的已配置凭据。
    async fn resolve(
        &self,
        _reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        self.resolve_count.fetch_add(1, Ordering::Relaxed);
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试凭据解析已取消"));
        }
        Ok(match &self.value {
            ResolvedCredential::Password(value) => {
                ResolvedCredential::Password(Zeroizing::new(value.to_string()))
            }
            ResolvedCredential::PrivateKey(value) => {
                ResolvedCredential::PrivateKey(Zeroizing::new(value.to_vec()))
            }
            ResolvedCredential::KeyPassphrase(value) => {
                ResolvedCredential::KeyPassphrase(Zeroizing::new(value.to_string()))
            }
        })
    }
}

struct CapturingRdpLauncher {
    received_password: Mutex<Option<Zeroizing<String>>>,
    launch_count: AtomicUsize,
}

#[async_trait]
impl RdpPlatformLauncher for CapturingRdpLauncher {
    /// 捕获 launcher 所有权输入以验证密码由应用端口安全转交。
    async fn launch(
        &self,
        target: RdpLaunchTarget,
        _cancel: CancellationToken,
    ) -> AppResult<RdpLaunchReceipt> {
        self.launch_count.fetch_add(1, Ordering::Relaxed);
        *self.received_password.lock().await = target.password;
        Ok(RdpLaunchReceipt {
            launch_id: kerminal_application::connections::RdpLaunchId::new(),
            endpoint: target.endpoint,
            process_id: Some(42),
            lifecycle: kerminal_application::connections::RdpClientLifecycle::ManagedProcess,
        })
    }
}

/// 创建含独立 vault 引用的 profile，覆盖 RDP 不持有内联密码的边界。
fn rdp_profile(protocol: RemoteProtocol) -> HostConnectionProfile {
    HostConnectionProfile {
        host_id: HostId::new(),
        protocol,
        endpoint: HostEndpoint {
            host: "rdp.example.test".to_owned(),
            port: 3389,
            username: String::new(),
        },
        authentication: None,
        rdp_password: Some(CredentialRef::new("credential:test:rdp").expect("valid reference")),
        route: SshRoute::default(),
        shell_dialect: RemoteShellDialect::Unknown,
        connect_timeout_ms: 2_000,
        keepalive_interval_ms: None,
    }
}

/// RDP 启动会从 vault 解析已保存密码，并只把短期秘密交给平台启动器。
#[tokio::test]
async fn rdp_launch_resolves_saved_password_for_platform_adapter() {
    let profile = rdp_profile(RemoteProtocol::Rdp);
    let host_id = profile.host_id.clone();
    let credentials = Arc::new(StaticCredentials {
        resolve_count: AtomicUsize::new(0),
        value: ResolvedCredential::Password(Zeroizing::new("test-secret".to_owned())),
    });
    let launcher = Arc::new(CapturingRdpLauncher {
        received_password: Mutex::new(None),
        launch_count: AtomicUsize::new(0),
    });
    let service = RdpLaunchService::new(
        Arc::new(StaticHostRepository(profile)),
        credentials.clone(),
        launcher.clone(),
    );

    let receipt = service
        .launch(RdpLaunchRequest { host_id }, CancellationToken::new())
        .await
        .expect("platform launch succeeds");

    assert_eq!(receipt.endpoint.port, 3389);
    assert!(receipt.endpoint.username.is_empty());
    assert_eq!(receipt.process_id, Some(42));
    assert_eq!(credentials.resolve_count.load(Ordering::Relaxed), 1);
    assert_eq!(launcher.launch_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        launcher
            .received_password
            .lock()
            .await
            .as_deref()
            .map(String::as_str),
        Some("test-secret")
    );
}

/// 非 RDP profile 必须在解密凭据和触碰平台客户端之前失败。
#[tokio::test]
async fn rdp_launch_rejects_other_protocol_before_secret_lookup() {
    let profile = rdp_profile(RemoteProtocol::Ssh);
    let host_id = profile.host_id.clone();
    let credentials = Arc::new(StaticCredentials {
        resolve_count: AtomicUsize::new(0),
        value: ResolvedCredential::Password(Zeroizing::new("test-secret".to_owned())),
    });
    let launcher = Arc::new(CapturingRdpLauncher {
        received_password: Mutex::new(None),
        launch_count: AtomicUsize::new(0),
    });
    let service = RdpLaunchService::new(
        Arc::new(StaticHostRepository(profile)),
        credentials.clone(),
        launcher.clone(),
    );

    let result = service
        .launch(RdpLaunchRequest { host_id }, CancellationToken::new())
        .await;

    assert!(matches!(
        result,
        Err(AppError::CapabilityNotSupported { .. })
    ));
    assert_eq!(credentials.resolve_count.load(Ordering::Relaxed), 0);
    assert_eq!(launcher.launch_count.load(Ordering::Relaxed), 0);
}
