//! 多个业务入口共用的主机选择弹层与分组列表。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, rc::Rc};

use gpui_kit::{
    AnyElement, App, Context, ElementId, FocusHandle, IntoElement, MouseButton, RenderOnce,
    ScrollHandle, SharedString, StyleRefinement, Window,
    assets::IconName,
    base::{Selectable, TestSupportExt},
    component::{
        Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::{Enter, Input, InputState},
        popover::{Popover, PopoverState},
        spinner::Spinner,
    },
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    workspace::SplitDirection,
};

/// 每行只携带调用方提供的 typed target，通用视图不读取凭据或发起连接。
#[derive(Clone)]
pub(crate) struct HostPickerOption<T> {
    pub(crate) key: SharedString,
    pub(crate) target: T,
    pub(crate) title: SharedString,
    pub(crate) detail: Option<SharedString>,
    pub(crate) group_id: Option<String>,
}

/// 调用方按导航顺序传入分组，列表保持连接目录的排序。
#[derive(Clone)]
pub(crate) struct HostPickerGroup {
    pub(crate) id: String,
    pub(crate) title: SharedString,
}

/// 状态由业务 owner 决定，重试仍由对应 owner 执行。
#[derive(Clone)]
pub(crate) enum HostPickerListMessage {
    Loading(SharedString),
    Error(SharedString),
    Empty(SharedString),
}

/// 多选列表交给业务 owner 的一行身份：分组行展开为该组当前可见的全部主机 key。
///
/// 列表不保存业务勾选集，点击只解释成可应用的主机集合，避免把 SFTP 或标签创建的选择混在一起。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostPickerRowSelection {
    pub(crate) row_key: String,
    pub(crate) host_keys: Vec<String>,
    /// 范围连选找不到旧锚点时，owner 应把当前行设为新锚点。
    pub(crate) adopt_anchor: bool,
}

/// 行勾选与连选回调携带解析后的主机集合；类型别名避免多处重复书写闭包签名。
pub(crate) type HostPickerRowAction = dyn Fn(HostPickerRowSelection, &mut Window, &mut App);

/// “打开所选”回调不携带目标，具体目标由 owner 重新核对。
pub(crate) type HostPickerOpenSelectedAction = dyn Fn(&mut Window, &mut App);

/// 勾选槽位自己的切换回调，必须与行主体动作隔离，避免勾选时同时打开。
pub(crate) type HostPickerToggleAction = dyn Fn(&mut Window, &mut App);

/// 多选模式由 owner 提供当前勾选、锚点和三个动作；列表只负责命中与范围计算。
///
/// @author kongweiguang
#[derive(Clone)]
pub(crate) struct HostPickerMultiSelect {
    pub(crate) selected_keys: BTreeSet<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) on_toggle: Rc<HostPickerRowAction>,
    pub(crate) on_range: Rc<HostPickerRowAction>,
    pub(crate) on_open_selected: Rc<HostPickerOpenSelectedAction>,
}

/// 仅投影 Root 捕获的分屏请求，使相邻方向按钮不会共用错误的弹层状态。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostPickerPopoverRequest {
    pub(crate) tab_id: TabId,
    pub(crate) pane_id: PaneId,
    pub(crate) source_session_id: SessionId,
    pub(crate) direction: SplitDirection,
    pub(crate) before: bool,
    pub(crate) generation: u64,
}

/// 主机选择弹层沿用业务 owner 的 typed list，并由 Kit Popover 管焦点和关闭。
///
/// @author kongweiguang
#[derive(Clone)]
pub(crate) struct HostPickerPopoverProjection {
    pub(crate) request: HostPickerPopoverRequest,
    pub(crate) content: gpui_kit::AnyView,
    pub(crate) focus_handle: FocusHandle,
}

/// 键盘高亮按稳定 option key 保存，搜索变化时安全退回首个可见项。
///
/// @author kongweiguang
pub(crate) struct HostPickerListKeyboardState {
    active_key: Option<String>,
    scroll: ScrollHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPickerKeyAction {
    MoveTo(usize),
    Select,
    Ignore,
}

/// 把按键、当前高亮与 IME 组合状态规约成列表意图，组合输入期间保留方向键和回车给输入法。
///
/// @author kongweiguang
fn host_picker_key_action(
    key: &str,
    current_index: usize,
    option_count: usize,
    is_composing: bool,
) -> HostPickerKeyAction {
    if is_composing || option_count == 0 {
        return HostPickerKeyAction::Ignore;
    }

    let current_index = current_index.min(option_count - 1);
    match key.to_ascii_lowercase().as_str() {
        "down" | "arrowdown" => {
            HostPickerKeyAction::MoveTo((current_index + 1).min(option_count - 1))
        }
        "up" | "arrowup" => HostPickerKeyAction::MoveTo(current_index.saturating_sub(1)),
        "enter" => HostPickerKeyAction::Select,
        _ => HostPickerKeyAction::Ignore,
    }
}

impl Default for HostPickerListKeyboardState {
    /// 新列表从未选择状态开始，滚动句柄只服务当前弹层的可见范围。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            active_key: None,
            scroll: ScrollHandle::new(),
        }
    }
}

