//! 已保存主机对外部目标的精确分类契约测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    connections::management::HostManagementRepository,
    platform::external_launch::{ExternalTargetIdentity, SavedExternalTargetLookup},
    preferences::ConfigurationRepository,
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
    preferences::ConfigDocumentPath,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
};

struct IsolatedConfigRoot(PathBuf);

impl IsolatedConfigRoot {
    /// 每个集成测试使用独立的临时目录，避免读取或改写用户主目录配置。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-external-target-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }
}

impl Drop for IsolatedConfigRoot {
    /// 测试结束后只清理本测试创建的唯一临时目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 以 current schema-v3 文件写入真实配置仓储，验证 lookup 与主机目录使用同一解析路径。
async fn repository_with_host() -> (IsolatedConfigRoot, HostConfigurationRepository, HostId) {
    let root = IsolatedConfigRoot::new();
    let store = ConfigurationStore::initialize(&root.0).expect("初始化隔离配置目录");
    let host_id = HostId::new();
    let source = format!(
        r#"schema_version = 3
protocol = "ssh"
id = "{}"
name = "jump route"
host = "EXAMPLE.COM."
port = 2222
username = "alice"
auth_type = "agent"
sort_order = 0
created_at = "1"
updated_at = "1"

[[ssh_options.jump_hosts]]
name = "first"
host = "JUMP-A.EXAMPLE."
port = 2201
username = "relay-a"
auth_type = "agent"

[[ssh_options.jump_hosts]]
name = "second"
host = "[2001:DB8::2]"
port = 22
username = "relay-b"
auth_type = "agent"
"#,
        host_id.as_str()
    );
    store
        .write(
            ConfigDocumentPath::Host(host_id.as_str().to_owned()),
            source,
            None,
        )
        .await
        .expect("写入隔离的 schema-v3 主机");
    (root, HostConfigurationRepository::new(store), host_id)
}

/// 没有 ssh_options 的最小 schema-v3 主机应等价于显式 no-proxy，而不因空默认协议整表失败。
#[tokio::test]
async fn host_without_ssh_options_defaults_to_no_proxy() {
    let root = IsolatedConfigRoot::new();
    let store = ConfigurationStore::initialize(&root.0).expect("初始化隔离配置目录");
    let host_id = HostId::new();
    let source = format!(
        r#"schema_version = 3
protocol = "ssh"
id = "{}"
name = "minimal host"
host = "127.0.0.1"
port = 22
username = "tester"
auth_type = "agent"
sort_order = 0
created_at = "1"
updated_at = "1"
"#,
        host_id.as_str()
    );
    store
        .write(
            ConfigDocumentPath::Host(host_id.as_str().to_owned()),
            source,
            None,
        )
        .await
        .expect("写入最小有效 schema-v3 主机");

    let repository = HostConfigurationRepository::new(store);
    let hosts = repository.list_hosts().await.expect("读取最小主机列表");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id, host_id);
    assert!(hosts[0].route.proxy.is_none());
}

/// 主机名大小写、DNS 尾点和 IPv6 方括号归一化；用户名、端口和跳板顺序仍精确匹配。
#[tokio::test]
async fn matches_normalized_endpoint_and_complete_ordered_jump_chain() {
    let (_root, repository, expected_id) = repository_with_host().await;
    let target = ExternalTargetIdentity {
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "example.com".to_owned(),
            port: 2222,
            username: " alice ".to_owned(),
        },
        jump_hosts: vec![
            HostEndpoint {
                host: "jump-a.example".to_owned(),
                port: 2201,
                username: "relay-a".to_owned(),
            },
            HostEndpoint {
                host: "2001:db8::2".to_owned(),
                port: 22,
                username: "relay-b".to_owned(),
            },
        ],
    };

    assert_eq!(
        repository.find_exact(&target).await.unwrap(),
        Some(expected_id.clone())
    );

    let mut wrong_username = target.clone();
    wrong_username.endpoint.username = "Alice".to_owned();
    assert_eq!(repository.find_exact(&wrong_username).await.unwrap(), None);

    let mut wrong_order = target.clone();
    wrong_order.jump_hosts.reverse();
    assert_eq!(repository.find_exact(&wrong_order).await.unwrap(), None);

    let mut wrong_port = target.clone();
    wrong_port.endpoint.port += 1;
    assert_eq!(repository.find_exact(&wrong_port).await.unwrap(), None);

    let mut wrong_protocol = target;
    wrong_protocol.protocol = RemoteProtocol::Sftp;
    assert_eq!(repository.find_exact(&wrong_protocol).await.unwrap(), None);
}
