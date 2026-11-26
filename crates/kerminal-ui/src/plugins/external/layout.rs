//! 原生节点布局、交互投影和集合分页路由。
//!
//! @author kongweiguang

use super::{
    collections::NativeCollection,
    inputs::{FieldControl, SelectSource},
    renderer::{
        CollectionEvent, CollectionQuery, NativeCollectionConfig, NativeRenderer, OpenStatus,
        collection_cache_key,
    },
    runtime::RuntimeRequest,
    state::{BindingPath, PageTicket},
};
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, IntoElement, PathPromptOptions, Pixels, Styled,
    Window,
    assets::IconName,
    base::{Disableable as _, StyledExt as _},
    black, canvas,
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::{Button as KitButton, ButtonVariants as _},
        checkbox::Checkbox as KitCheckbox,
        group_box::{GroupBox, GroupBoxVariants as _},
        input::{Editor, Input, Textarea},
        menu::{DropdownMenu as _, PopupMenuItem},
        progress::Progress as KitProgress,
        resizable::{ResizableState, h_resizable, resizable_panel},
        select::Select,
        switch::Switch as KitSwitch,
        tooltip::Tooltip,
    },
    container_query, div, fill, point,
    prelude::*,
    px, rems, size, white,
};
use kerminal_application::external_contributions::PreparedPlugin;
use kerminal_plugin_protocol::{
    native::{
        ButtonVariant, Gap, NativeNode, NativeNodeKind, PickerMode, QR_QUIET_ZONE, Tone,
        validate_qr_matrix,
    },
    runtime::ViewFetchResult,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
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
            List { .. } if has_appearance(node, "flat-copy-list") => {}
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

    /// 节点位置沿用冻结树的索引，使条件隐藏不会改变其余控件的身份或草稿。
    /// 布局只采用宿主主题与已知样式，插件文本不解释为 HTML；隐藏节点不参与可见项布局次序。
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
                let header = horizontal && has_appearance(node, "header");
                let stretch_first =
                    matches!(&node.kind, Row { .. }) && has_appearance(node, "stretch-first");
                let between = matches!(&node.kind, Row { .. }) && has_appearance(node, "between");
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
                    .when(between, |d| d.justify_between())
                    .when(header, |d| d.flex_wrap())
                    .min_w_0()
                    .w_full();
                box_ = match gap.unwrap_or(Gap::Md) {
                    Gap::Sm => box_.gap_1(),
                    Gap::Md => box_.gap_2(),
                    Gap::Lg => box_.gap_4(),
                };
                let section_title = match &node.kind {
                    Section { title, .. } => Some(self.projection.text(title)),
                    _ => None,
                };
                // 分组外观由节点自身声明决定，不以“是否声明了标题”为条件：
                // 无标题的分组也要渲染卡片，否则插件无法只靠间距分组而不占用标题槽。
                let grouped =
                    matches!(&node.kind, Section { .. }) && has_appearance(node, "grouped");
                if let Some(title) = section_title.as_ref().filter(|_| !grouped) {
                    box_ = box_.child(div().font_medium().child(title.clone()));
                }
                let mut visible_index = 0;
                for (index, child) in children.iter().enumerate() {
                    if !self.projection.is_visible(child) {
                        continue;
                    }
                    let content = self.render_node(child, format!("{path}.{index}"), window, cx);
                    box_ = if stretch_first && visible_index == 0 {
                        box_.child(div().flex_1().min_w_0().child(content))
                    } else if stretch_first {
                        box_.child(div().flex_shrink_0().child(content))
                    } else if header && index == 0 {
                        // 标题优先占用剩余空间；狭窄面板把动作换行，避免裁掉标题或按钮。
                        box_.child(div().flex_1().min_w(rems(7.)).max_w_full().child(content))
                    } else {
                        box_.child(content)
                    };
                    visible_index += 1;
                }
                if grouped {
                    match section_title {
                        Some(title) if !title.trim().is_empty() => GroupBox::new()
                            .id(format!("external-group-{path}"))
                            .fill()
                            .title(div().text_sm().font_medium().child(title))
                            .child(box_)
                            .into_any_element(),
                        // 标题为空或缺失时不再输出 GroupBox 的空标题槽，
                        // 只保留圆角卡片本体，让内部控件自己承担全部留白。
                        _ => {
                            let theme =
                                crate::theme::ThemeTokens::current(window, cx).for_workbench();
                            div()
                                .w_full()
                                .p_4()
                                .rounded(theme.radius_panel)
                                .bg(cx.theme().group_box)
                                .border_1()
                                .border_color(cx.theme().border)
                                .child(box_)
                                .into_any_element()
                        }
                    }
                } else {
                    box_.into_any_element()
                }
            }
            Scroll { child, .. } => {
                let content = self.render_node(child, format!("{path}.0"), window, cx);
                div()
                    .id(format!("external-scroll-{path}"))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(if has_appearance(node, "inset") {
                        // 滚动条贴面板边缘；内容限宽，避免右栏放大后表单和按钮相距过远。
                        div()
                            .flex()
                            .justify_center()
                            .px_4()
                            .py_4()
                            .child(div().w_full().min_w_0().max_w(rems(36.)).child(content))
                            .into_any_element()
                    } else {
                        content
                    })
                    .into_any_element()
            }
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
                    .min_w_0()
                    .text_color(color)
                    .when(has_appearance(node, "title"), |d| {
                        d.text_xl().font_semibold()
                    })
                    .when(has_appearance(node, "label"), |d| {
                        d.text_sm()
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                    })
                    .when(has_appearance(node, "caption"), |d| d.text_sm())
                    .when(has_appearance(node, "centered-caption"), |d| {
                        d.w_full().text_sm().text_center()
                    })
                    .when(has_appearance(node, "value"), |d| {
                        d.text_sm()
                            .text_color(cx.theme().foreground)
                            .whitespace_normal()
                    })
                    .when(has_appearance(node, "mono"), |d| {
                        d.font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .font_medium()
                    })
                    .child(self.projection.text(text))
                    .into_any_element()
            }
            Markdown { text, .. } => super::markdown::render_markdown(
                format!("external-markdown-{path}"),
                self.projection.text(text),
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
            Qrcode { matrix, label, .. } => {
                let label_text = label.as_deref().map(|label| self.projection.text(label));
                let accessibility = label_text
                    .clone()
                    .unwrap_or_else(|| crate::locale::text("二维码", "QR code").to_string());
                let matrix = self
                    .field_value(matrix)
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if validate_qr_matrix(matrix).is_err() {
                    return div()
                        .text_color(cx.theme().danger)
                        .child(crate::locale::text(
                            "二维码数据无效",
                            "Invalid QR code data",
                        ))
                        .into_any_element();
                }
                // 底板边长按「含静区模块数 × 约 5px」求，再夹到侧栏可用宽度；这只是布局上限。
                // 实际模块尺寸在绘制时按设备像素向下取整并居中，剩余留白属于额外静区，
                // 不会削弱扫码对比，因此这里不为了填满而放大到非整数设备像素。
                let total = matrix.lines().count() + QR_QUIET_ZONE * 2;
                let plate = (total as f32 * 5.0).clamp(145.0, 280.0);
                let matrix = matrix.to_owned();
                let mut card = div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .py_2()
                    .items_center()
                    .gap_2()
                    .id(format!("external-qrcode-{path}"))
                    .role(gpui_kit::Role::Image)
                    .aria_label(accessibility)
                    .child(
                        // 外层 1px 细边框与内边距独立于二维码画布，避免在浅色卡片上白底边缘
                        // 融进背景，同时不挤占内部 `plate` 的任何绘制像素，确保 `qr_layout` 判定不变。
                        {
                            let theme =
                                crate::theme::ThemeTokens::current(window, cx).for_workbench();
                            div()
                                .max_w_full()
                                .p(px(1.0))
                                .rounded(theme.control_radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(white())
                                .overflow_hidden()
                                .child(
                                    // 底板只负责占位与正方形比例；白底由 canvas 自己铺满整个区域（含静区），
                                    // 这样画不出二维码时不会留下一个看起来像空码的白色方块。
                                    // 用确定边长作为基准，而不是百分比宽度：百分比在自动宽度的父级里会解析
                                    // 成 auto，随后子元素 100% 高度也变 0，二维码会静默消失。确定宽度在弹性
                                    // 布局里仍可被收缩，max_w_full 再按父级宽度夹紧，正方形比例让高度跟随。
                                    // 不画边框：1px 边框会占掉内容盒，恰好够宽的二维码会被误判为画不下。
                                    div()
                                        .w(px(plate))
                                        .max_w_full()
                                        .aspect_square()
                                        .overflow_hidden()
                                        .child(
                                            // 布局完成后才知道真实可用宽度：画不下时给出可见失败状态，
                                            // 而不是让模块被舍入成 0 后静默画出空白码或截断图案。
                                            // 容器查询只提供尺寸，这里用零原点构造边界仅做可行性判断；
                                            // 真正的原点在绘制回调里由 GPUI 给出，并再次校验。
                                            container_query(move |area, window, cx| {
                                                let probe =
                                                    Bounds::new(point(px(0.0), px(0.0)), area);
                                                let scale = window.scale_factor();
                                                if qr_layout(&matrix, probe, scale).is_none() {
                                                    return div()
                                                        .size_full()
                                                        .p_2()
                                                        .text_sm()
                                                        .text_color(cx.theme().danger)
                                                        .child(crate::locale::text(
                                                            "空间不足，无法显示二维码",
                                                            "Not enough space to show the QR code",
                                                        ))
                                                        .into_any_element();
                                                }
                                                // 白底由 paint_qr 自己铺满整幅画布（含静区），这里不再叠样式背景：
                                                // 万一绘制阶段再次判定不可用，宁可什么都不画，也不留下像空码的白方块。
                                                canvas(
                                                    |_, _, _| (),
                                                    move |bounds, _, window, _| {
                                                        paint_qr(&matrix, bounds, window)
                                                    },
                                                )
                                                .size_full()
                                                .into_any_element()
                                            }),
                                        ),
                                )
                        },
                    );
                if let Some(label) = label_text {
                    card = card.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    );
                }
                card.into_any_element()
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
                extensions,
            } => {
                let key = node.id.clone().unwrap_or(path.clone());
                let action = action.clone();
                // 可编辑草稿即时更新入口；Sidecar 仍负责实际目录和地址校验。
                let missing_text = extensions
                    .get("x-kerminal-requires-text")
                    .and_then(Value::as_str)
                    .and_then(|binding| binding.strip_prefix('$'))
                    .is_some_and(|binding| {
                        self.is_stale_runtime_binding(binding)
                            || self
                                .field_value(binding)
                                .and_then(Value::as_str)
                                .is_none_or(str::is_empty)
                    });
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
                        busy || missing_text
                            || !self.field_errors.is_empty()
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
            List {
                key,
                title,
                subtitle,
                items,
                on_activate,
                extensions,
                ..
            } if has_appearance(node, "flat-copy-list") => self.render_flat_copy_list(
                &path,
                node.id.as_deref(),
                key,
                title,
                subtitle.as_deref(),
                items.as_deref(),
                on_activate.as_deref(),
                extensions,
                window,
                cx,
            ),
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

    /// 将小型内联集合渲染为“左侧分组标签 + 等宽地址 + 行尾复制图标”的紧凑平铺列表。
    ///
    /// 用于多网卡地址、端口端点等需要一眼扫视并单点复制的场景：不套虚拟列表的固定
    /// 高度盒，按内容自然撑开；同组标签只在首行显示一次，保持与桌面参考规范一致的
    /// 双列对齐。点击整行触发 `on_activate`（如切换二维码），点击行尾图标按钮则以
    /// `item_key` 直接触发 `x-kerminal-row-action`（如复制该行链接）。
    ///
    /// @author kongweiguang
    #[allow(clippy::too_many_arguments)]
    fn render_flat_copy_list(
        &self,
        path: &str,
        node_id: Option<&str>,
        key_field: &str,
        title_field: &str,
        subtitle_field: Option<&str>,
        items_binding: Option<&str>,
        on_activate: Option<&str>,
        extensions: &BTreeMap<String, Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const MAX_FLAT_COPY_ITEMS: usize = 64;
        let rows = items_binding
            .and_then(|binding| self.field_value(binding))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if rows.is_empty() {
            return div().into_any_element();
        }
        let theme = crate::theme::ThemeTokens::current(window, cx).for_workbench();
        let mono_font = cx.theme().mono_font_family.clone();
        let selected = extensions
            .get("x-kerminal-selected-binding")
            .and_then(Value::as_str)
            .and_then(|binding| self.field_value(binding))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let copied = extensions
            .get("x-kerminal-copied-binding")
            .and_then(Value::as_str)
            .and_then(|binding| self.field_value(binding))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let row_action = extensions
            .get("x-kerminal-row-action")
            .and_then(Value::as_str)
            .filter(|action| !action.trim().is_empty())
            .map(str::to_owned);
        let disabled = !matches!(self.status, OpenStatus::Ready)
            || !self.field_errors.is_empty()
            || self.secret_write_pending();
        let has_groups = subtitle_field.is_some_and(|field| {
            rows.iter().take(MAX_FLAT_COPY_ITEMS).any(|row| {
                row.get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.is_empty())
            })
        });
        let node_key = node_id.unwrap_or(path).to_owned();
        let mut list = div().w_full().flex().flex_col().gap_1();
        let mut last_group: Option<String> = None;

        for (index, row) in rows.iter().take(MAX_FLAT_COPY_ITEMS).enumerate() {
            let Some(item_key) = row
                .get(key_field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            let title_text = row
                .get(title_field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| item_key.clone());
            let group_text = subtitle_field
                .and_then(|field| row.get(field))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            let hint_text = row
                .get("hint")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
            let group_changed = last_group.as_deref() != Some(group_text.as_str());
            let is_subsequent_group = last_group.is_some() && group_changed;
            let visible_group = if group_changed {
                last_group = Some(group_text.clone());
                group_text
            } else {
                String::new()
            };
            let is_selected = selected.as_deref() == Some(item_key.as_str());
            let is_copied = copied.as_deref() == Some(item_key.as_str());

            let mut item_surface = div()
                .id(format!("external-flat-row-{path}-{index}"))
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.control_radius)
                .when(is_selected, |d| d.bg(theme.surface_selected()))
                .when(!is_selected, |d| {
                    d.hover(|style| style.bg(theme.surface_hover()))
                })
                .when_some(hint_text, |d, hint| {
                    d.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono_font.clone())
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(title_text.clone()),
                );

            if let Some(action) = on_activate.filter(|_| !disabled) {
                let activate_key = format!("{node_key}.select.{index}");
                let activate_action = action.to_owned();
                let activate_item = item_key.clone();
                item_surface = item_surface.on_click(cx.listener(move |renderer, _, _, cx| {
                    renderer.action(
                        activate_key.clone(),
                        activate_action.clone(),
                        Some(activate_item.clone()),
                        cx,
                    );
                }));
            }

            if let Some(action) = row_action.clone() {
                let copy_key = format!("{node_key}.copy.{index}");
                let copy_busy = self.projection.state().is_action_busy(&copy_key);
                let copy_item = item_key.clone();
                let tooltip = if is_copied {
                    crate::locale::text("已复制", "Copied")
                } else {
                    crate::locale::text(format!("复制 {title_text}"), format!("Copy {title_text}"))
                };
                let icon = if is_copied {
                    Icon::new(IconName::Check).text_color(theme.success)
                } else {
                    Icon::new(IconName::Copy).text_color(theme.muted)
                };
                item_surface = item_surface.child(
                    KitButton::new(format!("external-flat-copy-{path}-{index}"))
                        .compact()
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(icon)
                        .accessibility_label(tooltip.clone())
                        .tooltip(tooltip)
                        .loading(copy_busy)
                        .disabled(disabled || copy_busy)
                        .on_click(cx.listener(move |renderer, _, _, cx| {
                            cx.stop_propagation();
                            renderer.action(
                                copy_key.clone(),
                                action.clone(),
                                Some(copy_item.clone()),
                                cx,
                            );
                        })),
                );
            }

            let mut line = div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .when(is_subsequent_group, |d| d.pt_1());
            if has_groups {
                line = line.child(
                    div()
                        .w(rems(2.75))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.muted)
                        .truncate()
                        .child(visible_group),
                );
            }
            list = list.child(line.child(item_surface));
        }

        list.into_any_element()
    }

    /// 视觉标签与输入的可访问名称共用文案；路径选择按钮不与长路径争抢宽度。
    fn render_field(
        &self,
        path: &str,
        bind: &str,
        label: Option<&str>,
        node: &NativeNode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut field = div().flex().flex_col().gap_1().w_full();
        let label = label.map(|text| self.projection.text(text));
        if let Some(label) = &label {
            field = field.child(div().text_sm().font_semibold().child(label.clone()));
        }
        if let Some(control) = self.fields.get(path) {
            field = match control {
                FieldControl::Input {
                    state,
                    secret,
                    secret_write,
                    ..
                } => {
                    let mut row = div().flex().items_center().gap_2().child(
                        Input::new(state)
                            .flex_1()
                            .min_w_0()
                            .when_some(label.clone(), |input, label| input.aria_label(label)),
                    );
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
                                .label(crate::locale::text("选择…", "Browse…"))
                                .flex_shrink_0()
                                .on_click(cx.listener(move |renderer, _, _, cx| {
                                    renderer.choose_path(bind.clone(), mode, cx)
                                })),
                        );
                    }
                    field.child(row)
                }
                FieldControl::Textarea { state, .. } => field.child(Textarea::new(state).w_full()),
                FieldControl::Select { state, .. } => {
                    // 名称和当前值分开声明，验收探针可以按本地化字段标签定位控件，
                    // 不需要猜测像素坐标；普通输入节点升级为下拉时仍沿用原 placeholder。
                    let select = Select::new(state)
                        .w_full()
                        .when_some(label.clone(), |select, label| {
                            select.accessibility_label(label)
                        });
                    let select = match &node.kind {
                        NativeNodeKind::Input {
                            placeholder: Some(placeholder),
                            ..
                        } => select.placeholder(self.projection.text(placeholder)),
                        _ => select,
                    };
                    field.child(select)
                }
            };
            if matches!(
                control,
                FieldControl::Select {
                    source: SelectSource::Runtime { stale: true, .. },
                    ..
                }
            ) {
                // 补丁可能移除用户已选中的值；保留草稿并提示重新选择。动作快照会
                // 把该绑定投影为 null，显式声明依赖的按钮被禁用，其他动作不受影响。
                field = field.child(div().text_sm().text_color(cx.theme().danger).child(
                    crate::locale::text(
                        "所选选项已不可用，请重新选择",
                        "The selected option is no longer available; choose another",
                    ),
                ));
            }
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
    /// @author kongweiguang
    fn choose_path(&mut self, bind: String, mode: Option<PickerMode>, cx: &mut Context<Self>) {
        match path_prompt_options(mode.unwrap_or(PickerMode::Open)) {
            None => {
                let selected = cx.prompt_for_new_path(std::path::Path::new("."), None);
                cx.spawn(async move |this, cx| match selected.await {
                    Ok(Ok(Some(path))) => {
                        let _ = this.update_in(cx, |renderer, window, cx| {
                            renderer.apply_selected_path(&bind, path, window, cx)
                        });
                    }
                    Ok(Err(_)) => {
                        let _ = this
                            .update_in(cx, |renderer, _, cx| renderer.report_picker_failure(cx));
                    }
                    // 用户取消或窗口已释放：两者都不写回，草稿保持原样。
                    _ => {}
                })
                .detach();
            }
            Some(options) => {
                let selected = cx.prompt_for_paths(options);
                cx.spawn(async move |this, cx| match selected.await {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            let _ = this.update_in(cx, |renderer, window, cx| {
                                renderer.apply_selected_path(&bind, path, window, cx)
                            });
                        }
                    }
                    Ok(Err(_)) => {
                        let _ = this
                            .update_in(cx, |renderer, _, cx| renderer.report_picker_failure(cx));
                    }
                    // 取消不是失败：不覆盖草稿，也不弹额外提示。
                    _ => {}
                })
                .detach();
            }
        }
    }

    /// 选择器自身失败必须可见；静默丢弃会让用户误以为路径已经选好。
    /// @author kongweiguang
    fn report_picker_failure(&mut self, cx: &mut Context<Self>) {
        self.error = Some(
            crate::locale::text("无法打开路径选择器", "Cannot open the path picker").to_string(),
        );
        cx.notify();
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

/// 只识别宿主提供的外观名称，旧包和未知扩展继续使用默认布局。
fn has_appearance(node: &NativeNode, appearance: &str) -> bool {
    use NativeNodeKind::*;
    let extensions = match &node.kind {
        Scroll { extensions, .. }
        | Row { extensions, .. }
        | Toolbar { extensions, .. }
        | Section { extensions, .. }
        | Text { extensions, .. }
        | Badge { extensions, .. }
        | Notice { extensions, .. }
        | List { extensions, .. } => extensions,
        _ => return false,
    };
    extensions
        .get("x-kerminal-appearance")
        .and_then(Value::as_str)
        == Some(appearance)
}

/// 打开/保存/仅目录三种模式到平台选择参数的唯一映射；保存走独立的新路径对话框。
///
/// 目录模式关闭 `files`：Windows 只按 `directories` 决定文件夹选择，Linux 门户也按
/// 同一字段选择目录，因此这一层就是目录语义的唯一边界；存在性、类型与权限由
/// Sidecar 或共享执行器校验，不在窗口线程同步触碰磁盘。
/// @author kongweiguang
pub(super) fn path_prompt_options(mode: PickerMode) -> Option<PathPromptOptions> {
    match mode {
        PickerMode::Save => None,
        PickerMode::Open => Some(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: None,
        }),
        PickerMode::Directory => Some(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        }),
    }
}

