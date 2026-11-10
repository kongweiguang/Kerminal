//! 原生集合只按稳定业务键选择，并为 RPC 查询保留分页源坐标。
//!
//! @author kongweiguang

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use gpui_kit::{AppContext as _, TestAppContext};
use kerminal_plugin_protocol::native::parse_native_view;
use serde_json::{Value, json};

use super::{NativeCollection, activation_action, inline_rows, resolve_binding, row_key};
use crate::plugins::external::{renderer::NativeCollectionConfig, state::ViewState};

/// 从真实协议解析器建立集合节点，测试数据不会绕过视图字段校验。
fn node_from(source: &str) -> kerminal_plugin_protocol::native::NativeNode {
    parse_native_view(source.as_bytes())
        .expect("test native view must validate")
        .root
}

/// 通过父键路径读取集合数据，缺失对象或非数组都不会合成插件行。
#[test]
fn resolves_inline_rows_from_declared_binding() {
    let node = node_from(
        r#"{"schema":1,"data":{"panel":{"rows":[{"id":"a"}]}},"root":{"type":"list","key":"id","title":"name","items":"$panel.rows"}}"#,
    );
    let data = json!({"panel":{"rows":[{"id":"a"}]}});
    assert_eq!(inline_rows(&data, &node), vec![json!({"id":"a"})]);
    assert!(resolve_binding(&json!({"panel": 4}), "$panel.rows").is_none());
}

/// 菜单与激活身份来自真实节点类型，行顺序变化不会改变选择键。
#[test]
fn uses_stable_item_keys_and_declared_activation_action() {
    let node = node_from(
        r#"{"schema":1,"root":{"type":"table","items":"$rows","key":"record_id","columns":[{"key":"name","title":"Name"}],"on_activate":"open"}}"#,
    );
    let first = json!({"record_id":"record-b","name":"Second"});
    let second = json!({"record_id":"record-a","name":"First"});
    assert_eq!(row_key(&first, &node).as_deref(), Some("record-b"));
    assert_eq!(row_key(&second, &node).as_deref(), Some("record-a"));
    assert_eq!(activation_action(&node), Some("open"));
}

/// RPC 投影复用 ViewState 的 PageRow 坐标，普通数据补丁不清除分页行的稳定选择。
#[gpui_kit::test]
fn applies_rpc_pages_and_preserves_selection_on_data_sync(app: &mut TestAppContext) {
    let node = node_from(
        r#"{"schema":1,"root":{"id":"records","type":"list","key":"id","title":"name","source":"rpc"}}"#,
    );
    let config = NativeCollectionConfig {
        node_key: "records".to_owned(),
        node,
        data: json!({}),
        translations: BTreeMap::new(),
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let collection = cx.new(|cx| NativeCollection::new(config, window, cx));
        *mounted.borrow_mut() = Some(collection.clone());
        gpui_kit::base::Root::new(collection, window, cx)
    });
    let entity = slot.borrow_mut().take().expect("collection mounted");
    let mut state = ViewState::new(json!({}), 1);
    state.queue_page("records", 30..32).expect("page range");
    let ticket = state
        .take_page_requests("records")
        .expect("request tickets")
        .pop()
        .expect("one ticket");
    state
        .finish_page(
            &ticket,
            80,
            vec![
                json!({"id":"r30","name":"Row 30"}),
                json!({"id":"r31","name":"Row 31"}),
            ],
        )
        .expect("valid page");
    let rows = state.page_rows("records").to_vec();
    visual.update(|_, cx| {
        let active_query = entity.read(cx).query();
        entity.update(cx, |collection, cx| {
            collection.apply_page(&active_query, &rows, 80, cx);
            collection.selected.insert("r30".to_owned());
            collection.apply_data(json!({"patch": true}), cx);
        });
        let collection = entity.read(cx);
        let (_, page) = collection
            .pages
            .iter()
            .find(|(query, _)| query == &active_query)
            .expect("active page applied");
        assert_eq!(page.total, 80);
        assert_eq!(
            page.rows.get(&30),
            Some(&json!({"id":"r30","name":"Row 30"}))
        );
        assert!(collection.selected.contains("r30"));
    });
}

/// 本地化使用集合冻结的翻译映射，未声明的标记保留原文方便定位缺失资源。
#[test]
fn localizes_declared_collection_labels() {
    let node = node_from(
        r#"{"schema":1,"root":{"type":"table","items":"$rows","key":"id","columns":[{"key":"name","title":"%title%"}]}}"#,
    );
    let collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "rows".to_owned(),
        node,
        data: Value::Null,
        translations: BTreeMap::from([("title".to_owned(), "记录".to_owned())]),
    });
    let kerminal_plugin_protocol::native::NativeNodeKind::Table { columns, .. } =
        &collection.config.node.kind
    else {
        panic!("expected table");
    };
    assert_eq!(collection.localized(&columns[0].title), "记录");
    assert_eq!(collection.localized("%missing%"), "%missing%");
}

