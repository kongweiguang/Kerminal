//! 冻结内置目录资源及用户/内置片段合并契约。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::commands::SnippetRepository;
use kerminal_domain::commands::{
    SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetDefaultAction, SnippetDefinition,
    SnippetRisk, SnippetScope,
};
use kerminal_infrastructure::{
    commands::FileCommandCatalogRepository, storage::StorageCoordinator,
};

struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 每个集成测试使用隔离的 `.kerminal` 根，避免读取或改写真实用户配置。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-builtin-snippets-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }

    /// 建立真实文件片段 adapter，验证嵌入式目录和用户 TOML 的统一查询行为。
    fn repository(&self) -> FileCommandCatalogRepository {
        FileCommandCatalogRepository::new(StorageCoordinator::new(self.0.clone()), true)
    }
}

impl Drop for TemporaryConfigRoot {
    /// 测试结束后仅清理本测试创建的临时根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 冻结资源必须完整投影旧 v2 的稳定 ID 与只读来源元数据。
#[tokio::test]
async fn frozen_v2_resources_project_complete_catalog_metadata() {
    let repository = TemporaryConfigRoot::new().repository();
    let catalog = repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::Builtin),
            limit: Some(500),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("解析冻结的内置片段资源");
    assert_eq!(catalog.len(), 39);

    let mut ids = catalog
        .iter()
        .map(|item| item.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 39, "内置 ID 必须唯一");
    assert!(catalog.iter().all(|item| {
        item.origin == SnippetCatalogOrigin::Builtin
            && item.command_spec.is_some()
            && item.owner.is_some()
            && item.tested_version.is_some()
            && item.catalog_version.is_some()
            && item.source_name.is_some()
            && item.source_url.is_some()
    }));
}

/// 统一目录按来源和 scope 过滤，Any 片段继续匹配 Local/SSH 请求。
#[tokio::test]
async fn list_merges_builtin_and_user_catalog_with_legacy_filters() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository();
    repository
        .save_user(user_definition())
        .await
        .expect("创建临时用户片段");

    let all = repository
        .list(SnippetCatalogListRequest {
            limit: Some(500),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("读取统一目录");
    assert_eq!(all.len(), 40);
    assert_eq!(
        all.iter()
            .filter(|item| item.origin == SnippetCatalogOrigin::Builtin)
            .count(),
        39
    );
    assert_eq!(
        all.iter()
            .find(|item| item.id == "snippet.user.catalog-test")
            .map(|item| item.pack.as_str()),
        Some("mine")
    );

    let builtin_only = repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::Builtin),
            limit: Some(500),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("按来源筛选内置目录");
    assert_eq!(builtin_only.len(), 39);
    assert!(
        builtin_only
            .iter()
            .all(|item| item.origin == SnippetCatalogOrigin::Builtin)
    );

    let local = repository
        .list(SnippetCatalogListRequest {
            scope: Some(SnippetScope::Local),
            limit: Some(500),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("按本地范围筛选目录");
    assert!(
        local.iter().any(|item| {
            item.id == "snippet.user.catalog-test" && item.scope == SnippetScope::Any
        })
    );

    let any_scope = repository
        .list(SnippetCatalogListRequest {
            scope: Some(SnippetScope::Any),
            limit: Some(500),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("按通用范围筛选目录");
    assert!(any_scope.iter().all(|item| item.scope == SnippetScope::Any));
}

/// 精确查询按复合身份选择目录；用户文件与内置资源可以同 ID 而不互相回退。
#[tokio::test]
async fn exact_lookup_keeps_origin_identity_when_ids_collide() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository();
    let builtin = repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::Builtin),
            limit: None,
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("读取内置目录")
        .into_iter()
        .next()
        .expect("内置目录非空");
    let mut user = user_definition();
    user.id = builtin.id.clone();
    user.title = "same ID user entry".to_owned();
    repository
        .save_user(user)
        .await
        .expect("创建与内置项同 ID 的用户定义");

    let user_item = repository
        .get_catalog_item(SnippetCatalogOrigin::User, builtin.id.clone())
        .await
        .expect("精确读取用户来源")
        .expect("用户来源存在");
    let builtin_item = repository
        .get_catalog_item(SnippetCatalogOrigin::Builtin, builtin.id.clone())
        .await
        .expect("精确读取内置来源")
        .expect("内置来源存在");

    assert_eq!(user_item.origin, SnippetCatalogOrigin::User);
    assert_eq!(user_item.title, "same ID user entry");
    assert_eq!(builtin_item.origin, SnippetCatalogOrigin::Builtin);
    assert_eq!(builtin_item.title, builtin.title);
    assert!(
        repository
            .get_catalog_item(
                SnippetCatalogOrigin::Builtin,
                "missing-user-only".to_owned()
            )
            .await
            .expect("读取不存在的内置项")
            .is_none()
    );
}

/// 用简单有效定义验证用户目录，不在测试中引入命令正文以外的状态依赖。
fn user_definition() -> SnippetDefinition {
    SnippetDefinition {
        id: "snippet.user.catalog-test".to_owned(),
        title: "Catalog test".to_owned(),
        command: "printf ok".to_owned(),
        description: Some("isolated catalog fixture".to_owned()),
        tags: vec!["fixture".to_owned()],
        sort_order: 0,
        scope: SnippetScope::Any,
        category: Some("test".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}
