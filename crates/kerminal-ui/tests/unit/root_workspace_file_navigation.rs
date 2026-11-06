//! 文件导航覆盖完整目标身份和临时连接生命周期，不依赖真实网络与文件正文。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use kerminal_application::{
    AppResult, platform::external_launch::ExternalTargetLeaseOwner,
    workspace::WorkspaceSessionSnapshot,
};
use kerminal_domain::{files::RemotePath, identity::HostId};

use super::*;
use crate::root::workspace_files::WorkspaceFileRuntime;

/// 测试直接以引用计数观察 scope 释放，避免把普通 Clone 误当成独立文件消费者。
#[derive(Default)]
struct LeaseOwner {
    scopes: AtomicUsize,
}

impl ExternalTargetLeaseOwner for LeaseOwner {
    /// retain 必须对应新消费者，模拟实际临时目标仓储的独立所有权。
    fn retain_scope(&self, _: &HostId) -> AppResult<()> {
        self.scopes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Drop 立即释放对应 scope，使文件运行态销毁是否泄漏可被确定验证。
    fn release_scope(&self, _: &HostId) {
        self.scopes.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 空快照只建立已安装的 runtime，与 Root 完成初次 workspace 读取后的状态一致。
fn empty_snapshot() -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: None,
        focused_pane_id: None,
        selected_machine_id: None,
        terminal_tabs: Vec::new(),
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    }
}

/// 同名文件的导航去重依据完整端点，不依据可变标题或当前选中的主机。
#[test]
fn same_filename_on_distinct_hosts_has_distinct_navigation_target() {
    let first = FileEndpoint::Remote {
        host_id: HostId::parse("host-a").unwrap(),
        path: RemotePath::parse("/srv/app.txt").unwrap(),
    };
    let second = FileEndpoint::Remote {
        host_id: HostId::parse("host-b").unwrap(),
        path: RemotePath::parse("/srv/app.txt").unwrap(),
    };
    let (first_tab, _) = file_navigation_descriptor(&first, None).unwrap();
    let (second_tab, _) = file_navigation_descriptor(&second, None).unwrap();
    let id = first_tab.id().clone();
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&empty_snapshot());
    assert!(runtime.register_open_file_tab(first_tab, first.clone(), None, false));
    assert_eq!(runtime.tab_for_endpoint(&first), Some(id.clone()));
    assert_eq!(runtime.tab_for_endpoint(&second), None);
    assert!(runtime.register_open_file_tab(second_tab, second.clone(), None, false));
    assert_ne!(
        runtime.tab_for_endpoint(&first),
        runtime.tab_for_endpoint(&second)
    );
    assert!(
        runtime.ready_file_endpoints().is_empty(),
        "未装配的文件不能被空编辑器投影当成已关闭"
    );
    let (directory, lease) = runtime.file_browser_target(&id).unwrap();
    assert_eq!(
        directory,
        FileEndpoint::Remote {
            host_id: HostId::parse("host-a").unwrap(),
            path: RemotePath::parse("/srv").unwrap(),
        },
        "文件导航必须继续提供自己的父目录，不能丢失或借用另一主机"
    );
    assert!(lease.is_none());
}

/// 本机文件激活后继续浏览其目录，不能将文件路径本身当作目录发起读取。
#[test]
fn local_file_keeps_its_parent_available_for_continuous_browsing() {
    let parent = std::env::temp_dir().join("kerminal-file-navigation");
    let endpoint = FileEndpoint::Local {
        path: parent.join("document.txt"),
    };
    let (tab, transient) = file_navigation_descriptor(&endpoint, None).unwrap();
    let id = tab.id().clone();
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&empty_snapshot());
    assert!(runtime.register_open_file_tab(tab, endpoint, None, transient));
    let (directory, lease) = runtime.file_browser_target(&id).unwrap();
    assert_eq!(directory, FileEndpoint::Local { path: parent });
    assert!(lease.is_none());
    assert!(runtime.file_browser_target(&TabId::new()).is_none());
}

