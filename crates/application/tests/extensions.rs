//! 扩展页手动放入插件、内置条目枚举与生命周期校验的契约。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    extensions::{
        CatalogSelection, CatalogSnapshot, CatalogSource, DropInInspection, ExtensionChange,
        ExtensionReceipt, ExtensionsService, InstallProgress, LocalPackageInspection,
        PluginPackageStore,
    },
    plugins::CORE_PLUGIN_ID,
};
use kerminal_domain::plugins::{PluginId, PluginInstallStatus, PluginStoreState, PluginVersion};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// 目录端口不参与本组用例；显式失败避免测试误以为刷新过真实目录。
struct UnavailableCatalog;

#[async_trait]
impl CatalogSource for UnavailableCatalog {
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        Ok(None)
    }

    async fn readme(
        &self,
        _id: &PluginId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        Ok(None)
    }

    async fn refresh(&self, _cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        Err(AppError::infrastructure("测试未配置插件目录"))
    }
}

/// 记录服务层实际转发的意图；回退拒绝测试要求它保持为空。
#[derive(Default)]
struct FakePackages {
    inspections: Vec<DropInInspection>,
    changes: Mutex<Vec<ExtensionChange>>,
    confirmations: Mutex<Vec<(PluginId, String, u64)>>,
    revision: Mutex<u64>,
}

#[async_trait]
impl PluginPackageStore for FakePackages {
    async fn state(&self) -> AppResult<PluginStoreState> {
        Ok(PluginStoreState::default())
    }

    async fn install_catalog(
        &self,
        _selection: CatalogSelection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("测试不安装目录插件"))
    }

    async fn inspect_file(
        &self,
        _path: PathBuf,
        _cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        Err(AppError::unsupported("测试不检查本地插件包"))
    }

    async fn install_file(
        &self,
        _path: PathBuf,
        _inspected: LocalPackageInspection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("测试不安装本地插件包"))
    }

    async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        self.changes.lock().unwrap().push(change);
        Ok(ExtensionReceipt {
            revision: *self.revision.lock().unwrap(),
            restart_required: true,
            cleanup_warning: None,
        })
    }

    async fn inspect_drop_ins(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>> {
        Ok(self.inspections.clone())
    }

    async fn confirm_drop_in(
        &self,
        id: PluginId,
        expected_fingerprint: String,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        self.confirmations
            .lock()
            .unwrap()
            .push((id, expected_fingerprint, expected_revision));
        Ok(ExtensionReceipt {
            revision: expected_revision.saturating_add(1),
            restart_required: true,
            cleanup_warning: None,
        })
    }

    async fn delete_data(&self, _id: PluginId) -> AppResult<()> {
        Ok(())
    }
}

/// 服务与测试共用同一份假仓储，断言转发而不是复制业务逻辑。
fn service(packages: &Arc<FakePackages>) -> ExtensionsService {
    ExtensionsService::new(Arc::new(UnavailableCatalog), packages.clone())
}

/// 目录指纹只需满足展示契约，应用层不解析其内容；使用固定 v1 形状便于断言。
fn fingerprint() -> String {
    format!("v1:{}", "ab".repeat(32))
}

/// 观察结果覆盖已发现但未确认的常规项；具体目录规则由基础设施测试覆盖。
fn inspection(id: &str, revision: u64) -> DropInInspection {
    DropInInspection {
        id: PluginId::parse(id).unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        name: BTreeMap::from([("en".into(), "Notes".into())]),
        permissions: vec!["network:example.com".into()],
        fingerprint: Some(fingerprint()),
        enabled: false,
        confirmed: false,
        status: PluginInstallStatus::Ready,
        revision,
    }
}

/// 回退入口只保留编译外形；服务层固定拒绝，不能把排队意图传给存储。
#[tokio::test]
async fn rollback_is_rejected_without_touching_store() {
    let packages = Arc::new(FakePackages::default());
    let service = service(&packages);
    let id = PluginId::parse("acme.notes").unwrap();
    let error = service
        .change(ExtensionChange::Rollback(id))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(error.public_message(), "插件回退已移除");
    assert!(packages.changes.lock().unwrap().is_empty());
}