/// 二维码布局在绘制前一次确定：先验证方阵，再按设备像素取整模块尺寸与原点，避免高 DPI
/// 缩放出半像素灰缝；静区由宿主补足，插件矩阵本身不含静区。
///
/// 坐标语义刻意分成两套：`is_dark` 与 `total_modules` 使用含静区的绘制坐标，矩阵内容
/// 只在减去静区后读取。此前绘制循环误用原始边长遍历含静区坐标，导致右侧和底部各
/// 少画 4 个模块、二维码被截断无法识别，因此这里把两个边长分开命名，避免再次混用。
/// @author kongweiguang
pub(super) struct QrLayout<'a> {
    rows: Vec<&'a str>,
    module_px: f32,
    origin_x: f32,
    origin_y: f32,
}

/// 一段连续深色模块的绘制矩形，坐标是画布绝对坐标（已含画布自身原点）；绘制与测试
/// 共用同一几何来源，避免遍历范围与静区偏移再次脱节。
/// @author kongweiguang
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct QrRun {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
}

impl<'a> QrLayout<'a> {
    /// 含四周静区的绘制模块总数；绘制必须覆盖到这一范围，否则二维码边缘会被裁掉。
    /// @author kongweiguang
    pub(super) fn total_modules(&self) -> usize {
        self.rows.len() + QR_QUIET_ZONE * 2
    }