/// 长距离往返不能永久累积已浏览行；淘汰仅影响数据缓存，业务选择和源坐标保持稳定。
#[test]
fn rpc_cache_budget_preserves_viewport_new_page_and_stable_selection() {
    use std::rc::Rc;
    let node = node_from(
        r#"{"schema":1,"root":{"id":"rows","type":"list","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "rows".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let query = collection.query();
    collection.visible = 10..30;
    collection.selected.insert("r4000".into());
    for start in (0..20_000).step_by(100) {
        if collection.pages.is_empty() {
            collection.pages.push((
                query.clone(),
                super::CachedPage {
                    total: 100_000,
                    rows: Rc::new(BTreeMap::new()),
                },
            ));
        }
        let page = &mut collection.pages[0].1;
        for index in start..start + 100 {
            Rc::make_mut(&mut page.rows).insert(
                index,
                json!({"id":format!("r{index}"),"name":format!("Row {index}")}),
            );
        }
        collection.trim_cached_rows(&query, start..start + 100);
        assert!(
            collection
                .pages
                .iter()
                .map(|(_, page)| page.rows.len())
                .sum::<usize>()
                <= super::MAX_CACHED_ROWS
        );
        assert!((start..start + 100).all(|index| collection.pages[0].1.rows.contains_key(&index)));
    }
    let (total, rows) = collection.rows_for(&query);
    assert_eq!(total, 100_000);
    assert!((10..30).all(|index| rows.contains_key(&index)));
    assert!(!rows.contains_key(&4000));
    assert!(collection.selected.contains("r4000"));
    assert_eq!(
        row_key(rows.get(&19_999).unwrap(), &collection.config.node).as_deref(),
        Some("r19999")
    );
}

/// 展开父行 JSON 可以淘汰，但轻量结构元数据必须继续保留子树槽位和稳定滚动长度。
/// RPC 节点 ID 与查询目标一致，避免无效样本在缓存断言前被协议拒绝。
#[test]
fn cache_budget_is_shared_across_queries_and_protects_expanded_ancestors() {
    use std::rc::Rc;
    let node = node_from(
        r#"{"schema":1,"root":{"id":"tree","type":"tree","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "tree".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let root = collection.query();
    let child = super::CollectionQuery {
        parent_key: Some("expanded".into()),
        sort: None,
    };
    collection.expanded.insert("expanded".into());
    collection.pages = vec![
        (root.clone(), super::CachedPage { total:5000, rows:Rc::new((0..5000).map(|index| (index, json!({"id": if index == 4999 { "expanded".into() } else { format!("root-{index}") }, "has_children":true}))).collect()) }),
        (child.clone(), super::CachedPage { total:5000, rows:Rc::new((0..5000).map(|index| (index, json!({"id":format!("child-{index}")}))).collect()) }),
    ];
    collection.trim_cached_rows(&child, 4900..5000);
    assert!(
        collection
            .pages
            .iter()
            .map(|(_, page)| page.rows.len())
            .sum::<usize>()
            <= super::MAX_CACHED_ROWS
    );
    assert_eq!(collection.pages[0].1.total, 5000);
    assert!(!collection.pages[0].1.rows.contains_key(&4999));
    assert_eq!(
        collection.tree_positions[&root]
            .get(&4999)
            .map(String::as_str),
        Some("expanded")
    );
    assert_eq!(collection.flatten_tree().len(), 10_000);
    assert!((4900..5000).all(|index| collection.pages[1].1.rows.contains_key(&index)));
}

/// 绘制帧共享相同 RPC JSON 快照，随后到达的页更新不能反向改变已经捕获的行。
#[test]
fn rpc_render_snapshots_share_storage_until_page_mutation() {
    let node = node_from(
        r#"{"schema":1,"root":{"id":"rows","type":"list","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "rows".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let query = collection.query();
    collection.pages.push((
        query.clone(),
        super::CachedPage {
            total: 100_000,
            rows: Rc::new(BTreeMap::from([(42, json!({"id":"stable","name":"old"}))])),
        },
    ));
    let (_, first) = collection.rows_for(&query);
    let (_, second) = collection.rows_for(&query);
    assert!(Rc::ptr_eq(&first, &second));
    Rc::make_mut(&mut collection.pages[0].1.rows).insert(42, json!({"id":"stable","name":"new"}));
    let (_, latest) = collection.rows_for(&query);
    assert!(!Rc::ptr_eq(&first, &latest));
    assert_eq!(first[&42]["name"], json!("old"));
    assert_eq!(latest[&42]["name"], json!("new"));
    assert_eq!(
        row_key(&latest[&42], &collection.config.node).as_deref(),
        Some("stable")
    );
}

/// 十万行固定尺寸只在行数或实际字体行高变化时重建，普通帧复用同一分配。
#[test]
fn fixed_row_geometry_reuses_allocation_until_count_or_height_changes() {
    let node = node_from(
        r#"{"schema":1,"root":{"id":"rows","type":"list","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "rows".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let first = collection.row_sizes(100_000, gpui_kit::px(40.));
    for _ in 0..10 {
        assert!(Rc::ptr_eq(
            &first,
            &collection.row_sizes(100_000, gpui_kit::px(40.))
        ));
    }
    let scaled = collection.row_sizes(100_000, gpui_kit::px(50.));
    assert!(!Rc::ptr_eq(&first, &scaled));
    assert_eq!(scaled[99_999].height, gpui_kit::px(50.));
    assert_eq!(first[99_999].height, gpui_kit::px(40.));
    let shrunk = collection.row_sizes(100, gpui_kit::px(50.));
    assert_eq!(shrunk.len(), 100);
    assert!(!Rc::ptr_eq(&scaled, &shrunk));
}

/// 两个父查询的叶行同时可见时，超预算分页回执不能只保护最后查询并淘汰另一子树的屏幕行。
/// RPC 节点 ID 与分页目标一致，确保测试覆盖真实分页而非提前触发协议拒绝。
#[gpui_kit::test]
fn tree_cache_eviction_preserves_all_visible_parent_query_ranges(app: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;
    let node = node_from(
        r#"{"schema":1,"root":{"id":"tree","type":"tree","source":"rpc","key":"id","title":"name"}}"#,
    );
    let config = NativeCollectionConfig {
        node_key: "tree".into(),
        node: node.clone(),
        data: Value::Null,
        translations: BTreeMap::new(),
    };
    app.update(crate::init);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let collection = cx.new(|cx| NativeCollection::new(config, window, cx));
        *mounted.borrow_mut() = Some(collection.clone());
        gpui_kit::base::Root::new(collection, window, cx)
    });
    let entity = slot.borrow_mut().take().unwrap();
    let a = super::CollectionQuery {
        parent_key: Some("a".into()),
        sort: None,
    };
    let b = super::CollectionQuery {
        parent_key: Some("b".into()),
        sort: None,
    };
    let mut state = ViewState::new(json!({}), 1);
    state.queue_page("tree", 2050..2150).unwrap();
    let ticket = state.take_page_requests("tree").unwrap().pop().unwrap();
    state
        .finish_page(
            &ticket,
            5000,
            (2050..2150)
                .map(|index| json!({"id":format!("b-{index}"),"name":"incoming"}))
                .collect(),
        )
        .unwrap();
    let incoming = state.page_rows("tree").to_vec();
    visual.update(|window,cx| {
        entity.update(cx,|collection,cx| {
            collection.expanded.extend(["a".into(),"b".into()]);
            collection.pages = [&a,&b].into_iter().map(|query| (query.clone(),super::CachedPage {
                total:5000,rows:Rc::new((0..2050).map(|index|(index,json!({"id":format!("{}-{index}",query.parent_key.as_deref().unwrap()),"name":"leaf"}))).collect())
            })).collect();
            let slots = vec![
                super::TreeSlot { query:a.clone(),index:2047,depth:1,row:collection.pages[0].1.rows.get(&2047).cloned() },
                super::TreeSlot { query:b.clone(),index:0,depth:1,row:collection.pages[1].1.rows.get(&0).cloned() },
            ];
            collection.visible_queries.clear();
            let muted = cx.theme().muted_foreground;
            let elements = super::render_tree_range(collection,&slots,0..2,&node,muted,window,cx);
            assert_eq!(elements.len(),2);
            assert_eq!(collection.visible_queries.get(&a),Some(&(2047..2048)));
            assert_eq!(collection.visible_queries.get(&b),Some(&(0..1)));
            assert_eq!(collection.last_query,b);
            collection.apply_page(&b,&incoming,5000,cx);
            assert!(collection.pages.iter().map(|(_,page)|page.rows.len()).sum::<usize>() <= super::MAX_CACHED_ROWS);
            assert!(collection.pages[0].1.rows.contains_key(&2047));
            assert!(collection.pages[1].1.rows.contains_key(&0));
            assert!(!collection.pages[0].1.rows.contains_key(&2049));
            assert!((2050..2150).all(|index|collection.pages[1].1.rows.contains_key(&index)));
            #[cfg(feature = "right-rail-acceptance")]
            {
                // 新页回执尚未经过行构造，不能把 incoming 缓存认作已绘制业务行。
                assert_eq!(collection.rendered_visible_rows, vec![(0,"a-2047".into()),(1,"b-0".into())]);
                let before = collection.external_plugin_acceptance_snapshot();
                assert!(!before["visible_rows"].as_array().unwrap().iter().any(|row|row["key"] == json!("b-2050")));
                collection.rendered_visible_rows.clear();
                let (_,rows) = collection.rows_for(&b);
                let _ = super::render_flat_range(collection,&node,&rows,2050..2051,muted,cx);
                assert_eq!(collection.rendered_visible_rows,vec![(2050,"b-2050".into())]);
            }
        });
    });
}

/// 超过预算的展开集合只保留轻量结构，不以展开标记无限固定完整行 JSON，也不丢用户展开意图。
#[test]
fn many_expanded_branches_keep_structure_without_pinning_json() {
    let node = node_from(
        r#"{"schema":1,"root":{"id":"tree","type":"tree","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "tree".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let root = collection.query();
    collection
        .expanded
        .extend((0..6000).map(|index| format!("branch-{index}")));
    collection.pages.push((root.clone(),super::CachedPage { total:6000,rows:Rc::new((0..6000).map(|index|(index,json!({"id":format!("branch-{index}"),"has_children":true,"payload":"full row"}))).collect()) }));
    collection.trim_cached_rows(&root, 5900..6000);
    assert_eq!(collection.pages[0].1.rows.len(), super::MAX_CACHED_ROWS);
    assert_eq!(collection.expanded.len(), 6000);
    assert_eq!(collection.tree_positions[&root].len(), 6000);
    assert_eq!(collection.flatten_tree().len(), 6000);
}

/// 收起祖先后保留后代展开意图，但隐藏后代不再固定完整 JSON，重新展开时仍按元数据恢复槽位。
#[test]
fn collapsed_ancestor_does_not_pin_expanded_descendant_payloads() {
    let node = node_from(
        r#"{"schema":1,"root":{"id":"tree","type":"tree","source":"rpc","key":"id","title":"name"}}"#,
    );
    let mut collection = NativeCollection::from_config(NativeCollectionConfig {
        node_key: "tree".into(),
        node,
        data: Value::Null,
        translations: BTreeMap::new(),
    });
    let root = collection.query();
    let child = super::CollectionQuery {
        parent_key: Some("ancestor".into()),
        sort: None,
    };
    collection.expanded.insert("ancestor".into());
    collection
        .expanded
        .extend((0..5000).map(|index| format!("descendant-{index}")));
    collection.pages = vec![
        (
            root.clone(),
            super::CachedPage {
                total: 6000,
                rows: Rc::new(BTreeMap::from([(
                    0,
                    json!({"id":"ancestor","has_children":true}),
                )])),
            },
        ),
        (
            child.clone(),
            super::CachedPage {
                total: 5000,
                rows: Rc::new(
                    (0..5000)
                        .map(|index| {
                            (
                                index,
                                json!({"id":format!("descendant-{index}"),"has_children":true}),
                            )
                        })
                        .collect(),
                ),
            },
        ),
    ];
    collection.trim_cached_rows(&child, 4900..5000);
    assert_eq!(collection.flatten_tree().len(), 11_000);
    collection.expanded.remove("ancestor");
    collection.visible_queries.clear();
    collection.visible_queries.insert(root.clone(), 0..1);
    collection.last_query = root.clone();
    collection.visible = 0..1;
    for index in 1..5000 {
        Rc::make_mut(&mut collection.pages[0].1.rows)
            .insert(index, json!({"id":format!("new-{index}")}));
    }
    collection.trim_cached_rows(&root, 4900..5000);
    assert_eq!(
        collection
            .pages
            .iter()
            .map(|(_, page)| page.rows.len())
            .sum::<usize>(),
        super::MAX_CACHED_ROWS
    );
    assert!(collection.pages[1].1.rows.is_empty());
    assert_eq!(collection.expanded.len(), 5000);
    assert_eq!(collection.tree_positions[&child].len(), 5000);
    assert_eq!(collection.flatten_tree().len(), 6000);
    collection.expanded.insert("ancestor".into());
    assert_eq!(collection.flatten_tree().len(), 11_000);
}