type HostPickerSelect<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// 可搜索列表保留目标筛选与业务动作边界，统一复用视觉和键盘交互。
#[derive(IntoElement)]
pub(crate) struct HostPickerList<T: Clone + 'static> {
    id: SharedString,
    search_input: Option<gpui_kit::Entity<InputState>>,
    external_search_input: bool,
    keyboard_state: Option<gpui_kit::Entity<HostPickerListKeyboardState>>,
    query: String,
    groups: Vec<HostPickerGroup>,
    options: Vec<HostPickerOption<T>>,
    message: Option<HostPickerListMessage>,
    footer: Option<AnyElement>,
    theme: Option<ThemeTokens>,
    multi_select: Option<HostPickerMultiSelect>,
    on_select: HostPickerSelect<T>,
}

impl<T: Clone + 'static> HostPickerList<T> {
    /// 接收调用方提供的强类型投影，列表统一本地过滤但不接管业务动作。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        id: impl Into<SharedString>,
        options: Vec<HostPickerOption<T>>,
        on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            search_input: None,
            external_search_input: false,
            keyboard_state: None,
            query: String::new(),
            groups: Vec::new(),
            options,
            message: None,
            footer: None,
            theme: None,
            multi_select: None,
            on_select: Rc::new(on_select),
        }
    }

    /// 复用 owner 持有的输入实体，避免每帧重建焦点与输入法状态。
    ///
    /// @author kongweiguang
    pub(crate) fn search_input(mut self, input: gpui_kit::Entity<InputState>) -> Self {
        self.search_input = Some(input);
        self
    }

    /// 外部搜索框与结果列表共享键盘状态，但列表不重复挂载同一个输入实体。
    ///
    /// @author kongweiguang
    pub(crate) fn external_search_input(
        mut self,
        input: gpui_kit::Entity<InputState>,
        keyboard_state: gpui_kit::Entity<HostPickerListKeyboardState>,
    ) -> Self {
        self.search_input = Some(input);
        self.external_search_input = true;
        self.keyboard_state = Some(keyboard_state);
        self
    }

    /// 列表内部对名称、协议、地址、分组名等全部摘要字段执行统一匹配。
    ///
    /// @author kongweiguang
    pub(crate) fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// 沿用调用方主机目录的分组顺序。
    ///
    /// @author kongweiguang
    pub(crate) fn groups(mut self, groups: Vec<HostPickerGroup>) -> Self {
        self.groups = groups;
        self
    }

    /// 展示等待、失败或空结果状态，恢复动作仍由业务 owner 提供。
    ///
    /// @author kongweiguang
    pub(crate) fn message(mut self, message: HostPickerListMessage) -> Self {
        self.message = Some(message);
        self
    }

    /// 放入 SFTP 新建或重试等由业务 owner 持有的底部操作。
    ///
    /// @author kongweiguang
    pub(crate) fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }

    /// 使用 owner 当前从 Kit theme context 解析的令牌，RenderOnce 无需伪造窗口主题。
    ///
    /// @author kongweiguang
    pub(crate) fn theme(mut self, theme: ThemeTokens) -> Self {
        self.theme = Some(theme);
        self
    }

    /// 打开多选勾选、分组选择与“打开所选”入口；未设置时保持各入口原有的单击打开行为。
    ///
    /// @author kongweiguang
    pub(crate) fn multi_select(mut self, multi_select: HostPickerMultiSelect) -> Self {
        self.multi_select = Some(multi_select);
        self
    }

    /// 保留选项顺序，并将已删除分组安全归入未分组区域。
    ///
    /// @author kongweiguang
    fn grouped_options(&self) -> Vec<(String, SharedString, Vec<HostPickerOption<T>>)> {
        grouped_host_picker_options(&self.options, &self.groups, &self.query)
    }
}