/// 远端和容器遵循 POSIX 名称规则，工作区标签与编辑器标签都必须保留完整 basename。
///
/// @author kongweiguang
#[test]
fn posix_file_navigation_preserves_backslash_in_title() {
    let path = RemotePath::parse(r"/srv/backup\2026.txt").unwrap();
    let endpoints = [
        FileEndpoint::Remote {
            host_id: HostId::parse("host-a").unwrap(),
            path: path.clone(),
        },
        FileEndpoint::Container {
            host_id: None,
            runtime: ContainerRuntime::Docker,
            container_id: "container-a".into(),
            path,
        },
    ];
    for endpoint in endpoints {
        let (tab, _) = file_navigation_descriptor(&endpoint, None).unwrap();
        assert_eq!(tab.title(), r"backup\2026.txt");
        let WorkspaceSessionTab::WorkspaceFile { path, .. } = tab else {
            panic!("expected file tab");
        };
        assert_eq!(path, r"/srv/backup\2026.txt");
    }
}

/// 临时目标缺失或错配 lease 必须拒绝，不能把匹配路径误当成访问授权。
#[test]
fn external_file_requires_its_exact_target_lease() {
    let endpoint = FileEndpoint::Remote {
        host_id: HostId::parse("external:one").unwrap(),
        path: RemotePath::parse("/tmp/a.txt").unwrap(),
    };
    assert!(file_navigation_descriptor(&endpoint, None).is_err());
    let owner = Arc::new(LeaseOwner {
        scopes: AtomicUsize::new(1),
    });
    let wrong =
        ExternalTargetLease::new_registered(HostId::parse("external:two").unwrap(), owner).unwrap();
    assert!(file_navigation_descriptor(&endpoint, Some(&wrong)).is_err());
}

/// 来源终端释放后独立文件 scope 继续存在，浏览投影可安全交给下一消费者而不保活原终端。
#[test]
fn transient_file_owns_a_scope_after_source_terminal_closes() {
    let owner = Arc::new(LeaseOwner {
        scopes: AtomicUsize::new(1),
    });
    let host_id = HostId::parse("external:one").unwrap();
    let terminal = ExternalTargetLease::new_registered(host_id.clone(), owner.clone()).unwrap();
    let file_lease = terminal.retain().unwrap();
    let endpoint = FileEndpoint::Remote {
        host_id,
        path: RemotePath::parse("/tmp/a.txt").unwrap(),
    };
    let (tab, transient) = file_navigation_descriptor(&endpoint, Some(&file_lease)).unwrap();
    assert!(transient);
    let id = tab.id().clone();
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&empty_snapshot());
    assert!(runtime.register_open_file_tab(tab, endpoint, Some(file_lease), transient));
    drop(terminal);
    assert_eq!(owner.scopes.load(Ordering::SeqCst), 1);
    assert!(runtime.transient_tab_ids().contains(&id));
    let (directory, browser_lease) = runtime.file_browser_target(&id).unwrap();
    assert_eq!(
        directory,
        FileEndpoint::Remote {
            host_id: HostId::parse("external:one").unwrap(),
            path: RemotePath::parse("/tmp").unwrap(),
        }
    );
    let browser_lease = browser_lease.unwrap().retain().unwrap();
    assert_eq!(owner.scopes.load(Ordering::SeqCst), 2);
    drop(runtime);
    assert_eq!(owner.scopes.load(Ordering::SeqCst), 1);
    drop(browser_lease);
    assert_eq!(owner.scopes.load(Ordering::SeqCst), 0);
}

/// 本机容器没有冻结格式可恢复的宿主 ID，必须从最终持久化投影排除。
#[test]
fn local_container_file_is_transient_and_cannot_be_restored_as_local_disk() {
    let endpoint = FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "container-a".into(),
        path: RemotePath::parse("/etc/app.txt").unwrap(),
    };
    let (tab, transient) = file_navigation_descriptor(&endpoint, None).unwrap();
    assert!(transient);
    let mut snapshot = empty_snapshot();
    snapshot.active_tab_id = Some(tab.id().clone());
    snapshot.terminal_tabs.push(tab.clone());
    let mut runtime = WorkspaceFileRuntime::from_snapshot(&empty_snapshot());
    let id = tab.id().clone();
    assert!(runtime.register_open_file_tab(tab, endpoint, None, transient));
    assert!(
        runtime.file_browser_target(&id).is_none(),
        "容器文件不能被误读为宿主机紧凑文件列表"
    );
    let persisted = crate::root::workspace_transient::without_transient_resources(
        &snapshot,
        &runtime.transient_tab_ids(),
        &Default::default(),
    )
    .unwrap();
    assert!(persisted.terminal_tabs.is_empty());
    assert_eq!(persisted.active_tab_id, None);
}
