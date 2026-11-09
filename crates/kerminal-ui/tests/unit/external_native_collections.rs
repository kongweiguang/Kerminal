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
