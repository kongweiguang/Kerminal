//! 原生 list/table/tree 共用虚拟行绘制；插件只提供数据而不控制宿主样式。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    rc::Rc,
};

use gpui_kit::{
    AnyElement, App, ClickEvent, Context, EventEmitter, IntoElement, KeyDownEvent, Render,
    SharedString, Styled, Window,
    assets::IconName,
    base::{StyledExt as _, v_virtual_list},
    component::{
        ActiveTheme as _,
        button::ButtonVariants as _,
        menu::{ContextMenuExt as _, PopupMenuItem},
    },
    div,
    prelude::*,
    px, size,
};
use kerminal_plugin_protocol::{
    native::{CollectionSource, EmptyState, MenuItem, NativeNode, NativeNodeKind, TableColumn},
    runtime::{ViewFetchSort, ViewSortDirection},
};
use serde_json::Value;

use super::{
    renderer::{CollectionEvent, CollectionQuery, NativeCollectionConfig},
    state::PageRow,
    view::display_value,
};

const INITIAL_FETCH_ROWS: usize = 40;
const MAX_CACHED_ROWS: usize = 4096;

#[derive(Clone)]
struct CachedPage {
    total: usize,
    rows: Rc<BTreeMap<usize, Value>>,
}

#[derive(Clone)]
struct TreeSlot {
    query: CollectionQuery,
    index: usize,
    depth: usize,
    row: Option<Value>,
}

/// 每个集合保留自身滚动、选择与展开状态；分页权威状态仍由 ViewState 持有。
pub struct NativeCollection {
    config: NativeCollectionConfig,
    scroll: gpui_kit::base::VirtualListScrollHandle,
    selected: BTreeSet<String>,
    expanded: BTreeSet<String>,
    sort: Option<ViewFetchSort>,
    pages: Vec<(CollectionQuery, CachedPage)>,
    visible: Range<usize>,
    visible_queries: BTreeMap<CollectionQuery, Range<usize>>,
    tree_positions: BTreeMap<CollectionQuery, BTreeMap<usize, String>>,
    #[cfg(feature = "right-rail-acceptance")]
    rendered_visible_rows: Vec<(usize, String)>,
    /// 验收计数只在同一集合生命周期内累积；成功页会清除可见错误文本，
    /// 但这里的计数保留曾发生的请求、成功和失败，避免旧错误被成功结果掩盖。
    #[cfg(feature = "right-rail-acceptance")]
    acceptance_fetch_requests: u64,
    #[cfg(feature = "right-rail-acceptance")]
    acceptance_fetch_successes: u64,
    #[cfg(feature = "right-rail-acceptance")]
    acceptance_fetch_errors: u64,
    #[cfg(feature = "right-rail-acceptance")]
    acceptance_last_fetch: Option<(Range<usize>, CollectionQuery)>,
    last_query: CollectionQuery,
    bootstrap_pending: bool,
    error: Option<String>,
    row_sizes: Rc<Vec<gpui_kit::Size<gpui_kit::Pixels>>>,
}

impl EventEmitter<CollectionEvent> for NativeCollection {}