    /// 单模块逻辑像素；已按设备像素向下取整，绘制时不会跨半像素。
    /// @author kongweiguang
    pub(super) fn module_px(&self) -> f32 {
        self.module_px
    }

    /// 静区与越界坐标一律视为浅色，绘制循环不需要额外补边分支。
    /// @author kongweiguang
    pub(super) fn is_dark(&self, column: usize, row: usize) -> bool {
        if column < QR_QUIET_ZONE || row < QR_QUIET_ZONE {
            return false;
        }
        self.rows
            .get(row - QR_QUIET_ZONE)
            .and_then(|line| line.as_bytes().get(column - QR_QUIET_ZONE))
            == Some(&b'1')
    }

    /// 按行合并连续深色模块并回调输出，避免每帧为最多 185 行分配中间集合。
    ///
    /// 合并只对成片深色有效：真实二维码常见整行/整列黑边，四边形数远小于模块数；但
    /// 交替散点矩阵最坏仍是每行约一半模块成段，总量为 O(边长²)，版本 40 约 1.7 万个
    /// 四边形/帧。这一上界是已知且未做性能验收的，不能用「行数量级」描述；若实测帧
    /// 时间不达标，应在此处按 GPUI 真实 API 做批量几何（如单条填充路径）或缓存几何，
    /// 而不是削减绘制范围。输出坐标是画布绝对坐标，调用方不再自行叠加原点。
    /// @author kongweiguang
    pub(super) fn for_each_run(&self, mut emit: impl FnMut(QrRun)) {
        let total = self.total_modules();
        for row in 0..total {
            let mut column = 0;
            while column < total {
                if !self.is_dark(column, row) {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < total && self.is_dark(column, row) {
                    column += 1;
                }
                emit(QrRun {
                    x: self.origin_x + start as f32 * self.module_px,
                    y: self.origin_y + row as f32 * self.module_px,
                    width: (column - start) as f32 * self.module_px,
                });
            }
        }
    }
}

/// 测试专用几何读取入口放在 `tests/unit`：子模块可以访问父模块私有字段，
/// `src` 只保留条件编译的子模块引用，避免为测试暴露生产 API。
#[cfg(test)]
#[path = "../../../tests/unit/plugins_external_layout_test_support.rs"]
mod test_support;

/// 计算可绘制的二维码区域；非法矩阵、空间不足或无法严格对齐设备像素时返回空。
///
/// 绘制的最小单位是一个设备像素：模块宽必须是整数设备像素，原点也要落在整数设备像素
/// 上，否则居中和非整数 DPR 产生的分数偏移会让模块边界落到不同舍入结果，出现宽度不均
/// 和半像素灰边。这里先按较短边取理论最大整数模块，再按真实画布原点求整数原点；放不下
/// 才缩小一级模块重试，因此画布原点已对齐时保持最大模块，只有「原点不在设备像素网格上
/// 且余量不足一个设备像素」时才牺牲一级，不会为了对齐把码缩得过小。
///
/// 容器查询阶段只拿得到尺寸、拿不到绝对原点，只能按零原点预判。模块已缩到 1 个设备像素
/// 时没有继续缩小的余地：只要任一轴余量不足 1 个设备像素，画布一旦落在分数原点上绘制
/// 将无处落格，会留下空白画布却仍显示标签。这种「预判通过但绘制必失败」的组合一律按
/// 空间不足处理；其余情况绘制阶段最多缩一级即可保证成功，不会静默丢图，也不会画出错位、
/// 半像素或截断的码冒充成功。
/// @author kongweiguang
pub(super) fn qr_layout<'a>(
    matrix: &'a str,
    bounds: Bounds<Pixels>,
    scale: f32,
) -> Option<QrLayout<'a>> {
    validate_qr_matrix(matrix).ok()?;
    let rows = matrix.lines().collect::<Vec<_>>();
    let total = rows.len() + QR_QUIET_ZONE * 2;
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let origin_x = bounds.origin.x.as_f32() * scale;
    let origin_y = bounds.origin.y.as_f32() * scale;
    let width_dev = bounds.size.width.as_f32() * scale;
    let height_dev = bounds.size.height.as_f32() * scale;
    if !origin_x.is_finite()
        || !origin_y.is_finite()
        || !width_dev.is_finite()
        || !height_dev.is_finite()
    {
        return None;
    }
    let total_dev = total as f32;
    let mut device_module = (width_dev.min(height_dev) / total_dev).floor();
    if device_module < 1.0 {
        return None;
    }
    // 模块已是最小值 1 时，预判阶段看不到真实分数原点：只要任一轴余量不足一个设备
    // 像素，绘制阶段就可能无处落格。用 `<=` 而不是浮点相等比较，避免边界写法差异。
    if device_module <= 1.0 && (width_dev - total_dev < 1.0 || height_dev - total_dev < 1.0) {
        return None;
    }
    loop {
        let footprint = device_module * total_dev;
        if let (Some(x), Some(y)) = (
            aligned_origin(origin_x, width_dev - footprint),
            aligned_origin(origin_y, height_dev - footprint),
        ) {
            return Some(QrLayout {
                rows,
                module_px: device_module / scale,
                origin_x: x / scale,
                origin_y: y / scale,
            });
        }
        // 缩小一级后每轴余量至少增加一个完整边长（≥ 29 个设备像素），必定存在整数原点；
        // 保留循环只为不依赖该算术上界，正常最多回退一次。
        if device_module <= 1.0 {
            return None;
        }
        device_module -= 1.0;
    }
}

