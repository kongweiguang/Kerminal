//! JSON v1 批量导入的文件仓储端到端回读契约。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, fs, path::PathBuf};

use kerminal_application::commands::{
    MAX_SNIPPET_TRANSFER_ITEMS, SnippetImportBatchRequest, SnippetImportBatchState,
    SnippetRepository,
};
use kerminal_domain::commands::{
    SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetDefaultAction, SnippetDefinition,
    SnippetRisk, SnippetScope,
};
use kerminal_infrastructure::{
    commands::FileCommandCatalogRepository, storage::StorageCoordinator,
};
use uuid::Uuid;

struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 每个批量导入测试使用隔离数据根，不读取或改写用户的 Kerminal 配置。
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("snippet-import-roundtrip-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }

    /// 通过生产文件仓储构造真实 adapter，避免以 mock 代替磁盘格式验证。
    fn repository(&self) -> FileCommandCatalogRepository {
        FileCommandCatalogRepository::new(StorageCoordinator::new(self.0.clone()), true)
    }
}

impl Drop for TemporaryConfigRoot {
    /// 仅清理当前用例 UUID 根目录，不触碰其它应用状态。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 一次原子批次可完整保存、回读 501 项，目录查询不再被旧 500 上限截断。
#[tokio::test]
async fn five_hundred_one_snippets_round_trip_as_one_batch() {
    let temporary = TemporaryConfigRoot::new();
    let repository = temporary.repository();
    let definitions = (0..501).map(snippet_definition).collect::<Vec<_>>();
    let expected_ids = definitions
        .iter()
        .map(|definition| definition.id.clone())
        .collect::<BTreeSet<_>>();
    let request = SnippetImportBatchRequest {
        batch_id: "batch-501-roundtrip".to_owned(),
        definitions,
    };

    let committed = repository
        .commit_import_batch(request.clone())
        .await
        .expect("提交完整 501 项事务");
    let SnippetImportBatchState::Committed(receipt) = committed.clone() else {
        panic!("完整批次必须得到确定提交回执");
    };
    assert_eq!(receipt.batch_id, request.batch_id);
    assert_eq!(receipt.imported_ids.len(), 501);
    assert_eq!(
        receipt
            .imported_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        expected_ids
    );

    assert_eq!(
        repository
            .commit_import_batch(request.clone())
            .await
            .expect("同批次相同内容应幂等回读"),
        committed
    );
    assert_eq!(
        repository
            .import_batch_status(request.batch_id)
            .await
            .expect("读取持久化回执"),
        committed
    );

    let all = repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::User),
            limit: None,
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("读取完整用户目录");
    assert_eq!(all.len(), 501);
    assert_eq!(
        all.iter()
            .map(|item| item.id.clone())
            .collect::<BTreeSet<_>>(),
        expected_ids
    );
    let requested_limit = repository
        .list(SnippetCatalogListRequest {
            origin: Some(SnippetCatalogOrigin::User),
            limit: Some(MAX_SNIPPET_TRANSFER_ITEMS),
            ..SnippetCatalogListRequest::default()
        })
        .await
        .expect("读取允许的传输上限");
    assert_eq!(requested_limit.len(), 501);

    let last = repository
        .get_user("imported-0500".to_owned())
        .await
        .expect("读取末项定义")
        .expect("末项仍在磁盘");
    assert_eq!(last.title, "Fixture 500");
    assert_eq!(last.command, "printf 'fixture-500'");
    assert_eq!(last.sort_order, 500);
}

/// 构造没有敏感默认值的合法命令定义，固定每个导入文件的独立身份。
fn snippet_definition(index: usize) -> SnippetDefinition {
    SnippetDefinition {
        id: format!("imported-{index:04}"),
        title: format!("Fixture {index}"),
        command: format!("printf 'fixture-{index}'"),
        description: Some("bounded integration fixture".to_owned()),
        tags: vec!["fixture".to_owned()],
        sort_order: index as i64,
        scope: SnippetScope::Any,
        category: Some("roundtrip".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}