impl NativeCollection {
    /// 验收只驱动同一原生滚动句柄，不直接改可视范围或伪造分页事件。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn external_plugin_acceptance_scroll_to(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if index >= self.rows_for(&self.query()).0 {
            return false;
        }
        self.scroll
            .scroll_to_item(index, gpui_kit::ScrollStrategy::Top);
        cx.notify();
        true
    }

    /// 采样只返回真正构造过业务行的绘制回调记录，刚到达但尚未绘制的缓存不能冒充可见内容。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn external_plugin_acceptance_snapshot(&self) -> Value {
        let (total, _) = self.rows_for(&self.last_query);
        let visible_rows: Vec<_> = self
            .rendered_visible_rows
            .iter()
            .map(|(index, key)| serde_json::json!({"index":index,"key":key}))
            .collect();
        serde_json::json!({"total":total,"visible_start":self.visible.start,"visible_end":self.visible.end,
            "visible_rows":visible_rows,"cached_rows":self.pages.iter().map(|(_,page)|page.rows.len()).sum::<usize>(),
            "cached_queries":self.pages.len(),"error":self.error,
            "fetch_requests":self.acceptance_fetch_requests,"fetch_successes":self.acceptance_fetch_successes,"fetch_errors":self.acceptance_fetch_errors})
    }

    /// 构造时只冻结静态节点与初始数据；RPC 集合首屏请求延后到首帧后发出。
    pub fn new(
        config: NativeCollectionConfig,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self::from_config(config)
    }

    /// 纯构造保持初始状态可独立检验，窗口适配只负责把实体挂入 GPUI 生命周期。
    fn from_config(config: NativeCollectionConfig) -> Self {
        let is_rpc = collection_source(&config.node) == Some(CollectionSource::Rpc);
        Self {
            config,
            scroll: gpui_kit::base::VirtualListScrollHandle::new(),
            selected: BTreeSet::new(),
            expanded: BTreeSet::new(),
            sort: None,
            pages: Vec::new(),
            visible: 0..INITIAL_FETCH_ROWS,
            visible_queries: BTreeMap::new(),
            tree_positions: BTreeMap::new(),
            #[cfg(feature = "right-rail-acceptance")]
            rendered_visible_rows: Vec::new(),
            #[cfg(feature = "right-rail-acceptance")]
            acceptance_fetch_requests: 0,
            #[cfg(feature = "right-rail-acceptance")]
            acceptance_fetch_successes: 0,
            #[cfg(feature = "right-rail-acceptance")]
            acceptance_fetch_errors: 0,
            #[cfg(feature = "right-rail-acceptance")]
            acceptance_last_fetch: None,
            last_query: CollectionQuery::default(),
            bootstrap_pending: is_rpc,
            error: None,
            row_sizes: Rc::new(Vec::new()),
        }
    }

    /// 页回执只复制一次必要数据；渲染共享不可变快照，缓存淘汰不改变业务键选择和滚动坐标。
    pub fn apply_page(
        &mut self,
        query: &CollectionQuery,
        rows: &[PageRow],
        total: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_query(query) {
            return;
        }
        #[cfg(feature = "right-rail-acceptance")]
        {
            self.acceptance_fetch_successes = self.acceptance_fetch_successes.saturating_add(1);
        }
        if let Some(positions) = self.tree_positions.get_mut(query) {
            positions.retain(|index, _| *index < total);
            for row in rows {
                if positions.get(&row.index()).is_some_and(|old| {
                    row_key(row.value(), &self.config.node).as_ref() != Some(old)
                        || row.value().get("has_children").and_then(Value::as_bool) != Some(true)
                }) {
                    positions.remove(&row.index());
                }
            }
        }
        let page = if let Some((_, page)) = self.pages.iter_mut().find(|(key, _)| key == query) {
            page
        } else {
            self.pages.push((
                query.clone(),
                CachedPage {
                    total,
                    rows: Rc::new(BTreeMap::new()),
                },
            ));
            &mut self.pages.last_mut().expect("page was inserted").1
        };
        page.total = total;
        let cached = Rc::make_mut(&mut page.rows);
        cached.retain(|index, _| *index < total);
        for row in rows {
            if row.index() < total {
                cached.insert(row.index(), row.value().clone());
            }
        }
        let incoming = rows.first().map_or(0..0, |first| {
            first.index()
                ..rows
                    .last()
                    .map_or(first.index(), PageRow::index)
                    .saturating_add(1)
        });
        self.trim_cached_rows(query, incoming);
        self.error = None;
        cx.notify();
    }

    /// 展开结构只保留坐标和业务键，完整 JSON 仅保护本帧全部可见查询和刚到达页。
    fn trim_cached_rows(&mut self, incoming_query: &CollectionQuery, incoming: Range<usize>) {
        let count = self
            .pages
            .iter()
            .map(|(_, page)| page.rows.len())
            .sum::<usize>();
        if count <= MAX_CACHED_ROWS {
            return;
        }
        for (query, page) in &self.pages {
            let positions = self.tree_positions.entry(query.clone()).or_default();
            positions.retain(|_, key| self.expanded.contains(key));
            for (index, row) in page.rows.iter() {
                if row.get("has_children").and_then(Value::as_bool) == Some(true)
                    && let Some(key) = row_key(row, &self.config.node)
                    && self.expanded.contains(&key)
                {
                    positions.insert(*index, key);
                }
            }
        }
        let mut candidates = Vec::new();
        for (page_index, (query, page)) in self.pages.iter().enumerate() {
            for index in page.rows.keys() {
                if (query == incoming_query && incoming.contains(index))
                    || (query == &self.last_query && self.visible.contains(index))
                    || self
                        .visible_queries
                        .get(query)
                        .is_some_and(|range| range.contains(index))
                {
                    continue;
                }
                let anchor = if query == incoming_query {
                    &incoming
                } else {
                    &self.visible
                };
                let distance = if *index < anchor.start {
                    anchor.start - *index
                } else {
                    index.saturating_sub(anchor.end.saturating_sub(1))
                };
                candidates.push((query != incoming_query, distance, page_index, *index));
            }
        }
        candidates.sort_unstable_by(|left, right| right.cmp(left));
        let mut removed = BTreeMap::<usize, BTreeSet<usize>>::new();
        for (_, _, page, index) in candidates.into_iter().take(count - MAX_CACHED_ROWS) {
            removed.entry(page).or_default().insert(index);
        }
        for (page, indices) in removed {
            Rc::make_mut(&mut self.pages[page].1.rows).retain(|index, _| !indices.contains(index));
        }
    }

    /// 固定行高只有数量或字体尺寸改变才重新分配，普通帧复用同一份十万行尺寸快照。
    fn row_sizes(
        &mut self,
        count: usize,
        row_height: gpui_kit::Pixels,
    ) -> Rc<Vec<gpui_kit::Size<gpui_kit::Pixels>>> {
        if self.row_sizes.len() != count
            || self
                .row_sizes
                .first()
                .is_some_and(|size| size.height != row_height)
        {
            self.row_sizes = Rc::new(vec![size(px(1.), row_height); count]);
        }
        Rc::clone(&self.row_sizes)
    }

    /// 内联列表来自 ViewState 的合并快照；直接替换数据而不改写分页缓存。
    pub fn apply_data(&mut self, data: Value, cx: &mut Context<Self>) {
        self.config.data = data;
        if collection_source(&self.config.node) != Some(CollectionSource::Rpc) {
            let valid_keys = inline_rows(&self.config.data, &self.config.node)
                .iter()
                .filter_map(|row| row_key(row, &self.config.node))
                .collect::<BTreeSet<_>>();
            self.selected.retain(|key| valid_keys.contains(key));
        }
        cx.notify();
    }

    /// 分页失败保留先前行；错误由宿主传入已本地化文案并提供原位重试。
    pub fn apply_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        #[cfg(feature = "right-rail-acceptance")]
        {
            self.acceptance_fetch_errors = self.acceptance_fetch_errors.saturating_add(1);
        }
        self.error = Some(message.into());
        cx.notify();
    }

    /// 从协议节点与已合并数据构造当前查询，树父键与表格排序共同区分页缓存。
    pub fn query(&self) -> CollectionQuery {
        CollectionQuery {
            parent_key: None,
            sort: self.sort.clone(),
        }
    }

    /// 树只接受已展开父项的结果；其他集合必须与当前排序查询完全一致。
    fn accepts_query(&self, query: &CollectionQuery) -> bool {
        if query.sort != self.sort {
            return false;
        }
        match &self.config.node.kind {
            NativeNodeKind::Tree { .. } => query
                .parent_key
                .as_ref()
                .is_none_or(|parent| self.expanded.contains(parent)),
            _ => query.parent_key.is_none(),
        }
    }

    /// 选择由稳定业务键保存，排序、插页和树折叠都不会把索引误当身份。
    pub fn selected_keys(&self) -> &BTreeSet<String> {
        &self.selected
    }

    /// 逐父查询累积本帧实际可视范围；last_query 仅用于原有重试入口，不再代表完整视口。
    fn schedule_visible(
        &mut self,
        range: Range<usize>,
        query: CollectionQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.visible = range.clone();
        self.last_query = query.clone();
        // 同一范围/查询的连续重绘不应膨胀请求计数；只有范围或查询真正变化时才记录一次，
        // 与布局层按 ViewState 去重后的分页请求节奏保持同向。
        #[cfg(feature = "right-rail-acceptance")]
        {
            let request = (range.clone(), query.clone());
            if self.acceptance_last_fetch.as_ref() != Some(&request) {
                self.acceptance_fetch_requests = self.acceptance_fetch_requests.saturating_add(1);
                self.acceptance_last_fetch = Some(request);
            }
        }
        self.visible_queries.insert(query.clone(), range.clone());
        let node = self.config.node_key.clone();
        cx.on_next_frame(window, move |_, _, cx| {
            cx.emit(CollectionEvent::Visible { node, range, query });
        });
    }

    /// 用户点击行后更新稳定键选择；双击才触发声明的打开动作。
    fn activate_or_select(&mut self, item_key: String, event: &ClickEvent, cx: &mut Context<Self>) {
        self.selected.clear();
        self.selected.insert(item_key.clone());
        if event.click_count() > 1
            && let Some(action) = activation_action(&self.config.node)
        {
            cx.emit(CollectionEvent::Activate {
                node: self.config.node_key.clone(),
                action: action.to_owned(),
                item_key,
            });
        }
        cx.notify();
    }

    /// 键盘路径让选择与激活动作共用同一稳定键，不依赖鼠标双击才能完成主操作。
    fn activate_or_select_key(&mut self, item_key: String, activate: bool, cx: &mut Context<Self>) {
        self.selected.clear();
        self.selected.insert(item_key.clone());
        if activate && let Some(action) = activation_action(&self.config.node) {
            cx.emit(CollectionEvent::Activate {
                node: self.config.node_key.clone(),
                action: action.to_owned(),
                item_key,
            });
        }
        cx.notify();
    }

    /// 展开状态在实体本地保留，子节点拉取通过独立 parent_key 查询交给宿主。
    fn toggle_tree_parent(
        &mut self,
        parent_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expanded = if self.expanded.remove(&parent_key) {
            false
        } else {
            self.expanded.insert(parent_key.clone());
            true
        };
        cx.emit(CollectionEvent::Toggle {
            node: self.config.node_key.clone(),
            parent_key: parent_key.clone(),
            expanded,
        });
        if expanded && collection_source(&self.config.node) == Some(CollectionSource::Rpc) {
            let query = CollectionQuery {
                parent_key: Some(parent_key),
                sort: self.sort.clone(),
            };
            self.last_query = query.clone();
            self.visible = 0..INITIAL_FETCH_ROWS;
            let node = self.config.node_key.clone();
            cx.on_next_frame(window, move |_, _, cx| {
                cx.emit(CollectionEvent::Visible {
                    node,
                    range: 0..INITIAL_FETCH_ROWS,
                    query,
                });
            });
        }
        cx.notify();
    }

    /// 点击可排序列切换方向并向宿主请求新快照，旧查询行继续可见。
    fn toggle_sort(&mut self, column: &TableColumn, cx: &mut Context<Self>) {
        if column.sortable == Some(false) {
            return;
        }
        let direction = match self.sort.as_ref() {
            Some(sort) if sort.column == column.key && sort.direction == ViewSortDirection::Asc => {
                ViewSortDirection::Desc
            }
            _ => ViewSortDirection::Asc,
        };
        let sort = ViewFetchSort {
            column: column.key.clone(),
            direction,
        };
        self.sort = Some(sort.clone());
        self.error = None;
        if collection_source(&self.config.node) == Some(CollectionSource::Rpc) {
            cx.emit(CollectionEvent::Sort {
                node: self.config.node_key.clone(),
                sort,
            });
        }
        cx.notify();
    }

    /// RPC 只克隆快照句柄，不在每帧复制分页 JSON；内联列表仍按有界静态数据排序。
    fn rows_for(&self, query: &CollectionQuery) -> (usize, Rc<BTreeMap<usize, Value>>) {
        if collection_source(&self.config.node) == Some(CollectionSource::Rpc) {
            if let Some((_, page)) = self.pages.iter().find(|(key, _)| key == query) {
                return (page.total, page.rows.clone());
            }
            return self
                .pages
                .iter()
                .rev()
                .find(|(cached_query, _)| cached_query.parent_key == query.parent_key)
                .map(|(_, page)| (page.total, page.rows.clone()))
                .unwrap_or((0, Rc::new(BTreeMap::new())));
        }
        let mut rows = inline_rows(&self.config.data, &self.config.node);
        if let Some(sort) = self.sort.as_ref() {
            rows.sort_by(|left, right| {
                compare_values(left.get(&sort.column), right.get(&sort.column))
            });
            if sort.direction == ViewSortDirection::Desc {
                rows.reverse();
            }
        }
        (rows.len(), Rc::new(rows.into_iter().enumerate().collect()))
    }

    /// 显示字段只按协议声明的对象键读取，不把插件自定义 HTML 当成界面内容。
    fn display_field(&self, row: &Value, field: &str) -> SharedString {
        row.get(field).map(display_value).unwrap_or_default().into()
    }

    /// 完整空态优先使用插件声明文案，缺少声明时按节点种类选择宿主双语默认值。
    fn empty_state(&self, empty: Option<&EmptyState>, cx: &App) -> AnyElement {
        let title = if let Some(empty) = empty {
            self.localized(&empty.title)
        } else {
            match &self.config.node.kind {
                NativeNodeKind::List { .. } => {
                    crate::locale::text("没有项目", "No items").to_string()
                }
                NativeNodeKind::Table { .. } => {
                    crate::locale::text("没有记录", "No records").to_string()
                }
                NativeNodeKind::Tree { .. } => {
                    crate::locale::text("没有子项", "No items").to_string()
                }
                _ => crate::locale::text("没有数据", "No data").to_string(),
            }
        };
        div()
            .w_full()
            .px_3()
            .py_5()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(title)
            .into_any_element()
    }

    /// 翻译模板按 `%key%` 查找当前集合冻结的语言映射，其他文本保持原样。
    fn localized(&self, text: &str) -> String {
        text.strip_prefix('%')
            .and_then(|text| text.strip_suffix('%'))
            .and_then(|key| self.config.translations.get(key))
            .cloned()
            .unwrap_or_else(|| text.to_owned())
    }

    /// 列表行的稳定菜单动作通过同一 Activate 事件出口，不暴露运行时句柄给插件视图。
    fn row_menu(&self) -> Vec<MenuItem> {
        match &self.config.node.kind {
            NativeNodeKind::List { menu, .. }
            | NativeNodeKind::Table { menu, .. }
            | NativeNodeKind::Tree { menu, .. } => menu.clone().unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// 树展开父键使用节点声明的稳定业务键，缺失标记则视作叶子。
    fn tree_has_children(&self, row: &Value) -> bool {
        row.get("has_children").and_then(Value::as_bool) == Some(true)
    }

    /// 展开树递归生成虚拟槽位；未加载位置仍按真实总数占位，供可见回调换算父查询范围。
    fn flatten_tree(&self) -> Vec<TreeSlot> {
        let mut result = Vec::new();
        self.append_tree_slots(None, 0, &mut result);
        result
    }

    /// 离屏展开父行即使已淘汰 JSON，也用轻量坐标继续保留子树槽位，避免滚动长度跳变。
    fn append_tree_slots(
        &self,
        parent_key: Option<String>,
        depth: usize,
        result: &mut Vec<TreeSlot>,
    ) {
        let query = CollectionQuery {
            parent_key: parent_key.clone(),
            sort: self.sort.clone(),
        };
        let (total, rows) = self.rows_for(&query);
        let positions = self
            .pages
            .iter()
            .find(|(cached, _)| cached == &query)
            .or_else(|| {
                self.pages
                    .iter()
                    .rev()
                    .find(|(cached, _)| cached.parent_key == query.parent_key)
            })
            .and_then(|(cached, _)| self.tree_positions.get(cached));
        for index in 0..total {
            let row = rows.get(&index).cloned();
            let item_key = row
                .as_ref()
                .and_then(|row| row_key(row, &self.config.node))
                .or_else(|| {
                    positions
                        .and_then(|positions| positions.get(&index))
                        .cloned()
                });
            result.push(TreeSlot {
                query: query.clone(),
                index,
                depth,
                row: row.clone(),
            });
            if let Some(item_key) = item_key
                && (row.as_ref().is_some_and(|row| self.tree_has_children(row))
                    || positions.is_some_and(|positions| positions.contains_key(&index)))
                && self.expanded.contains(&item_key)
            {
                self.append_tree_slots(Some(item_key), depth + 1, result);
            }
        }
    }
}

impl Render for NativeCollection {
    /// 集合主体共享行数据与固定尺寸快照，只有可视范围生成元素；RPC 继续延至下一帧分发。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let node = self.config.node.clone();
        let entity = cx.entity().clone();
        let (total, rows) = self.rows_for(&self.query());
        let tree_slots =
            matches!(&node.kind, NativeNodeKind::Tree { .. }).then(|| self.flatten_tree());
        let count = tree_slots.as_ref().map_or(total, Vec::len);
        let row_height = window.rem_size() * 2.5;
        let row_sizes = self.row_sizes(count, row_height);
        let title = match &node.kind {
            NativeNodeKind::List { title, .. } | NativeNodeKind::Tree { title, .. } => {
                Some(self.localized(title))
            }
            _ => None,
        };
        let columns = match &node.kind {
            NativeNodeKind::Table { columns, .. } => Some(
                columns
                    .iter()
                    .map(|column| (column.clone(), self.localized(&column.title)))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let empty = match &node.kind {
            NativeNodeKind::List { empty, .. }
            | NativeNodeKind::Table { empty, .. }
            | NativeNodeKind::Tree { empty, .. } => empty.as_ref().cloned(),
            _ => None,
        };
        let query = self.query();
        let source_rpc = collection_source(&node) == Some(CollectionSource::Rpc);
        let bootstrap = self.bootstrap_pending && source_rpc;
        self.bootstrap_pending = false;
        let theme_background = cx.theme().background;
        let theme_foreground = cx.theme().foreground;
        let theme_muted = cx.theme().muted_foreground;
        let mut body = div()
            .id(format!("native-collection-{}", self.config.node_key))
            .w_full()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme_background)
            .text_color(theme_foreground);

        if let Some(title) = title {
            body = body.child(div().px_3().py_2().font_medium().child(title));
        }
        if let Some(columns) = columns.as_ref() {
            let header_entity = entity.clone();
            let active_sort = self.sort.clone();
            let node_id = node.id.clone().unwrap_or_else(|| "table".to_owned());
            body = body.child(
                div()
                    .flex()
                    .px_3()
                    .py_2()
                    .gap_3()
                    .children(columns.iter().map(move |(column, title)| {
                        let column_for_click = column.clone();
                        let column_for_key = column.clone();
                        let header_entity = header_entity.clone();
                        let key_entity = header_entity.clone();
                        let indicator = active_sort
                            .as_ref()
                            .filter(|sort| sort.column == column.key)
                            .map(|sort| match sort.direction {
                                ViewSortDirection::Asc => " ↑",
                                ViewSortDirection::Desc => " ↓",
                            })
                            .unwrap_or("");
                        div()
                            .id(format!("native-column-{node_id}-{}", column.key))
                            .flex_1()
                            .text_sm()
                            .text_color(theme_muted)
                            .child(format!("{title}{indicator}"))
                            .when(column.sortable != Some(false), |this| {
                                this.cursor_pointer()
                                    .role(gpui_kit::Role::Button)
                                    .tab_index(0)
                                    .aria_label(format!("按 {title} 排序"))
                                    .aria_keyshortcuts("Enter Space")
                                    .on_click(move |_, _, cx| {
                                        header_entity.update(cx, |collection, cx| {
                                            collection.toggle_sort(&column_for_click, cx);
                                        });
                                    })
                                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                                        if matches!(
                                            event.keystroke.key.to_ascii_lowercase().as_str(),
                                            "enter" | "space" | " "
                                        ) {
                                            key_entity.update(cx, |collection, cx| {
                                                collection.toggle_sort(&column_for_key, cx);
                                            });
                                            cx.stop_propagation();
                                        }
                                    })
                            })
                    })),
            );
        }

        if count == 0 && !bootstrap {
            body = body.child(self.empty_state(empty.as_ref(), cx));
        } else {
            let callback_node = node.clone();
            let callback_query = query.clone();
            let callback_rows = rows.clone();
            let callback_slots = tree_slots.clone();
            let list = v_virtual_list(
                entity.clone(),
                format!("native-collection-list-{}", self.config.node_key),
                row_sizes,
                move |this, range, window, cx| {
                    // 测量回调和绘制回调分别重建各自完整的范围，避免把上帧不可见行永久钉住。
                    this.visible_queries.clear();
                    #[cfg(feature = "right-rail-acceptance")]
                    this.rendered_visible_rows.clear();
                    if let Some(slots) = callback_slots.as_ref() {
                        return render_tree_range(
                            this,
                            slots,
                            range,
                            &callback_node,
                            theme_muted,
                            window,
                            cx,
                        );
                    }
                    if source_rpc {
                        this.schedule_visible(range.clone(), callback_query.clone(), window, cx);
                    }
                    render_flat_range(this, &callback_node, &callback_rows, range, theme_muted, cx)
                },
            )
            .track_scroll(&self.scroll);
            body = body.child(list.flex_1().min_h_0());
        }

        if let Some(error) = self.error.clone() {
            let retry_entity = entity.clone();
            let range = self.visible.clone();
            let query = self.last_query.clone();
            body = body.child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error)
                    .child(
                        gpui_kit::component::button::Button::new("native-collection-retry")
                            .ghost()
                            .label(crate::locale::text("重试", "Retry"))
                            .on_click(move |_, window, app| {
                                retry_entity.update(app, |collection, cx| {
                                    collection.error = None;
                                    let node = collection.config.node_key.clone();
                                    let range = range.clone();
                                    let query = query.clone();
                                    cx.on_next_frame(window, move |_, _, cx| {
                                        cx.emit(CollectionEvent::Visible { node, range, query });
                                    });
                                    cx.notify();
                                });
                            }),
                    ),
            );
        }

        if bootstrap {
            let query = self.query();
            cx.on_next_frame(window, move |this, window, cx| {
                this.schedule_visible(0..INITIAL_FETCH_ROWS, query, window, cx);
            });
        }
        body
    }
}