/// 在画布占据的设备像素区间内，取最接近居中的整数设备像素原点。
///
/// 下界向上取整保证不越过画布左/上边缘，上界向下取整保证右/下边缘不越出画布；区间为空
/// 说明当前模块宽在真实原点下放不下，返回 `None` 交给调用方缩小模块，而不是退回分数
/// 原点画出灰边。两轴分别求解：非正方形画布上较短边决定模块宽，较长边只影响该轴的
/// 居中余量。
/// @author kongweiguang
fn aligned_origin(origin: f32, slack: f32) -> Option<f32> {
    let low = origin.ceil();
    let high = (origin + slack).floor();
    if !low.is_finite() || !high.is_finite() || low > high {
        return None;
    }
    Some((origin + slack * 0.5).round().clamp(low, high))
}

/// 只绘制已验证的 0/1 方阵：浅底深模块保证扫码对比，模块来自插件编码而非宿主依赖。
/// 函数内重新校验而非信任渲染分支，避免绕过 `qr_layout` 后把任意文本当作图形解释。
/// @author kongweiguang
fn paint_qr(matrix: &str, area: Bounds<Pixels>, window: &mut Window) {
    let Some(layout) = qr_layout(matrix, area, window.scale_factor()) else {
        return;
    };
    window.paint_quad(fill(area, white()));
    // 连续深色模块合并为一次绘制，减少四边形数量；几何由 QrLayout 统一给出，绘制与
    // 回归测试共享同一坐标来源，避免遍历范围与静区偏移再次脱节。
    let module = px(layout.module_px());
    layout.for_each_run(|run| {
        // 绘制段已是画布绝对坐标（含 area.origin），这里不能再叠加一次原点。
        let origin = point(px(run.x), px(run.y));
        window.paint_quad(fill(
            Bounds::new(origin, size(px(run.width), module)),
            black(),
        ));
    });
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
    match kerminal_plugin_protocol::classify_icon_reference(name).ok()? {
        kerminal_plugin_protocol::IconReference::PackagePath => {
            // 路径语义仍限定为 SVG：现有清单和按钮/空态都只承诺这一种可渲染格式，
            // 其他扩展名即使包内存在，也不能交给 GPUI 的 SVG 加载器。
            if !name.ends_with(".svg") {
                return None;
            }
            resources
                .get(name)
                .map(|bytes| PluginIconSource::Package(bytes.as_ref()))
        }
        kerminal_plugin_protocol::IconReference::Builtin => {
            super::super::icon_names::builtin_icon_name(name).map(PluginIconSource::Builtin)
        }
    }
}

/// 展示层只消费内存中的确认结果，不从插件传入的名称读取文件或网络。
fn resolve_icon(prepared: &PreparedPlugin, name: &str) -> Option<Icon> {
    match icon_source(&prepared.resources, name)? {
        PluginIconSource::Builtin(icon) => Some(Icon::new(icon)),
        PluginIconSource::Package(bytes) => Some(Icon::default().data(bytes)),
    }
}
