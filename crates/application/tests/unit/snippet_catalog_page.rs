//! Commands 片段目录的完整读取、常用筛选、分块偏好和 cursor 契约。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetDefaultAction, SnippetPreference,
    SnippetPreferenceKey, SnippetRisk, SnippetScope, SnippetUsageAction, SnippetUsageClearResult,
    SnippetUsageReceipt, SnippetUsageRecordRequest,
};

use super::{
    SnippetCatalogPage, SnippetCatalogPageOutcome, SnippetCatalogPageRequest,
    SnippetCatalogSection, build_catalog_page, decorate_with_repository,
};
use crate::{
    AppError, AppResult,
    commands::{SnippetPreferenceAvailability, SnippetPreferenceRepository},
};

#[derive(Default)]
struct RecordingPreferences {
    batches: Mutex<Vec<usize>>,
    values: Mutex<HashMap<SnippetPreferenceKey, SnippetPreference>>,
}

impl RecordingPreferences {
    /// 测试仓储按来源与 ID 保存偏好，观察分块大小和实际装饰结果。
    fn with_preference(self, preference: SnippetPreference) -> Self {
        self.values
            .lock()
            .expect("preference test mutex should not be poisoned")
            .insert(preference.key.clone(), preference);
        self
    }

    /// 返回仓储实际收到的 key 数，以验证 adapter 500 条调用契约。
    fn batch_sizes(&self) -> Vec<usize> {
        self.batches
            .lock()
            .expect("preference test mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl SnippetPreferenceRepository for RecordingPreferences {
    /// 测试中的虚拟偏好仓储始终可用，路径只覆盖纯批量读取逻辑。
    fn availability(&self) -> SnippetPreferenceAvailability {
        SnippetPreferenceAvailability::Available
    }

    /// 记录每批尺寸并只返回实际存在的偏好行，缺失行沿用目录默认值。
    async fn list(&self, keys: Vec<SnippetPreferenceKey>) -> AppResult<Vec<SnippetPreference>> {
        self.batches
            .lock()
            .expect("preference test mutex should not be poisoned")
            .push(keys.len());
        let values = self
            .values
            .lock()
            .expect("preference test mutex should not be poisoned");
        Ok(keys
            .iter()
            .filter_map(|key| values.get(key).cloned())
            .collect())
    }

    /// 目录分页不写收藏，意外调用时明确失败。
    async fn set_favorite(
        &self,
        _key: SnippetPreferenceKey,
        _favorite: bool,
    ) -> AppResult<SnippetPreference> {
        Err(AppError::unsupported(
            "catalog page test does not write favorites",
        ))
    }

    /// 目录分页不写动作回执，意外调用时明确失败。
    async fn record_usage(
        &self,
        _request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt> {
        Err(AppError::unsupported(
            "catalog page test does not record usage",
        ))
    }

    /// 目录分页不清理历史，意外调用时明确失败。
    async fn clear_usage(&self) -> AppResult<SnippetUsageClearResult> {
        Err(AppError::unsupported(
            "catalog page test does not clear usage",
        ))
    }
}

/// 目录和偏好超出旧 MCP 上限时仍完整分页，偏好查询严格按 500 个 key 分批。
#[tokio::test]
async fn full_catalog_pages_and_preferences_cross_legacy_limits() {
    let catalog = (0..1_001)
        .map(|index| {
            item(
                &format!("builtin-{index}"),
                SnippetCatalogOrigin::Builtin,
                "core",
            )
        })
        .collect::<Vec<_>>();
    let first_key = key(SnippetCatalogOrigin::Builtin, "builtin-0");
    let preferences = RecordingPreferences::default().with_preference(SnippetPreference {
        key: first_key,
        favorite: true,
        use_count: 7,
        last_action: Some(SnippetUsageAction::Run),
        last_used_at_unix_ms: Some(42),
    });
    let catalog = decorate_with_repository(Some(&preferences), catalog)
        .await
        .expect("分批读取所有偏好");

    assert_eq!(catalog.len(), 1_001);
    assert_eq!(preferences.batch_sizes(), vec![500, 500, 1]);
    assert!(catalog[0].favorite);
    let mut request = page_request(SnippetCatalogSection::Library, 100);
    let mut visited = Vec::new();
    loop {
        let page = match build_catalog_page(catalog.clone(), &request, 100) {
            SnippetCatalogPageOutcome::Page(page) => page,
            SnippetCatalogPageOutcome::StaleCursor => panic!("刚生成的 cursor 应匹配原快照"),
        };
        assert_eq!(page.total, 1_001);
        visited.extend(page.items.iter().map(|item| item.id.clone()));
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request.cursor = Some(cursor);
    }
    assert_eq!(visited.len(), 1_001);
    assert_eq!(visited.first().map(String::as_str), Some("builtin-0"));
    assert_eq!(visited.last().map(String::as_str), Some("builtin-1000"));
}

/// Common 从所有来源收集收藏和用过的条目，按收藏、最近使用、次数和目录顺序稳定排列。
#[test]
fn common_is_global_and_orders_favorites_then_usage_stably() {
    let mut catalog = vec![
        item("tie-first", SnippetCatalogOrigin::User, "mine"),
        item("favorite-old", SnippetCatalogOrigin::Builtin, "core"),
        item("tie-second", SnippetCatalogOrigin::Builtin, "network"),
        item("favorite-new", SnippetCatalogOrigin::User, "mine"),
        item("unused", SnippetCatalogOrigin::Builtin, "core"),
    ];
    catalog[0].use_count = 3;
    catalog[0].last_used_at_unix_ms = Some(200);
    catalog[1].favorite = true;
    catalog[1].last_used_at_unix_ms = Some(100);
    catalog[2].use_count = 3;
    catalog[2].last_used_at_unix_ms = Some(200);
    catalog[3].favorite = true;
    catalog[3].last_used_at_unix_ms = Some(300);

    let page = expect_page(build_catalog_page(
        catalog,
        &page_request(SnippetCatalogSection::Common, 20),
        20,
    ));

    assert_eq!(page.total, 4);
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["favorite-new", "favorite-old", "tie-first", "tie-second"]
    );
}

/// 空 Common 无偏好时回退 core 前八条，关键词则覆盖 Mine tab 搜索整个内置目录。
#[test]
fn common_fallback_and_cross_section_search_keep_catalog_semantics() {
    let catalog = (0..10)
        .map(|index| {
            item(
                &format!("core-{index}"),
                SnippetCatalogOrigin::Builtin,
                "core",
            )
        })
        .chain(std::iter::once(item(
            "other-pack-hit",
            SnippetCatalogOrigin::Builtin,
            "network",
        )))
        .collect::<Vec<_>>();
    let common = expect_page(build_catalog_page(
        catalog.clone(),
        &page_request(SnippetCatalogSection::Common, 20),
        20,
    ));
    assert_eq!(common.total, 8);
    assert_eq!(
        common.items.first().map(|item| item.id.as_str()),
        Some("core-0")
    );
    assert_eq!(
        common.items.last().map(|item| item.id.as_str()),
        Some("core-7")
    );

    let mut search = page_request(SnippetCatalogSection::Mine, 20);
    search.query = Some("other-pack-hit".to_owned());
    let results = expect_page(build_catalog_page(catalog, &search, 20));
    assert_eq!(results.total, 1);
    assert_eq!(results.items[0].origin, SnippetCatalogOrigin::Builtin);
}

/// continuation 在全目录偏好或任一过滤条件变化后返回 typed stale 结果。
#[test]
fn cursor_rejects_catalog_preference_and_filter_changes() {
    let catalog = vec![
        item("first", SnippetCatalogOrigin::Builtin, "core"),
        item("second", SnippetCatalogOrigin::Builtin, "core"),
    ];
    let request = page_request(SnippetCatalogSection::Library, 1);
    let first = expect_page(build_catalog_page(catalog.clone(), &request, 1));
    let cursor = first.next_cursor.expect("第二项应有 continuation");

    let mut catalog_changed = catalog.clone();
    catalog_changed[1].title.push_str(" changed");
    let mut stale_request = request.clone();
    stale_request.cursor = Some(cursor.clone());
    assert!(matches!(
        build_catalog_page(catalog_changed, &stale_request, 1),
        SnippetCatalogPageOutcome::StaleCursor
    ));

    let mut preference_changed = catalog.clone();
    preference_changed[0].favorite = true;
    stale_request.cursor = Some(cursor.clone());
    assert!(matches!(
        build_catalog_page(preference_changed, &stale_request, 1),
        SnippetCatalogPageOutcome::StaleCursor
    ));

    stale_request.section = SnippetCatalogSection::Mine;
    assert!(matches!(
        build_catalog_page(catalog, &stale_request, 1),
        SnippetCatalogPageOutcome::StaleCursor
    ));
}

/// 构造所有页查询共用的 scope-neutral 请求，测试只覆盖过滤器明确指定的维度。
fn page_request(section: SnippetCatalogSection, page_size: usize) -> SnippetCatalogPageRequest {
    SnippetCatalogPageRequest {
        section,
        query: None,
        scope: None,
        page_size,
        cursor: None,
    }
}

/// 断言测试构造结果为正常 page，以便 stale 分支有单独且明确的检查。
fn expect_page(outcome: SnippetCatalogPageOutcome) -> SnippetCatalogPage {
    match outcome {
        SnippetCatalogPageOutcome::Page(page) => page,
        SnippetCatalogPageOutcome::StaleCursor => panic!("初始请求不应产生 stale cursor"),
    }
}

/// 构造最小目录投影，保持测试与 production page filter 使用相同领域类型。
fn item(id: &str, origin: SnippetCatalogOrigin, pack: &str) -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: id.to_owned(),
        origin,
        title: id.to_owned(),
        description: String::new(),
        template: "printf ok".to_owned(),
        category: "test".to_owned(),
        pack: pack.to_owned(),
        tags: Vec::new(),
        scope: SnippetScope::Any,
        platforms: Vec::new(),
        shells: Vec::new(),
        capabilities: Vec::new(),
        risk: SnippetRisk::Inspect,
        sensitive: false,
        duration: String::new(),
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
        catalog_version: None,
        command_spec: None,
        owner: None,
        tested_version: None,
        source_name: None,
        source_url: None,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: 0,
        updated_at: String::new(),
    }
}

/// 构造来源隔离的 preference key，测试重复 ID 时不把另一来源偏好混入当前行。
fn key(origin: SnippetCatalogOrigin, snippet_id: &str) -> SnippetPreferenceKey {
    SnippetPreferenceKey {
        origin,
        snippet_id: snippet_id.to_owned(),
    }
}