/// 检查与确认必须原样转发指纹和修订，存储端才能在锁内复核同一份快照。
#[tokio::test]
async fn drop_in_inspection_and_confirmation_are_delegated() {
    let packages = Arc::new(FakePackages {
        inspections: vec![inspection("acme.notes", 4)],
        ..Default::default()
    });
    let service = service(&packages);
    let id = PluginId::parse("acme.notes").unwrap();
    let inspections = service
        .inspect_drop_ins(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inspections.len(), 1);
    assert_eq!(inspections[0].revision, 4);
    assert_eq!(
        inspections[0].fingerprint.as_deref(),
        Some(fingerprint().as_str())
    );

    let receipt = service
        .confirm_drop_in(id.clone(), fingerprint(), 4)
        .await
        .unwrap();
    assert!(receipt.restart_required);
    assert_eq!(receipt.revision, 5);
    assert_eq!(
        packages.confirmations.lock().unwrap().as_slice(),
        &[(id, fingerprint(), 4)]
    );
}

/// 删除缺失记录是普通 CAS 意图，服务层不额外确认，也不触碰数据目录。
#[tokio::test]
async fn missing_record_removal_is_forwarded() {
    let packages = Arc::new(FakePackages {
        revision: Mutex::new(9),
        ..Default::default()
    });
    let service = service(&packages);
    let id = PluginId::parse("acme.notes").unwrap();
    let receipt = service
        .change(ExtensionChange::RemoveMissingRecord(id.clone()))
        .await
        .unwrap();
    assert_eq!(receipt.revision, 9);
    let changes = packages.changes.lock().unwrap();
    assert!(
        matches!(changes.as_slice(), [ExtensionChange::RemoveMissingRecord(changed)] if changed == &id)
    );
}

/// 内置条目必须直接来自冻结注册表：数量和顺序稳定，界面才不必复制名单；
/// 只有核心单元不可关闭，名称保持注册表原文交给界面本地化。
#[test]
fn builtin_units_expose_static_identity_in_registry_order() {
    let packages = Arc::new(FakePackages::default());
    let service = service(&packages);
    let units = service.builtin_units().unwrap();
    assert_eq!(units.len(), 13);
    assert_eq!(units[0].id.as_str(), CORE_PLUGIN_ID);
    assert!(!units[0].can_disable);
    assert!(units[1..].iter().all(|unit| unit.can_disable));
    assert!(
        units
            .iter()
            .all(|unit| !unit.id.as_str().is_empty() && !unit.name.trim().is_empty())
    );
}

/// 核心停用是界面语义错误，必须在触达存储前拒绝，避免排队一个永远生效不了的意图。
#[tokio::test]
async fn core_disable_is_rejected_without_touching_store() {
    let packages = Arc::new(FakePackages::default());
    let service = service(&packages);
    let core = PluginId::parse(CORE_PLUGIN_ID).unwrap();
    let error = service
        .change(ExtensionChange::Disable(core))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(error.public_message(), "核心插件不可停用");
    assert!(packages.changes.lock().unwrap().is_empty());
}

/// 内置单元没有安装记录，卸载类意图在应用层拒绝；启停仍按普通意图转发给存储端。
#[tokio::test]
async fn builtin_enable_is_forwarded_but_uninstall_is_rejected() {
    let packages = Arc::new(FakePackages::default());
    let service = service(&packages);
    let ssh = PluginId::parse("kerminal.ssh").unwrap();

    let receipt = service
        .change(ExtensionChange::Enable(ssh.clone()))
        .await
        .unwrap();
    assert!(receipt.restart_required);

    for change in [
        ExtensionChange::Uninstall(ssh.clone()),
        ExtensionChange::RemoveMissingRecord(ssh.clone()),
    ] {
        let error = service.change(change).await.unwrap_err();
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
        assert_eq!(error.public_message(), "内置单元没有安装记录");
    }

    let changes = packages.changes.lock().unwrap();
    assert!(matches!(changes.as_slice(), [ExtensionChange::Enable(changed)] if changed == &ssh));
}

/// 非内置身份不受内置语义校验影响，启停意图保持原样转发。
#[tokio::test]
async fn non_builtin_disable_is_still_forwarded() {
    let packages = Arc::new(FakePackages::default());
    let service = service(&packages);
    let id = PluginId::parse("acme.notes").unwrap();
    service
        .change(ExtensionChange::Disable(id.clone()))
        .await
        .unwrap();
    let changes = packages.changes.lock().unwrap();
    assert!(matches!(changes.as_slice(), [ExtensionChange::Disable(changed)] if changed == &id));
}
