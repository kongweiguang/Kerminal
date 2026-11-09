//! 原生节点布局、交互投影和集合分页路由。
//!
//! @author kongweiguang

use super::{
    collections::NativeCollection,
    inputs::FieldControl,
    renderer::{
        CollectionEvent, CollectionQuery, NativeCollectionConfig, NativeRenderer, OpenStatus,
        collection_cache_key,
    },
    runtime::RuntimeRequest,
    state::{BindingPath, PageTicket},
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, IntoElement, PathPromptOptions, Styled, Window,
    base::{Disableable as _, StyledExt as _},
    component::{
        ActiveTheme as _, Icon,
        button::{Button as KitButton, ButtonVariants as _},
        checkbox::Checkbox as KitCheckbox,
        input::{Editor, Input, Textarea},
        menu::{DropdownMenu as _, PopupMenuItem},
        progress::Progress as KitProgress,
        resizable::{ResizableState, h_resizable, resizable_panel},
        select::Select,
        switch::Switch as KitSwitch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::external_contributions::PreparedPlugin;
use kerminal_plugin_protocol::{
    native::{ButtonVariant, Gap, NativeNode, NativeNodeKind, PickerMode, Tone},
    runtime::ViewFetchResult,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

impl NativeRenderer {
    /// 固定节点实体与静态视图同生共死；集合事件只传递业务意图。
    pub(super) fn prepare_collections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.projection.view().root.clone();
        self.prepare_node_collections(&root, "root".into(), window, cx);
    }

    /// RPC 使用原始节点 ID，内联集合可使用静态树路径作为本地实体键。
    fn prepare_node_collections(
        &mut self,
        node: &NativeNode,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use NativeNodeKind::*;
        match &node.kind {
            Split { children, .. } => {
                self.split_states
                    .insert(path.clone(), cx.new(|_| ResizableState::default()));
                for (index, child) in children.iter().enumerate() {
                    self.prepare_node_collections(child, format!("{path}.{index}"), window, cx);
                }
            }
            Stack { children, .. }
            | Row { children, .. }
            | Section { children, .. }
            | Toolbar { children, .. } => {
                for (index, child) in children.iter().enumerate() {
                    self.prepare_node_collections(child, format!("{path}.{index}"), window, cx);
                }
            }
            Scroll { child, .. } => {
                self.prepare_node_collections(child, format!("{path}.0"), window, cx)
            }
            Tabs { tabs, .. } => {
                for (index, tab) in tabs.iter().enumerate() {
                    self.prepare_node_collections(
                        &tab.content,
                        format!("{path}.{index}"),
                        window,
                        cx,
                    );
                }
            }
            List { .. } | Table { .. } | Tree { .. } => {
                let key = node.id.clone().unwrap_or(path);
                let translations = self
                    .prepared
                    .localizations
                    .get(&self.locale)
                    .cloned()
                    .or_else(|| self.prepared.localizations.get("en").cloned())
                    .unwrap_or_default();
                let config = NativeCollectionConfig {
                    node_key: key.clone(),
                    node: node.clone(),
                    data: self.projection.state().data().clone(),
                    translations,
                };
                let collection = cx.new(|cx| NativeCollection::new(config, window, cx));
                let subscription =
                    cx.subscribe(&collection, |renderer, _, event: &CollectionEvent, cx| {
                        renderer.collection_event(event.clone(), cx)
                    });
                self.subscriptions.push(subscription);
                self.collections.insert(key, collection);
            }
            _ => {}
        }
    }

    /// 内联集合从同一份草稿快照更新，分页集合不会因此丢失滚动或已展示行。
    pub(super) fn sync_collections(&mut self, cx: &mut Context<Self>) {
        let data = self.projection.state().data().clone();
        for collection in self.collections.values() {
            collection.update(cx, |collection, cx| collection.apply_data(data.clone(), cx));
        }
    }

    /// 合并可见范围并让 ViewState 去重，避免布局重绘重复发起分页 RPC。
    fn collection_event(&mut self, event: CollectionEvent, cx: &mut Context<Self>) {
        match event {
            CollectionEvent::Visible { node, range, query } => {
                self.queue_collection(node, range, query, cx)
            }
            CollectionEvent::Sort { node, sort } => self.queue_collection(
                node,
                0..40,
                CollectionQuery {
                    parent_key: None,
                    sort: Some(sort),
                },
                cx,
            ),
            CollectionEvent::Toggle { .. } => {}
            CollectionEvent::Activate {
                node,
                action,
                item_key,
            } => self.action(node, action, Some(item_key), cx),
        }
    }

    /// 查询键包含父节点和排序，实际 RPC 始终携带协议声明的原始节点 ID。
    fn queue_collection(
        &mut self,
        node: String,
        range: std::ops::Range<usize>,
        query: CollectionQuery,
        cx: &mut Context<Self>,
    ) {
        let key = collection_cache_key(&node, &query);
        if self.projection.state_mut().queue_page(&key, range).is_err() {
            return;
        }
        self.collection_queries.insert(key.clone(), (node, query));
        self.flush_collection(&key, cx);
    }

    /// 首屏在视图打开期间入队；打开回执后统一发送，不能丢失第一帧请求。
    pub(super) fn flush_collections(&mut self, cx: &mut Context<Self>) {
        let keys = self.collection_queries.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.flush_collection(&key, cx);
        }
    }

    /// RPC 固定发起时的实例和进程代次，迟到回复不能覆盖重新打开后的集合。
    fn flush_collection(&mut self, key: &str, cx: &mut Context<Self>) {
        if !matches!(self.status, OpenStatus::Ready) {
            return;
        }
        let Some((node, query)) = self.collection_queries.get(key).cloned() else {
            return;
        };
        let Ok(tickets) = self.projection.state_mut().take_page_requests(key) else {
            return;
        };
        for ticket in tickets {
            let range = ticket.range();
            let request = RuntimeRequest {
                identity: self.identity.clone(),
                process_generation: self.process_generation,
                method: "view.fetch".into(),
                params: json!({"node":node,"range":[range.start,range.end],"parent_key":query.parent_key,"sort":query.sort}),
                deadline: Instant::now() + Duration::from_secs(30),
                cancellation: self.operation_cancellation.child_token(),
            };
            let runtime = self.runtime.clone();
            let executor = self.executor.clone();
            let expected = self.process_generation;
            let node = node.clone();
            let query = query.clone();
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run_cancellable(request.cancellation.clone(), move |token| async move {
                        runtime
                            .request(RuntimeRequest {
                                cancellation: token,
                                ..request
                            })
                            .await
                    })
                    .await;
                let _ = this.update(cx, |renderer, cx| {
                    renderer.finish_collection(node, query, ticket, expected, result, cx)
                });
            })
            .detach();
        }
    }

    /// 收齐当前范围后才替换显示行；错误保留旧页并暴露重试入口。
    fn finish_collection(
        &mut self,
        node: String,
        query: CollectionQuery,
        ticket: PageTicket,
        expected: Option<u64>,
        result: kerminal_application::AppResult<super::runtime::RuntimeReply>,
        cx: &mut Context<Self>,
    ) {
        if self.process_generation != expected {
            return;
        }
        let result = result.and_then(|reply| {
            if Some(reply.process_generation) != expected {
                return Err(kerminal_application::AppError::conflict(
                    "插件进程代次已改变",
                ));
            }
            let page: ViewFetchResult = serde_json::from_value(reply.value)
                .map_err(|_| kerminal_application::AppError::invalid("插件分页返回无效"))?;
            let total = usize::try_from(page.total)
                .map_err(|_| kerminal_application::AppError::invalid("插件分页总数无效"))?;
            Ok((total, page.rows))
        });
        match result {
            Ok((total, rows)) => {
                if self
                    .projection
                    .state_mut()
                    .finish_page(&ticket, total, rows)
                    .is_err()
                {
                    return;
                }
                let rows = self.projection.state().page_rows(ticket.node()).to_vec();
                if let Some(total) = self.projection.state().page_total(ticket.node())
                    && let Some(collection) = self.collections.get(&node)
                {
                    collection.update(cx, |collection, cx| {
                        collection.apply_page(&query, &rows, total, cx)
                    });
                }
            }
            Err(error) => {
                let _ = self.projection.state_mut().fail_page(&ticket);
                if let Some(collection) = self.collections.get(&node) {
                    collection.update(cx, |collection, cx| {
                        collection.apply_error(error.public_message(), cx)
                    });
                }
            }
        }
    }

    /// 当前结构严格对应冻结节点树；插件文本只作为内容，不解释为 HTML。
    pub(super) fn render_node(
        &mut self,
        node: &NativeNode,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.projection.is_visible(node) {
            return div().into_any_element();
        }
        use NativeNodeKind::*;
        match &node.kind {
            Split { children, .. } => {
                let mut group = h_resizable(format!("external-split-{path}"));
                if let Some(state) = self.split_states.get(&path) {
                    group = group.with_state(state);
                }
                for (index, child) in children.iter().enumerate() {
                    let content = self.render_node(child, format!("{path}.{index}"), window, cx);
                    group = group
                        .child(resizable_panel().child(div().size_full().min_w_0().child(content)));
                }
                group.into_any_element()
            }
            Stack { children, .. }
            | Section { children, .. }
            | Row { children, .. }
            | Toolbar { children, .. } => {
                let horizontal = matches!(&node.kind, Row { .. } | Toolbar { .. });
                let gap = match &node.kind {
                    Stack { gap, .. }
                    | Section { gap, .. }
                    | Row { gap, .. }
                    | Toolbar { gap, .. } => *gap,
                    _ => None,
                };
                let mut box_ = div()
                    .flex()
                    .when(!horizontal, |d| d.flex_col())
                    .when(horizontal, |d| d.items_center())
                    .w_full();
                box_ = match gap.unwrap_or(Gap::Md) {
                    Gap::Sm => box_.gap_1(),
                    Gap::Md => box_.gap_2(),
                    Gap::Lg => box_.gap_4(),
                };
                if let Section { title, .. } = &node.kind {
                    box_ = box_.child(div().font_medium().child(self.projection.text(title)));
                }
                for (index, child) in children.iter().enumerate() {
                    box_ =
                        box_.child(self.render_node(child, format!("{path}.{index}"), window, cx));
                }
                box_.into_any_element()
            }
            Scroll { child, .. } => div()
                .id(format!("external-scroll-{path}"))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(self.render_node(child, format!("{path}.0"), window, cx))
                .into_any_element(),
            Tabs { tabs, .. } => {
                let selected = self
                    .active_tabs
                    .get(&path)
                    .cloned()
                    .or_else(|| tabs.first().map(|tab| tab.id.clone()))
                    .unwrap_or_default();
                let mut header = div().flex().gap_1();
                for tab in tabs {
                    let tab_id = tab.id.clone();
                    let path_for_click = path.clone();
                    header = header.child(
                        KitButton::new(format!("external-tab-{path}-{}", tab.id))
                            .label(self.projection.text(&tab.title))
                            .when(tab.id == selected, |b| b.primary())
                            .on_click(cx.listener(move |renderer, _, _, cx| {
                                renderer
                                    .active_tabs
                                    .insert(path_for_click.clone(), tab_id.clone());
                                cx.notify();
                            })),
                    );
                }
                let mut content = div().flex().flex_col().gap_2().child(header);
                if let Some((index, tab)) =
                    tabs.iter().enumerate().find(|(_, tab)| tab.id == selected)
                {
                    content = content.child(self.render_node(
                        &tab.content,
                        format!("{path}.{index}"),
                        window,
                        cx,
                    ));
                }
                content.into_any_element()
            }
            Text { text, tone, .. } | Badge { text, tone, .. } | Notice { text, tone, .. } => {
                let color = match tone.unwrap_or(Tone::Default) {
                    Tone::Default => cx.theme().foreground,
                    Tone::Muted => cx.theme().muted_foreground,
                    Tone::Success => cx.theme().success,
                    Tone::Warning => cx.theme().warning,
                    Tone::Danger => cx.theme().danger,
                };
                div()
                    .text_color(color)
                    .child(self.projection.text(text))
                    .into_any_element()
            }
            Markdown { text, .. } => super::markdown::render_markdown(
                format!("external-markdown-{path}"),
                self.projection.text(text),
                self.prepared
                    .package
                    .manifest
                    .permissions
                    .iter()
                    .cloned()
                    .collect(),
            ),
            Icon { icon, .. } => match resolve_icon(&self.prepared, icon) {
                Some(icon) => icon.into_any_element(),
                None => div()
                    .text_color(cx.theme().danger)
                    .child(format!(
                        "{}: {icon}",
                        crate::locale::text("图标不可用", "Icon unavailable")
                    ))
                    .into_any_element(),
            },
            Code { .. } => self
                .code_blocks
                .get(&path)
                .map(|(state, _)| {
                    Editor::new(state)
                        .readonly(true)
                        .aria_label(crate::locale::text("代码", "Code"))
                        .w_full()
                        .h(px(160.))
                        .into_any_element()
                })
                .unwrap_or_else(|| {
                    div()
                        .text_color(cx.theme().danger)
                        .child(crate::locale::text(
                            "代码视图不可用",
                            "Code view unavailable",
                        ))
                        .into_any_element()
                }),
            Progress { value, max, .. } => {
                let value = match value {
                    Value::String(value) if value.starts_with('$') => {
                        self.field_value(value).and_then(Value::as_f64)
                    }
                    _ => value.as_f64(),
                }
                .unwrap_or(0.0);
                let max = max.unwrap_or(1.0).max(f64::EPSILON);
                let percent = (value / max).clamp(0.0, 1.0) * 100.0;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(
                        KitProgress::new(format!("external-progress-{path}"))
                            .value(percent as f32)
                            .accessibility_label(crate::locale::text("进度", "Progress"))
                            .flex_1(),
                    )
                    .child(format!("{percent:.0}%"))
                    .into_any_element()
            }
            Empty {
                title,
                icon,
                description,
                ..
            } => div()
                .flex()
                .flex_col()
                .gap_1()
                .when_some(
                    icon.as_deref()
                        .and_then(|name| resolve_icon(&self.prepared, name)),
                    |d, icon| d.child(icon),
                )
                .child(self.projection.text(title))
                .when_some(description.as_ref(), |d, text| {
                    d.child(self.projection.text(text))
                })
                .into_any_element(),
            Input { bind, label, .. }
            | Textarea { bind, label, .. }
            | Number { bind, label, .. }
            | Select { bind, label, .. }
            | Path { bind, label, .. }
            | Secret { bind, label, .. } => {
                self.render_field(&path, bind, label.as_deref(), node, cx)
            }
            Switch { bind, label, .. } | Checkbox { bind, label, .. } => {
                let checked = self
                    .field_value(bind)
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let label = label
                    .as_deref()
                    .map(|s| self.projection.text(s))
                    .unwrap_or_default();
                let bind = bind.clone();
                if matches!(&node.kind, Switch { .. }) {
                    KitSwitch::new(format!("external-{path}"))
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |renderer, checked: &bool, _, cx| {
                            renderer.edit_field(&bind, Value::Bool(*checked), cx)
                        }))
                        .into_any_element()
                } else {
                    KitCheckbox::new(format!("external-{path}"))
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |renderer, checked: &bool, _, cx| {
                            renderer.edit_field(&bind, Value::Bool(*checked), cx)
                        }))
                        .into_any_element()
                }
            }
            Button {
                action,
                label,
                icon,
                variant,
                busy,
                ..
            } => {
                let key = node.id.clone().unwrap_or(path.clone());
                let action = action.clone();
                let busy = busy
                    .as_deref()
                    .and_then(|path| self.field_value(path))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || self.projection.state().is_action_busy(&key);
                let resolved_icon = icon
                    .as_deref()
                    .and_then(|name| resolve_icon(&self.prepared, name));
                let label = label.as_deref().map(|label| self.projection.text(label));
                let fallback = if resolved_icon.is_none() && label.is_none() {
                    Some(icon.as_deref().unwrap_or(&action).to_owned())
                } else {
                    None
                };
                let accessibility = label
                    .clone()
                    .or(fallback.clone())
                    .unwrap_or_else(|| action.clone());
                let button = KitButton::new(format!("external-button-{path}"))
                    .accessibility_label(accessibility)
                    .when_some(label.or(fallback), |button, label| button.label(label))
                    .when_some(resolved_icon, |button, icon| button.icon(icon))
                    .loading(busy)
                    .disabled(
                        busy || !self.field_errors.is_empty()
                            || self.secret_write_pending()
                            || !matches!(self.status, OpenStatus::Ready),
                    )
                    .on_click(cx.listener(move |renderer, _, _, cx| {
                        renderer.action(key.clone(), action.clone(), None, cx)
                    }));
                match variant.unwrap_or(ButtonVariant::Secondary) {
                    ButtonVariant::Primary => button.primary(),
                    ButtonVariant::Secondary => button.secondary(),
                    ButtonVariant::Ghost => button.ghost(),
                    ButtonVariant::Danger => button.danger(),
                }
                .into_any_element()
            }
            Menu { items, .. } => {
                let entries = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        (
                            format!("{path}.{index}"),
                            self.projection.text(&item.label),
                            item.action.clone(),
                            item.danger.unwrap_or(false),
                        )
                    })
                    .collect::<Vec<_>>();
                let renderer = cx.entity().clone();
                let theme = crate::theme::ThemeTokens::current(window, cx).for_workbench();
                KitButton::new(format!("external-menu-{path}"))
                    .label(crate::locale::text("更多", "More"))
                    .icon(gpui_kit::assets::IconName::Ellipsis)
                    .ghost()
                    .disabled(
                        !matches!(self.status, OpenStatus::Ready)
                            || !self.field_errors.is_empty()
                            || self.secret_write_pending(),
                    )
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = crate::ui_popup::dropdown_menu(menu, theme);
                        for (key, label, action, danger) in &entries {
                            let renderer = renderer.clone();
                            let key = key.clone();
                            let action = action.clone();
                            let mut entry =
                                PopupMenuItem::new(label.clone()).on_click(move |_, _, app| {
                                    renderer.update(app, |renderer, cx| {
                                        renderer.action(key.clone(), action.clone(), None, cx)
                                    });
                                });
                            if *danger {
                                entry = entry.danger();
                            }
                            menu = menu.item(entry);
                        }
                        menu
                    })
                    .into_any_element()
            }
            List { .. } | Table { .. } | Tree { .. } => self
                .collections
                .get(node.id.as_deref().unwrap_or(&path))
                .map(|collection| {
                    div()
                        .min_h(px(120.))
                        .flex_1()
                        .child(collection.clone())
                        .into_any_element()
                })
                .unwrap_or_else(|| div().into_any_element()),
            Form { schema, bind, .. } => {
                if let Some((expected, form)) = &self.embedded_form
                    && schema == expected
                    && bind.is_none()
                {
                    return div().child(form.clone()).into_any_element();
                }
                match self.forms.get(&path) {
                    Some(Ok(form)) => {
                        let root = form.root.clone();
                        self.render_node(&root, format!("{path}.form"), window, cx)
                    }
                    Some(Err(error)) => div()
                        .text_color(cx.theme().danger)
                        .child(error.clone())
                        .into_any_element(),
                    None => div()
                        .text_color(cx.theme().danger)
                        .child(crate::locale::text("表单不可用", "Form unavailable"))
                        .into_any_element(),
                }
            }
        }
    }

    /// 字段外壳统一显示标签、远端冲突和校验错误，输入实体保持原位。
    fn render_field(
        &self,
        path: &str,
        bind: &str,
        label: Option<&str>,
        node: &NativeNode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut field = div().flex().flex_col().gap_1().w_full();
        if let Some(label) = label {
            field = field.child(div().text_sm().child(self.projection.text(label)));
        }
        if let Some(control) = self.fields.get(path) {
            field = match control {
                FieldControl::Input {
                    state,
                    secret,
                    secret_write,
                    ..
                } => {
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Input::new(state).w_full());
                    if *secret {
                        let pending = secret_write
                            .as_ref()
                            .is_some_and(|write| write.is_pending());
                        let path = path.to_owned();
                        row = row.child(
                            KitButton::new(format!("external-secret-{path}"))
                                .label(crate::locale::text("保存", "Save"))
                                .loading(pending)
                                .disabled(pending)
                                .on_click(cx.listener(move |renderer, _, _, cx| {
                                    renderer.save_secret(&path, cx)
                                })),
                        );
                    }
                    if let NativeNodeKind::Path { mode, .. } = &node.kind {
                        let bind = bind.to_owned();
                        let mode = *mode;
                        row = row.child(
                            KitButton::new(format!("external-path-{path}"))
                                .label(crate::locale::text("选择", "Browse"))
                                .on_click(cx.listener(move |renderer, _, _, cx| {
                                    renderer.choose_path(bind.clone(), mode, cx)
                                })),
                        );
                    }
                    field.child(row)
                }
                FieldControl::Textarea { state, .. } => field.child(Textarea::new(state).w_full()),
                FieldControl::Select { state, .. } => field.child(Select::new(state).w_full()),
            };
        }
        if let Ok(path) = BindingPath::parse(bind)
            && self.projection.state().has_new_value(&path)
        {
            field = field.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .child(crate::locale::text("有新值", "New value available")),
            );
        }
        if let Some(error) = self.field_errors.get(bind) {
            field = field.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        #[cfg(feature = "right-rail-acceptance")]
        {
            field
                .id(format!("external-field-{path}"))
                .into_any_element()
        }
        #[cfg(not(feature = "right-rail-acceptance"))]
        {
            field.into_any_element()
        }
    }

    /// 原生文件选择器只在用户点击时打开；取消不改变原有草稿。
    fn choose_path(&mut self, bind: String, mode: Option<PickerMode>, cx: &mut Context<Self>) {
        if matches!(mode, Some(PickerMode::Save)) {
            let selected = cx.prompt_for_new_path(std::path::Path::new("."), None);
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(path))) = selected.await {
                    let _ = this.update_in(cx, |renderer, window, cx| {
                        renderer.apply_selected_path(&bind, path, window, cx)
                    });
                }
            })
            .detach();
        } else {
            let selected = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: true,
                multiple: false,
                prompt: None,
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(paths))) = selected.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let _ = this.update_in(cx, |renderer, window, cx| {
                        renderer.apply_selected_path(&bind, path, window, cx)
                    });
                }
            })
            .detach();
        }
    }

    /// 选择结果必须能编码为字符串；同步所有绑定控件并保留其他字段草稿。
    fn apply_selected_path(
        &mut self,
        bind: &str,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match path.to_str() {
            Some(value) => {
                self.edit_field(bind, Value::String(value.to_owned()), cx);
                self.sync_controls(window, cx);
            }
            None => {
                self.error = Some(
                    crate::locale::text("路径无法表示为文本", "Path cannot be represented as text")
                        .to_string(),
                );
                cx.notify();
            }
        }
    }
}

