//! 文件目标名称投影应提高可读性，同时始终保留稳定目标身份。
//!
//! @author kongweiguang

use std::collections::HashSet;

use kerminal_domain::{
    files::{ContainerRuntime, FileEndpoint, RemotePath},
    identity::HostId,
    operations::ContainerTarget,
};
use tokio_util::sync::CancellationToken;

use super::{FileTargetNameKey, FileTargetNameLookup, FileTargetNameState, PendingFileTargetName};

/// 使用真实领域解析器构造远端路径，避免测试复制 endpoint 的路径规则。
///
/// @author kongweiguang
fn remote(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::remote(host_id.clone(), path).expect("remote path should be valid")
}

/// 容器端点的路径不参与目标名称缓存键，但宿主、运行时与容器 ID 都参与。
///
/// @author kongweiguang
fn container(target: &ContainerTarget, path: &str) -> FileEndpoint {
    FileEndpoint::Container {
        host_id: target.host_id.clone(),
        runtime: target.runtime,
        container_id: target.container_id.clone(),
        path: RemotePath::parse_container(path).expect("container path should be valid"),
    }
}

/// 左右目录或多个路径指向同一容器时复用一个精确名称快照。
///
/// @author kongweiguang
#[test]
fn target_keys_deduplicate_paths_but_keep_complete_container_identity() {
    let host_id = HostId::new();
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-one".to_owned(),
    };
    let endpoints = [
        remote(&host_id, "/srv/one"),
        remote(&host_id, "/srv/two"),
        container(&target, "/app"),
        container(&target, "/var/log"),
    ];

    let keys = endpoints
        .iter()
        .flat_map(super::file_target_name_keys)
        .collect::<HashSet<_>>();

    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&FileTargetNameKey::Host(host_id.clone())));
    assert!(keys.contains(&FileTargetNameKey::Container(target.clone())));
    assert!(
        !keys.contains(&FileTargetNameKey::Container(ContainerTarget {
            host_id: Some(host_id),
            runtime: ContainerRuntime::Podman,
            container_id: target.container_id,
        }))
    );
}

/// 名称投影用于紧凑主标签，详情仍带完整 HostId 和远端路径以供核对目标。
///
/// @author kongweiguang
#[test]
fn resolved_host_name_keeps_full_identity_in_details() {
    let host_id = HostId::parse("production-host-identifier").expect("host ID is valid");
    let endpoint = remote(&host_id, "/srv/app/config.toml");
    let key = FileTargetNameKey::Host(host_id.clone());
    let mut state = FileTargetNameState::default();
    state.active.insert(key.clone());
    state
        .resolved
        .insert(key, Some("Production SSH".to_owned()));

    let display = state.display(&endpoint);
    let detail = state.endpoint_detail(&endpoint);

    assert_eq!(display.compact, "SSH · Production SSH");
    assert!(detail.contains("Production SSH"));
    assert!(detail.contains(host_id.as_str()));
    assert!(detail.contains("/srv/app/config.toml"));
}

/// 容器与宿主名称可并列显示，详情继续保留 runtime、双方完整 ID 和容器内路径。
///
/// @author kongweiguang
#[test]
fn resolved_container_display_retains_exact_target_and_path() {
    let host_id = HostId::parse("docker-host-identifier").expect("host ID is valid");
    let target = ContainerTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-id-123456789".to_owned(),
    };
    let endpoint = container(&target, "/app/settings.toml");
    let mut state = FileTargetNameState::default();
    state
        .active
        .insert(FileTargetNameKey::Host(host_id.clone()));
    state
        .active
        .insert(FileTargetNameKey::Container(target.clone()));
    state.resolved.insert(
        FileTargetNameKey::Host(host_id.clone()),
        Some("Build host".to_owned()),
    );
    state.resolved.insert(
        FileTargetNameKey::Container(target.clone()),
        Some("api-service".to_owned()),
    );

    let display = state.display(&endpoint);
    let detail = state.endpoint_detail(&endpoint);

    assert!(display.compact.contains("Build host"));
    assert!(display.compact.contains("Docker"));
    assert!(display.compact.contains("api-service"));
    assert!(detail.contains(host_id.as_str()));
    assert!(detail.contains(&target.container_id));
    assert!(detail.contains("/app/settings.toml"));
}

/// 解析失败时仍以短标签和完整详情区分目标，名称服务故障不阻断文件浏览。
///
/// @author kongweiguang
#[test]
fn missing_name_falls_back_to_short_label_and_full_id() {
    let host_id = HostId::parse("production-host-identifier").expect("host ID is valid");
    let endpoint = remote(&host_id, "/srv/app");
    let state = FileTargetNameState::default();

    let display = state.display(&endpoint);
    let detail = state.endpoint_detail(&endpoint);

    assert!(display.compact.contains(&host_id.as_str()[..12]));
    assert!(detail.contains(host_id.as_str()));
}

/// 移出会话后同一主机重新进入时，旧代次回执不能覆盖新的名称查询。
///
/// @author kongweiguang
#[test]
fn reopened_same_target_rejects_old_generation_and_accepts_current_result() {
    let host_id = HostId::new();
    let key = FileTargetNameKey::Host(host_id.clone());
    let cancellation = CancellationToken::new();
    let mut state = FileTargetNameState::default();
    state.active.insert(key.clone());
    state.pending.insert(
        key.clone(),
        PendingFileTargetName {
            generation: 4,
            cancellation: cancellation.clone(),
        },
    );

    state.synchronize([], &FileTargetNameLookup::default());

    assert!(cancellation.is_cancelled());
    assert!(!state.complete(&key, 4, Ok(Some("late name".to_owned()))));

    // 模拟相同 HostId 再次进入 FilesPane 后重新发出的新一代请求。
    state.active.insert(key.clone());
    state.pending.insert(
        key.clone(),
        PendingFileTargetName {
            generation: 5,
            cancellation: CancellationToken::new(),
        },
    );
    assert!(!state.complete(&key, 4, Ok(Some("old host name".to_owned()))));
    assert!(state.complete(&key, 5, Ok(Some("current host name".to_owned()))));
    assert!(
        !state
            .display(&remote(&host_id, "/srv/app"))
            .detail
            .contains("old host name")
    );
    assert!(
        state
            .display(&remote(&host_id, "/srv/app"))
            .detail
            .contains("current host name")
    );
}