/// 列表只按视口范围构造业务行；验收记录在构造处更新，不能把仅缓存到达误当作已绘制。
fn render_flat_range(
    this: &mut NativeCollection,
    node: &NativeNode,
    rows: &BTreeMap<usize, Value>,
    range: Range<usize>,
    muted: gpui_kit::Hsla,
    cx: &mut Context<NativeCollection>,
) -> Vec<AnyElement> {
    range
        .map(|index| {
            let Some(row) = rows.get(&index) else {
                return div().h_10().into_any_element();
            };
            #[cfg(feature = "right-rail-acceptance")]
            if let Some(key) = row_key(row, node) {
                this.rendered_visible_rows.push((index, key));
            }
            render_row(this, node, row, 0, muted, cx)
        })
        .collect()
}

/// 树完整收集所有可见父查询范围，并只在真正构造业务行时记录验收键。
fn render_tree_range(
    this: &mut NativeCollection,
    slots: &[TreeSlot],
    range: Range<usize>,
    node: &NativeNode,
    muted: gpui_kit::Hsla,
    window: &mut Window,
    cx: &mut Context<NativeCollection>,
) -> Vec<AnyElement> {
    let mut visible_queries = Vec::<(CollectionQuery, Range<usize>)>::new();
    let mut elements = Vec::with_capacity(range.len());
    for index in range {
        let Some(slot) = slots.get(index) else {
            elements.push(div().h_10().into_any_element());
            continue;
        };
        if let Some((_, visible_range)) = visible_queries
            .iter_mut()
            .find(|(query, _)| query == &slot.query)
        {
            visible_range.start = visible_range.start.min(slot.index);
            visible_range.end = visible_range.end.max(slot.index + 1);
        } else {
            visible_queries.push((slot.query.clone(), slot.index..slot.index + 1));
        }
        if let Some(row) = &slot.row {
            #[cfg(feature = "right-rail-acceptance")]
            if let Some(key) = row_key(row, node) {
                this.rendered_visible_rows.push((index, key));
            }
            elements.push(render_row(this, node, row, slot.depth, muted, cx));
        } else {
            elements.push(div().h_10().into_any_element());
        }
    }
    for (query, range) in visible_queries {
        this.schedule_visible(range, query, window, cx);
    }
    elements
}

