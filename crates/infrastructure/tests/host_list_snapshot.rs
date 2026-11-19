//! 主机列表批量读取的事务门禁与分类回归。
//!
//! @author kongweiguang

use std::fs;

use kerminal_application::connections::management::HostManagementRepository;
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
};
use tempfile::tempdir;

/// 合成 schema v3 SSH 主机；只用于确认批量读取仍按原类型解码。
fn ssh_host_source(id: &str) -> String {
    format!(
        "schema_version = 3\n\
         protocol = \"ssh\"\n\
         id = \"{id}\"\n\
         name = \"Remote {id}\"\n\
         host = \"198.51.100.1\"\n\
         port = 22\n\
         username = \"bench\"\n\
         auth_type = \"agent\"\n\
         sort_order = 1\n\
         created_at = \"1\"\n\
         updated_at = \"1\"\n"
    )
}

/// 合成 schema v3 本地主机；环境引用必须留给 vault，这里不写入任何环境字段。
fn local_host_source(id: &str) -> String {
    format!(
        "schema_version = 3\n\
         protocol = \"local\"\n\
         id = \"{id}\"\n\
         name = \"Local {id}\"\n\
         sort_order = 2\n\
         created_at = \"1\"\n\
         updated_at = \"1\"\n\
         [local]\n\
         shell = \"pwsh\"\n\
         args = []\n"
    )
}

/// 空主机目录也必须命中原有事务门禁，不能把未完成分组事务伪装成空列表。
///
/// 批量读取会先进入与单文件读取相同的共享锁和事务检查，这里固定该边界。
#[tokio::test]
async fn empty_host_directory_rejects_unfinished_group_transaction() {
    let root = tempdir().expect("创建隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化隔离配置仓储");
    let repository = HostConfigurationRepository::new(store);

    assert!(
        repository
            .list_hosts()
            .await
            .expect("空目录正常读取")
            .is_empty(),
        "空目录应返回空远程列表"
    );
    assert!(
        repository
            .list_local_hosts()
            .await
            .expect("空目录正常读取本地列表")
            .is_empty(),
        "空目录应返回空本地列表"
    );

    let transactions = root.path().join(".storage-transactions");
    fs::create_dir_all(&transactions).expect("创建事务目录");
    let journal = transactions.join("host-group.json");
    fs::write(&journal, b"{}").expect("保留未完成事务证据");

    assert!(
        repository.list_hosts().await.is_err(),
        "未完成分组事务下远程列表必须失败"
    );
    assert!(
        repository.list_local_hosts().await.is_err(),
        "未完成分组事务下本地列表必须失败"
    );
    assert_eq!(
        fs::read(&journal).expect("事务文件未被覆盖"),
        b"{}",
        "读取失败不能删除或改写恢复材料"
    );
}

/// 同一批读取仍要区分远程与本地；任一损坏文件都必须让两种投影整体失败。
///
/// @author kongweiguang
#[tokio::test]
async fn batch_read_preserves_host_classification_and_corrupt_error() {
    let root = tempdir().expect("创建隔离配置根");
    let hosts = root.path().join("hosts");
    fs::create_dir_all(&hosts).expect("创建主机目录");
    for id in ["remote-a", "remote-b"] {
        fs::write(hosts.join(format!("{id}.toml")), ssh_host_source(id)).expect("写入远程主机");
    }
    for id in ["local-a", "local-b"] {
        fs::write(hosts.join(format!("{id}.toml")), local_host_source(id)).expect("写入本地主机");
    }
    let broken = hosts.join("broken.toml");
    fs::write(&broken, "schema_version = 3\nprotocol = \"ssh\"\n").expect("写入损坏主机");

    let store = ConfigurationStore::initialize(root.path()).expect("初始化隔离配置仓储");
    let repository = HostConfigurationRepository::new(store);
    assert!(
        repository.list_hosts().await.is_err(),
        "损坏远程主机必须让远程列表整体失败"
    );
    assert!(
        repository.list_local_hosts().await.is_err(),
        "损坏条目不能在本地列表被静默跳过"
    );

    fs::remove_file(&broken).expect("移除损坏样本");
    let remote = repository.list_hosts().await.expect("读取远程主机");
    assert_eq!(
        remote
            .iter()
            .map(|host| host.id.as_str())
            .collect::<Vec<_>>(),
        ["remote-a", "remote-b"]
    );
    let local = repository.list_local_hosts().await.expect("读取本地主机");
    assert_eq!(
        local
            .iter()
            .map(|host| host.id.as_str())
            .collect::<Vec<_>>(),
        ["local-a", "local-b"]
    );

    let catalog = repository.list_catalog().await.expect("一次读取主机目录");
    assert_eq!(catalog.hosts, remote, "目录快照不能改变远程投影");
    assert_eq!(catalog.local_hosts, local, "目录快照不能改变本地投影");
    assert!(catalog.plugin_hosts.is_empty(), "内置主机不能混入扩展投影");
}