/// 分别记录已知内置名和冻结包内字节，避免把插件字符串交给任意资源加载器。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PluginIconSource<'a> {
    Builtin(gpui_kit::assets::IconName),
    Package(&'a [u8]),
}

/// 只接受内置目录的精确名称或已冻结的 SVG 键；外部 URL、缺失资源均失败。
pub(super) fn icon_source<'a>(
    resources: &'a BTreeMap<String, std::sync::Arc<[u8]>>,
    name: &str,
) -> Option<PluginIconSource<'a>> {
    use gpui_kit::assets::IconName;
    static BUILTIN: OnceLock<BTreeMap<String, IconName>> = OnceLock::new();
    if name.ends_with(".svg") || name.contains('/') {
        if !name.ends_with(".svg") || kerminal_plugin_protocol::validate_package_path(name).is_err()
        {
            return None;
        }
        return resources
            .get(name)
            .map(|bytes| PluginIconSource::Package(bytes.as_ref()));
    }
    let icons = BUILTIN.get_or_init(|| {
        let mut names = BTreeMap::new();
        for icon in IconName::ALL.iter().copied() {
            let path = icon.path();
            if let Some(stem) = path
                .strip_prefix("icons/")
                .and_then(|path| path.strip_suffix(".svg"))
            {
                names.insert(stem.to_owned(), icon);
            }
            names.insert(format!("{icon:?}"), icon);
        }
        names
    });
    icons.get(name).copied().map(PluginIconSource::Builtin)
}

/// 展示层只消费内存中的确认结果，不从插件传入的名称读取文件或网络。
fn resolve_icon(prepared: &PreparedPlugin, name: &str) -> Option<Icon> {
    match icon_source(&prepared.resources, name)? {
        PluginIconSource::Builtin(icon) => Some(Icon::new(icon)),
        PluginIconSource::Package(bytes) => Some(Icon::default().data(bytes)),
    }
}