/// 单行交互统一处理选择、双击激活、树展开与协议菜单。
fn render_row(
    this: &mut NativeCollection,
    node: &NativeNode,
    row: &Value,
    depth: usize,
    muted: gpui_kit::Hsla,
    cx: &mut Context<NativeCollection>,
) -> AnyElement {
    let item_key = row_key(row, node).unwrap_or_default();
    let menu_items = this.row_menu();
    let key_for_click = item_key.clone();
    let key_for_keyboard = item_key.clone();
    let entity = cx.entity().clone();
    let keyboard_entity = entity.clone();
    let selected = this.selected.contains(&item_key);
    let accessible_label = row_accessible_label(row, node);
    let tree_row = matches!(&node.kind, NativeNodeKind::Tree { .. });
    let has_children = tree_row && this.tree_has_children(row);
    let mut element = div()
        .id(format!(
            "native-row-{}-{item_key}",
            node.id.as_deref().unwrap_or("collection")
        ))
        .h_10()
        .w_full()
        .role(gpui_kit::Role::Button)
        .tab_index(0)
        .aria_label(if selected {
            format!("{}，已选择", accessible_label)
        } else {
            accessible_label
        })
        .aria_keyshortcuts("Enter Space")
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .when(selected, |this| this.bg(cx.theme().secondary))
        .when(!selected, |this| {
            this.hover(|style| style.bg(cx.theme().muted))
        })
        .on_click(move |event, _, cx| {
            entity.update(cx, |collection, cx| {
                collection.activate_or_select(key_for_click.clone(), event, cx);
            });
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            let key = event.keystroke.key.to_ascii_lowercase();
            match key.as_str() {
                "enter" => {
                    keyboard_entity.update(cx, |collection, cx| {
                        collection.activate_or_select_key(key_for_keyboard.clone(), true, cx);
                    });
                    cx.stop_propagation();
                }
                "space" | " " => {
                    keyboard_entity.update(cx, |collection, cx| {
                        collection.activate_or_select_key(key_for_keyboard.clone(), false, cx);
                    });
                    cx.stop_propagation();
                }
                "arrowright" if has_children => {
                    keyboard_entity.update(cx, |collection, cx| {
                        if !collection.expanded.contains(&key_for_keyboard) {
                            collection.toggle_tree_parent(key_for_keyboard.clone(), window, cx);
                        }
                    });
                    cx.stop_propagation();
                }
                "arrowleft" if has_children => {
                    keyboard_entity.update(cx, |collection, cx| {
                        if collection.expanded.contains(&key_for_keyboard) {
                            collection.toggle_tree_parent(key_for_keyboard.clone(), window, cx);
                        }
                    });
                    cx.stop_propagation();
                }
                _ => {}
            }
        });
    if let NativeNodeKind::Tree { .. } = &node.kind {
        for _ in 0..depth {
            element = element.child(div().w_4().flex_shrink_0());
        }
        let can_expand = has_children;
        let expanded = this.expanded.contains(&item_key);
        if can_expand {
            let toggle_entity = cx.entity().clone();
            let keyboard_toggle_entity = toggle_entity.clone();
            let key = item_key.clone();
            let keyboard_key = item_key.clone();
            element = element.child(
                div()
                    .id(format!("native-tree-toggle-{item_key}"))
                    .w_5()
                    .text_sm()
                    .text_color(muted)
                    .role(gpui_kit::Role::Button)
                    .tab_index(0)
                    .aria_label(crate::locale::text(
                        if expanded {
                            "收起子项"
                        } else {
                            "展开子项"
                        },
                        if expanded {
                            "Collapse items"
                        } else {
                            "Expand items"
                        },
                    ))
                    .aria_keyshortcuts("Enter Space")
                    .child(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        toggle_entity.update(cx, |collection, cx| {
                            collection.toggle_tree_parent(key.clone(), window, cx);
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        if matches!(
                            event.keystroke.key.to_ascii_lowercase().as_str(),
                            "enter" | "space" | " "
                        ) {
                            cx.stop_propagation();
                            keyboard_toggle_entity.update(cx, |collection, cx| {
                                collection.toggle_tree_parent(keyboard_key.clone(), window, cx);
                            });
                        }
                    }),
            );
        } else {
            element = element.child(div().w_5());
        }
    }
    match &node.kind {
        NativeNodeKind::List {
            title, subtitle, ..
        } => {
            element = element.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .child(this.display_field(row, title)),
            );
            if let Some(subtitle) = subtitle {
                element = element.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(muted)
                        .text_ellipsis()
                        .child(this.display_field(row, subtitle)),
                );
            }
        }
        NativeNodeKind::Tree { title, .. } => {
            element = element.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .child(this.display_field(row, title)),
            );
        }
        NativeNodeKind::Table { columns, .. } => {
            element = element.children(columns.iter().map(|column| {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .child(this.display_field(row, &column.key))
            }));
        }
        _ => {}
    }
    if menu_items.is_empty() {
        element.into_any_element()
    } else {
        let menu_entity = cx.entity().clone();
        let node_key = this.config.node_key.clone();
        let translations = this.config.translations.clone();
        element
            .context_menu(move |mut menu, _, _| {
                for item in &menu_items {
                    let action = item.action.clone();
                    let item_key = item_key.clone();
                    let menu_entity = menu_entity.clone();
                    let node_key = node_key.clone();
                    let label = localize(&translations, &item.label);
                    let mut entry = PopupMenuItem::new(label).on_click(move |_, _, app| {
                        menu_entity.update(app, |_, cx| {
                            cx.emit(CollectionEvent::Activate {
                                node: node_key.clone(),
                                action: action.clone(),
                                item_key: item_key.clone(),
                            });
                        });
                    });
                    if item.danger == Some(true) {
                        entry = entry.danger();
                    }
                    menu = menu.item(entry);
                }
                menu
            })
            .into_any_element()
    }
}