/// 结果列表与外置搜索框共用过滤和分组顺序，避免键盘选中与可见结果错位。
///
/// @author kongweiguang
pub(crate) fn grouped_host_picker_options<T: Clone>(
    all_options: &[HostPickerOption<T>],
    groups: &[HostPickerGroup],
    query: &str,
) -> Vec<(String, SharedString, Vec<HostPickerOption<T>>)> {
    let known_ids = groups
        .iter()
        .map(|group| group.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut result = Vec::new();
    for group in groups {
        let options = all_options
            .iter()
            .filter(|option| {
                option.group_id.as_deref() == Some(group.id.as_str())
                    && option_matches_query(option, query, Some(&group.title))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !options.is_empty() {
            result.push((format!("group:{}", group.id), group.title.clone(), options));
        }
    }
    let ungrouped = all_options
        .iter()
        .filter(|option| {
            option
                .group_id
                .as_deref()
                .is_none_or(|group_id| !known_ids.contains(group_id))
                && option_matches_query(option, query, None)
        })
        .cloned()
        .collect::<Vec<_>>();
    if !ungrouped.is_empty() {
        result.push((
            "ungrouped".to_owned(),
            crate::locale::text("未分组", "Ungrouped"),
            ungrouped,
        ));
    }
    result
}

/// 切换一组主机：整组已选时取消，否则补齐；主机行传入单个 key 即标准勾选切换。
///
/// 分组行与主机行共用同一规则，勾选结果始终落在主机 key 上，打开时无需再判断“目录还是主机”。
///
/// @author kongweiguang
pub(crate) fn toggled_host_keys(
    selected: &BTreeSet<String>,
    host_keys: &[String],
) -> BTreeSet<String> {
    let mut next = selected.clone();
    let all_selected = !host_keys.is_empty() && host_keys.iter().all(|key| selected.contains(key));
    if all_selected {
        for key in host_keys {
            next.remove(key);
        }
    } else {
        for key in host_keys {
            next.insert(key.clone());
        }
    }
    next
}

/// 按可见行顺序计算 Shift 连选；锚点已隐藏时退化为当前行，并让 owner 采用当前行为新锚点。
///
/// 行序列由渲染同帧的过滤结果生成，因此搜索变化后不会把不可见主机偷偷加入选择。
///
/// @author kongweiguang
pub(crate) fn host_picker_range_keys(
    rows: &[(String, Vec<String>)],
    anchor: Option<&str>,
    target_row: &str,
) -> (Vec<String>, bool) {
    let Some(target_index) = rows.iter().position(|(key, _)| key == target_row) else {
        return (Vec::new(), false);
    };
    let anchor_index = anchor.and_then(|anchor| rows.iter().position(|(key, _)| key == anchor));
    let start = anchor_index.unwrap_or(target_index);
    let (low, high) = if start <= target_index {
        (start, target_index)
    } else {
        (target_index, start)
    };
    let mut seen = BTreeSet::new();
    let mut keys = Vec::new();
    for (_, host_keys) in &rows[low..=high] {
        for key in host_keys {
            if seen.insert(key.clone()) {
                keys.push(key.clone());
            }
        }
    }
    (keys, anchor_index.is_none())
}

/// 勾选槽位与行主体共享同一状态：分组部分选中显示减号，避免半选被误读为整组已选。
///
/// 槽位自己截断点击冒泡，行主体的“打开”动作不会在勾选时被意外触发。
///
/// @author kongweiguang
fn host_picker_check_slot(
    id: String,
    checked: bool,
    partial: bool,
    accessible: SharedString,
    theme: ThemeTokens,
    on_toggle: Rc<HostPickerToggleAction>,
) -> AnyElement {
    let icon = if checked {
        IconName::SquareCheck
    } else if partial {
        IconName::SquareMinus
    } else {
        IconName::Square
    };
    let color = if checked || partial {
        theme.accent
    } else {
        theme.subtle
    };
    let debug_id = id.clone();
    div()
        .id(id)
        .debug_selector(move || debug_id.clone())
        .w(px(22.))
        .h(px(22.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .text_color(color)
        .role(gpui_kit::Role::CheckBox)
        .aria_label(accessible)
        .aria_selected(checked || partial)
        .hover(move |style| style.bg(theme.surface_hover()))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_toggle(window, cx);
        })
        .child(icon)
        .into_any_element()
}

/// 外置输入框只拦截列表导航键；选择结果仍由业务 owner 执行。
pub(crate) enum HostPickerExternalKey<T> {
    Ignore,
    Handled,
    Select(T),
}

/// 搜索快照由调用方冻结，键盘选择与当前绘制的结果使用同一数据。
pub(crate) struct HostPickerSearchProjection<'a, T> {
    pub(crate) options: &'a [HostPickerOption<T>],
    pub(crate) groups: &'a [HostPickerGroup],
    pub(crate) query: &'a str,
}

/// 与列表使用同一键盘状态、过滤规则和滚动行号，并保留中文输入法组合按键。
///
/// @author kongweiguang
pub(crate) fn handle_external_host_picker_key<T: Clone>(
    key: &str,
    keyboard_state: &gpui_kit::Entity<HostPickerListKeyboardState>,
    input: &gpui_kit::Entity<InputState>,
    projection: HostPickerSearchProjection<'_, T>,
    has_message: bool,
    cx: &mut App,
) -> HostPickerExternalKey<T> {
    let grouped =
        grouped_host_picker_options(projection.options, projection.groups, projection.query);
    let mut visible = Vec::new();
    let mut scroll_indices = Vec::new();
    let mut child_index = if has_message { 1 } else { 0 };
    for (_, _, options) in grouped {
        child_index += 1;
        for option in options {
            visible.push(option);
            scroll_indices.push(child_index);
            child_index += 1;
        }
    }
    if visible.is_empty() {
        return HostPickerExternalKey::Ignore;
    }
    let current = keyboard_state
        .read(cx)
        .active_key
        .as_deref()
        .and_then(|active| {
            visible
                .iter()
                .position(|option| option.key.as_ref() == active)
        })
        .unwrap_or(0);
    match host_picker_key_action(key, current, visible.len(), input.read(cx).is_composing()) {
        HostPickerKeyAction::Ignore => HostPickerExternalKey::Ignore,
        HostPickerKeyAction::Select => {
            HostPickerExternalKey::Select(visible[current].target.clone())
        }
        HostPickerKeyAction::MoveTo(next) => {
            let key = visible[next].key.to_string();
            keyboard_state.update(cx, |state, cx| {
                state.active_key = Some(key);
                state.scroll.scroll_to_item(scroll_indices[next]);
                cx.notify();
            });
            HostPickerExternalKey::Handled
        }
    }
}

/// 按全部 query 词匹配名称、目标摘要、稳定身份和分组标题，减少不同入口的搜索差异。
///
/// @author kongweiguang
fn option_matches_query<T>(
    option: &HostPickerOption<T>,
    query: &str,
    group_title: Option<&SharedString>,
) -> bool {
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return true;
    }
    let search_text = format!(
        "{} {} {} {}",
        option.key,
        option.title,
        option.detail.as_deref().unwrap_or_default(),
        group_title.map_or("", |title| title.as_ref()),
    )
    .to_lowercase();
    terms.iter().all(|term| search_text.contains(term))
}

impl<T: Clone + 'static> RenderOnce for HostPickerList<T> {
    /// 按活动应用主题渲染紧凑列表；缺省投影也保留用户色板，不从系统外观重建默认配色。
    /// 捕获阶段先处理方向键与回车，避免输入控件的按键处理抢走列表导航。
    /// 行的可访问选中状态与高亮样式共用同一状态，供辅助技术和绘制回归读取。
    /// 已有候选刷新时将忙态留在搜索槽位，避免插入状态行改变键盘滚动索引与可见位置。
    ///
    /// @author kongweiguang
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = self
            .theme
            .unwrap_or_else(|| ThemeTokens::current(window, cx));
        let grouped = self.grouped_options();
        let visible_options = grouped
            .iter()
            .flat_map(|(_, _, options)| options.iter())
            .cloned()
            .collect::<Vec<_>>();
        let multi_select = self.multi_select.clone();
        let selected_keys = multi_select
            .as_ref()
            .map(|multi| multi.selected_keys.clone())
            .unwrap_or_default();
        let selected_count = visible_options
            .iter()
            .filter(|option| selected_keys.contains(option.key.as_ref()))
            .count();
        // 与下方绘制同帧生成可见行序列；搜索过滤后的范围连选不会包含已隐藏主机。
        let mut selection_rows: Vec<(String, Vec<String>)> = Vec::new();
        for (group_key, _, options) in &grouped {
            selection_rows.push((
                group_key.clone(),
                options
                    .iter()
                    .map(|option| option.key.to_string())
                    .collect(),
            ));
            for option in options {
                selection_rows.push((option.key.to_string(), vec![option.key.to_string()]));
            }
        }
        let multi_open_selected = multi_select
            .as_ref()
            .map(|multi| Rc::clone(&multi.on_open_selected));
        let multi_toggle = multi_select
            .as_ref()
            .map(|multi| Rc::clone(&multi.on_toggle));
        let multi_range = multi_select
            .as_ref()
            .map(|multi| Rc::clone(&multi.on_range));
        let multi_anchor = multi_select.as_ref().and_then(|multi| multi.anchor.clone());
        let loading = matches!(
            self.message.as_ref(),
            Some(HostPickerListMessage::Loading(_))
        );
        let inline_loading = loading
            && !self.external_search_input
            && self.search_input.is_some()
            && !visible_options.is_empty();
        let has_message = self.message.is_some() && !inline_loading;
        let keyboard_state = self.keyboard_state.clone().unwrap_or_else(|| {
            window.use_keyed_state(format!("{}-keyboard-state", self.id), cx, |_, _| {
                HostPickerListKeyboardState::default()
            })
        });
        let active_key = keyboard_state
            .read(cx)
            .active_key
            .as_deref()
            .filter(|key| {
                visible_options
                    .iter()
                    .any(|option| option.key.as_ref() == *key)
            })
            .map(ToOwned::to_owned)
            .or_else(|| visible_options.first().map(|option| option.key.to_string()));
        let key_options = visible_options.clone();
        let mut list_child_index = usize::from(has_message);
        let mut key_scroll_indices = Vec::with_capacity(visible_options.len());
        for (_, _, options) in &grouped {
            list_child_index += 1; // 每个真实分组标题也是滚动容器的直接子项。
            for _ in options {
                key_scroll_indices.push(list_child_index);
                list_child_index += 1;
            }
        }
        let key_active = active_key.clone();
        let key_select = self.on_select.clone();
        let key_multi_toggle = multi_toggle.clone();
        let key_state = keyboard_state.clone();
        let key_scroll = keyboard_state.read(cx).scroll.clone();
        let search_input_for_keys = self.search_input.clone();
        let confirm_options = visible_options.clone();
        let confirm_active_key = active_key.clone();
        let confirm_select = self.on_select.clone();
        let confirm_search_input = self.search_input.clone();
        let confirm_open_selected = multi_open_selected.clone();
        let confirm_selected_count = selected_count;
        let viewport = window.viewport_size();
        let max_width = px((viewport.width.as_f32() - 24.0).clamp(0.0, 480.0));
        let max_height = px((viewport.height.as_f32() - 32.0).clamp(0.0, 560.0));
        let composition_input = self.search_input.clone();
        let mut panel = div()
            .id(self.id.clone())
            .key_context(crate::features::input_composition::PICKER_INPUT_CONTEXT)
            .w(px(360.))
            .max_w(max_width)
            .max_h(max_height)
            .min_w(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_color(theme.foreground)
            .capture_action(move |_: &gpui_kit::component::input::Escape, window, cx| {
                if let Some(input) = composition_input.as_ref() {
                    crate::features::input_composition::cancel_input_composition(input, window, cx);
                }
            })
            .capture_action(move |_: &Enter, window, cx| {
                let is_composing = confirm_search_input
                    .as_ref()
                    .is_some_and(|input| input.read(cx).is_composing());
                let selected = confirm_options
                    .iter()
                    .position(|option| {
                        confirm_active_key
                            .as_deref()
                            .is_some_and(|active| option.key.as_ref() == active)
                    })
                    .and_then(|current_index| {
                        match host_picker_key_action(
                            "enter",
                            current_index,
                            confirm_options.len(),
                            is_composing,
                        ) {
                            HostPickerKeyAction::Select => {
                                Some(confirm_options[current_index].target.clone())
                            }
                            HostPickerKeyAction::MoveTo(_) | HostPickerKeyAction::Ignore => None,
                        }
                    });

                if !is_composing {
                    window.prevent_default();
                    // 已勾选多个目标时，Ctrl+Enter 执行与搜索栏右侧按钮相同的批量打开。
                    if window.modifiers().secondary()
                        && confirm_selected_count > 0
                        && let Some(open_selected) = confirm_open_selected.as_ref()
                    {
                        open_selected(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    if let Some(target) = selected {
                        confirm_select(target, window, cx);
                    }
                }
                // 在 Input 的 Enter handler 之前截断动作，避免 PressEnter 与 Popover Confirm 继续传播；
                // IME 组合时不选中目标，并保持弹层打开。
                cx.stop_propagation();
            })
            .capture_key_down(move |event, window, cx| {
                let key = event.keystroke.key.to_ascii_lowercase();
                // Ctrl+Space 勾选键盘高亮的主机；普通空格仍属于搜索输入，不能被列表抢走。
                if event.keystroke.modifiers.secondary()
                    && key == "space"
                    && let Some(toggle) = key_multi_toggle.as_ref()
                    && let Some(option) = key_active.as_deref().and_then(|active| {
                        key_options
                            .iter()
                            .find(|option| option.key.as_ref() == active)
                    })
                {
                    toggle(
                        HostPickerRowSelection {
                            row_key: option.key.to_string(),
                            host_keys: vec![option.key.to_string()],
                            adopt_anchor: true,
                        },
                        window,
                        cx,
                    );
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                let Some(current_index) = key_options.iter().position(|option| {
                    key_active
                        .as_deref()
                        .is_some_and(|active| option.key.as_ref() == active)
                }) else {
                    return;
                };
                let action = host_picker_key_action(
                    &key,
                    current_index,
                    key_options.len(),
                    search_input_for_keys
                        .as_ref()
                        .is_some_and(|input| input.read(cx).is_composing()),
                );
                let next_index = match action {
                    HostPickerKeyAction::MoveTo(next_index) => next_index,
                    HostPickerKeyAction::Select => {
                        key_select(key_options[current_index].target.clone(), window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                        return;
                    }
                    HostPickerKeyAction::Ignore => return,
                };
                let next_key = key_options[next_index].key.to_string();
                key_state.update(cx, |state, cx| {
                    state.active_key = Some(next_key);
                    cx.notify();
                });
                key_scroll.scroll_to_item(key_scroll_indices[next_index]);
                window.prevent_default();
                cx.stop_propagation();
            });

        if !self.external_search_input
            && let Some(search_input) = self.search_input
        {
            let reduce_motion = cx.reduce_motion();
            // 按钮槽位始终存在，勾选数量变化只改变标签与可用状态，搜索行不横向跳动。
            let open_selected_button = multi_open_selected.as_ref().map(|open_selected| {
                let open_selected = Rc::clone(open_selected);
                let count = selected_count;
                let label: SharedString = if count > 0 {
                    SharedString::from(format!("{} {}", crate::locale::text("打开", "Open"), count))
                } else {
                    crate::locale::text("打开", "Open")
                };
                let tooltip = if count > 0 {
                    SharedString::from(format!(
                        "{}{}{}{}",
                        crate::locale::text("一起打开已勾选的 ", "Open the "),
                        count,
                        crate::locale::text(" 个分屏目标", " selected split targets"),
                        crate::locale::text("；Ctrl+Enter 亦可", "; Ctrl+Enter also works"),
                    ))
                } else {
                    crate::locale::text(
                        "先勾选要一起打开的分屏目标",
                        "Select split targets to open together",
                    )
                };
                let mut button = Button::new(format!("{}-open-selected", self.id))
                    .compact()
                    .flex_none()
                    .label(label)
                    .tab_stop(true)
                    .tooltip(tooltip)
                    .disabled(count == 0);
                button = if count > 0 {
                    button.primary()
                } else {
                    button.ghost()
                };
                button.on_click(move |_, window, cx| {
                    if count > 0 {
                        open_selected(window, cx);
                    }
                    window.prevent_default();
                })
            });
            panel = panel.child(
                div()
                    .id(format!("{}-search", self.id))
                    .w_full()
                    .h(theme.metrics.control_height)
                    .mb_2()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(theme.control_radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .text_color(theme.muted)
                    .child(
                        div()
                            .size(theme.metrics.icon_size)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!loading, |slot| slot.child(IconName::Search))
                            .when(loading && reduce_motion, |slot| {
                                slot.child(IconName::LoaderCircle)
                            })
                            .when(loading && !reduce_motion, |slot| {
                                slot.child(Spinner::new().small().color(theme.muted))
                            }),
                    )
                    .child(
                        Input::new(&search_input)
                            .appearance(false)
                            .aria_label(if loading {
                                crate::locale::text(
                                    "搜索主机名称、协议、地址或分组；正在加载主机",
                                    "Search host names, protocols, addresses, or groups; loading hosts",
                                )
                            } else {
                                crate::locale::text(
                                    "搜索主机名称、协议、地址或分组",
                                    "Search host names, protocols, addresses, or groups",
                                )
                            })
                            .flex_1()
                            .min_w(px(0.))
                            .h((theme.metrics.control_height - window.rem_size() * 0.25).max(px(0.)))
                            .text_size(theme.text_sm),
                    )
                    .when_some(open_selected_button, |row, button| row.child(button)),
            );
        }

        let mut list = div()
            .id(format!("{}-options", self.id))
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .gap_1()
            .role(gpui_kit::Role::List)
            .aria_label(crate::locale::text("主机列表", "Host list"))
            .aria_keyshortcuts(if multi_select.is_some() {
                "ArrowUp ArrowDown Enter Control-Enter Control-Space"
            } else {
                "ArrowUp ArrowDown Enter"
            })
            .track_scroll(&keyboard_state.read(cx).scroll);
        if has_message && let Some(message) = self.message {
            let (icon, label, color) = match message {
                HostPickerListMessage::Loading(label) => {
                    (IconName::LoaderCircle, label, theme.muted)
                }
                HostPickerListMessage::Error(label) => (IconName::CircleAlert, label, theme.error),
                HostPickerListMessage::Empty(label) => (IconName::Search, label, theme.muted),
            };
            list = list.child(
                div()
                    .id(format!("{}-message", self.id))
                    .w_full()
                    .min_h(px(52.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .text_color(color)
                    .child(icon)
                    .child(label),
            );
        }
        if grouped.is_empty() && !has_message {
            list = list.child(
                div()
                    .id(format!("{}-empty", self.id))
                    .w_full()
                    .min_h(px(52.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(IconName::Search)
                    .child(crate::locale::text(
                        "没有匹配的主机。",
                        "No hosts match this search.",
                    )),
            );
        }
        for (group_key, title, options) in grouped {
            let group_host_keys = options
                .iter()
                .map(|option| option.key.to_string())
                .collect::<Vec<_>>();
            let group_all_selected = multi_select.as_ref().is_some_and(|multi| {
                !group_host_keys.is_empty()
                    && group_host_keys
                        .iter()
                        .all(|key| multi.selected_keys.contains(key))
            });
            let group_any_selected = multi_select.as_ref().is_some_and(|multi| {
                group_host_keys
                    .iter()
                    .any(|key| multi.selected_keys.contains(key))
            });
            let mut group_row = div()
                .id(format!("{}-group-{group_key}", self.id))
                .w_full()
                .px_2()
                .pt_1()
                .pb_1()
                .flex()
                .items_center()
                .gap_2()
                .rounded(theme.control_radius)
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(div().flex_1().min_w(px(0.)).truncate().child(title.clone()));
            if let Some(multi) = multi_select.as_ref() {
                // 分组行是可勾选的“目录”：点击选中整组，Shift 连选与主机行共用同一范围规则。
                let toggle = Rc::clone(&multi.on_toggle);
                let range = Rc::clone(&multi.on_range);
                let toggle_key = group_key.clone();
                let toggle_keys = group_host_keys.clone();
                let range_key = group_key.clone();
                let range_rows = selection_rows.clone();
                let range_anchor = multi_anchor.clone();
                group_row = group_row
                    .hover(move |style| style.bg(theme.surface_hover()))
                    .role(gpui_kit::Role::ListItem)
                    .aria_label(SharedString::from(format!(
                        "{}{}{}",
                        title,
                        crate::locale::text("，已选 ", ", selected "),
                        group_host_keys
                            .iter()
                            .filter(|key| multi.selected_keys.contains(*key))
                            .count()
                    )))
                    .on_click(move |event, window, cx| {
                        if event.modifiers().shift {
                            let (keys, adopt_anchor) = host_picker_range_keys(
                                &range_rows,
                                range_anchor.as_deref(),
                                &range_key,
                            );
                            range(
                                HostPickerRowSelection {
                                    row_key: range_key.clone(),
                                    host_keys: keys,
                                    adopt_anchor,
                                },
                                window,
                                cx,
                            );
                            return;
                        }
                        toggle(
                            HostPickerRowSelection {
                                row_key: toggle_key.clone(),
                                host_keys: toggle_keys.clone(),
                                adopt_anchor: true,
                            },
                            window,
                            cx,
                        );
                    })
                    .child(host_picker_check_slot(
                        format!("{}-group-check-{group_key}", self.id),
                        group_all_selected,
                        group_any_selected && !group_all_selected,
                        {
                            let label = format!(
                                "{}{}",
                                crate::locale::text("选择分组 ", "Select group "),
                                title
                            );
                            SharedString::from(label)
                        },
                        theme,
                        {
                            let toggle = Rc::clone(&multi.on_toggle);
                            let toggle = move |window: &mut Window, cx: &mut App| {
                                toggle(
                                    HostPickerRowSelection {
                                        row_key: group_key.clone(),
                                        host_keys: group_host_keys.clone(),
                                        adopt_anchor: true,
                                    },
                                    window,
                                    cx,
                                );
                            };
                            Rc::new(toggle)
                        },
                    ));
            }
            list = list.child(group_row);
            for option in options {
                let option_selector = format!("{}-option-{}", self.id, option.key);
                let option_debug_selector = option_selector.clone();
                let target = option.target.clone();
                let click_select = self.on_select.clone();
                let click_state = keyboard_state.clone();
                let option_selected = selected_keys.contains(option.key.as_ref());
                let any_selected = !selected_keys.is_empty();
                let click_toggle = multi_toggle.clone();
                let click_range = multi_range.clone();
                let click_range_rows = selection_rows.clone();
                let click_range_anchor = multi_anchor.clone();
                let row_key = option.key.to_string();
                let toggle_row_key = row_key.clone();
                let is_active = active_key
                    .as_deref()
                    .is_some_and(|active| option.key.as_ref() == active);
                let accessible = option.detail.as_ref().map_or_else(
                    || option.title.to_string(),
                    |detail| format!("{}, {detail}", option.title),
                );
                let mut description = div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(option.title.clone()),
                    );
                if let Some(detail) = option.detail.clone() {
                    description = description.child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(detail),
                    );
                }
                let mut row = div()
                    .id(option_selector)
                    .debug_selector(move || option_debug_selector.clone())
                    .w_full()
                    .min_w(px(0.))
                    .min_h(theme.metrics.row_height + window.rem_size() * 0.875)
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(theme.control_radius)
                    .border_1()
                    .border_color(theme.panel.alpha(0.))
                    .bg(theme.panel)
                    .hover(move |style| {
                        style.bg(if is_active || option_selected {
                            theme.surface_selected()
                        } else {
                            theme.surface_hover()
                        })
                    })
                    .role(gpui_kit::Role::ListItem)
                    .aria_label(accessible)
                    .aria_selected(is_active || option_selected)
                    .on_click(move |event, window, cx| {
                        let active = toggle_row_key.clone();
                        click_state.update(cx, |state, cx| {
                            state.active_key = Some(active);
                            cx.notify();
                        });
                        if let Some(toggle) = click_toggle.as_ref() {
                            if event.modifiers().shift {
                                let (keys, adopt_anchor) = host_picker_range_keys(
                                    &click_range_rows,
                                    click_range_anchor.as_deref(),
                                    &toggle_row_key,
                                );
                                if let Some(range) = click_range.as_ref() {
                                    range(
                                        HostPickerRowSelection {
                                            row_key: toggle_row_key.clone(),
                                            host_keys: keys,
                                            adopt_anchor,
                                        },
                                        window,
                                        cx,
                                    );
                                }
                                return;
                            }
                            // 已有勾选后单击行等同于勾选切换，避免用户以为在“多开”却只打开了当前行。
                            if event.modifiers().secondary() || any_selected {
                                toggle(
                                    HostPickerRowSelection {
                                        row_key: toggle_row_key.clone(),
                                        host_keys: vec![toggle_row_key.clone()],
                                        adopt_anchor: true,
                                    },
                                    window,
                                    cx,
                                );
                                return;
                            }
                        }
                        click_select(target.clone(), window, cx);
                    })
                    .when(is_active || option_selected, |row| {
                        row.bg(theme.surface_selected())
                    })
                    .child(description);
                if let Some(multi) = multi_select.as_ref() {
                    let check_key = row_key.clone();
                    let toggle = Rc::clone(&multi.on_toggle);
                    row = row.child(host_picker_check_slot(
                        format!("{}-check-{}", self.id, option.key),
                        option_selected,
                        false,
                        SharedString::from(format!(
                            "{}{}",
                            crate::locale::text("勾选 ", "Select "),
                            option.title
                        )),
                        theme,
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            toggle(
                                HostPickerRowSelection {
                                    row_key: check_key.clone(),
                                    host_keys: vec![check_key.clone()],
                                    adopt_anchor: true,
                                },
                                window,
                                cx,
                            );
                        }),
                    ));
                } else {
                    row = row.child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.subtle)
                            .child(IconName::ArrowRight),
                    );
                }
                list = list.child(row.test_support());
            }
        }
        panel = panel.child(list);
        if let Some(footer) = self.footer {
            panel = panel.child(
                div()
                    .w_full()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(footer),
            );
        }
        panel
    }
}

/// 一个 picker 弹层的只读装配参数；将触发器几何、焦点和内容绑定到同一弹层请求。
///
/// @author kongweiguang
pub(crate) struct HostPickerPopoverConfig<T, F, C> {
    pub(crate) id: ElementId,
    pub(crate) trigger: T,
    pub(crate) mouse_button: Option<MouseButton>,
    pub(crate) open: bool,
    pub(crate) trigger_style: StyleRefinement,
    pub(crate) focus_handle: Option<FocusHandle>,
    pub(crate) on_open_change: F,
    pub(crate) content: C,
}

/// 通过独立 trigger/content 锚定统一弹层，避免上下文菜单给图标按钮注入子节点改变尺寸。
/// 调用方提供固定触发器样式，使弹层开关前后的锚点几何保持一致。
///
/// @author kongweiguang
pub(crate) fn host_picker_popover<T, F, C, E>(config: HostPickerPopoverConfig<T, F, C>) -> Popover
where
    T: Selectable + IntoElement + 'static,
    F: Fn(&bool, &mut Window, &mut App) + 'static,
    C: Fn(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> E + 'static,
    E: IntoElement,
{
    let HostPickerPopoverConfig {
        id,
        trigger,
        mouse_button,
        open,
        trigger_style,
        focus_handle,
        on_open_change,
        content,
    } = config;
    let popover = Popover::new(id)
        .trigger(trigger)
        .trigger_on_mouse_down(mouse_button.is_some())
        .when_some(mouse_button, |popover, button| popover.mouse_button(button))
        .open(open)
        .trigger_style(trigger_style)
        .overlay_closable(true)
        .on_open_change(on_open_change)
        .offset(px(6.))
        .content(content);
    if let Some(focus_handle) = focus_handle {
        popover.track_focus(&focus_handle)
    } else {
        popover
    }
}

#[cfg(test)]
#[path = "../../tests/unit/host_picker.rs"]
mod tests;