/// 绑定数据只接受 JSON 对象键链；数组索引和表达式仍由协议校验层拒绝。
fn inline_rows(data: &Value, node: &NativeNode) -> Vec<Value> {
    let path = match &node.kind {
        NativeNodeKind::List { items, .. }
        | NativeNodeKind::Table { items, .. }
        | NativeNodeKind::Tree { items, .. } => items.as_deref(),
        _ => None,
    };
    path.and_then(|path| resolve_binding(data, path))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 不重排插件自有行对象，键路径用于用户选择和动作身份。
fn row_key(row: &Value, node: &NativeNode) -> Option<String> {
    let key = match &node.kind {
        NativeNodeKind::List { key, .. }
        | NativeNodeKind::Table { key, .. }
        | NativeNodeKind::Tree { key, .. } => key,
        _ => return None,
    };
    row.get(key)
        .map(display_value)
        .filter(|key| !key.is_empty())
}

/// 可访问名称优先使用用户可读字段，只有表格才回退到第一列，稳定键不暴露为主文案。
fn row_accessible_label(row: &Value, node: &NativeNode) -> String {
    let field = match &node.kind {
        NativeNodeKind::List { title, .. } | NativeNodeKind::Tree { title, .. } => Some(title),
        NativeNodeKind::Table { columns, .. } => columns.first().map(|column| &column.key),
        _ => None,
    };
    field
        .and_then(|field| row.get(field))
        .map(display_value)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| crate::locale::text("集合行", "Collection row").to_string())
}

/// 激活动作按节点变体读取，避免用平铺字段猜测协议动作。
fn activation_action(node: &NativeNode) -> Option<&str> {
    match &node.kind {
        NativeNodeKind::List { on_activate, .. }
        | NativeNodeKind::Table { on_activate, .. }
        | NativeNodeKind::Tree { on_activate, .. } => on_activate.as_deref(),
        _ => None,
    }
}

/// 数据绑定从可选 `$` 后沿对象键读取，不执行插件表达式。
fn resolve_binding<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.strip_prefix('$').unwrap_or(path);
    path.split('.').try_fold(data, |value, key| value.get(key))
}

/// 只从集合节点的协议变体读取 source，避免把普通节点误判为 RPC 数据。
fn collection_source(node: &NativeNode) -> Option<CollectionSource> {
    match &node.kind {
        NativeNodeKind::List { source, .. }
        | NativeNodeKind::Table { source, .. }
        | NativeNodeKind::Tree { source, .. } => *source,
        _ => None,
    }
}

/// 在延迟构建上下文菜单时复用冻结的翻译映射，不捕获已结束的绘制借用。
fn localize(translations: &BTreeMap<String, String>, text: &str) -> String {
    text.strip_prefix('%')
        .and_then(|text| text.strip_suffix('%'))
        .and_then(|key| translations.get(key))
        .cloned()
        .unwrap_or_else(|| text.to_owned())
}

/// 表格排序只比较标量值，复合 JSON 使用规范化文本维持确定顺序。
fn compare_values(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left.cmp(right),
        (Some(left), Some(right)) => display_value(left).cmp(&display_value(right)),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_collections.rs"]
mod tests;
