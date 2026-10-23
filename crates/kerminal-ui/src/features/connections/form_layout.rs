//! 主机编辑器共享尺寸和响应式字段；把缩放与窄窗规则集中，避免每个协议各自修补像素。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, App, Bounds, Context, FocusHandle, IntoElement, Pixels, RenderOnce, ScrollHandle,
    SharedString, Size, Subscription, Window, div, point, prelude::*, px, rems, size,
};

use crate::theme::ThemeTokens;

const EDITOR_WIDTH_REM: f32 = 64.0;
const EDITOR_HEIGHT_REM: f32 = 46.0;
const COMPACT_WINDOW_WIDTH_REM: f32 = 54.0;

/// 外边距和弹窗随同一 rem 缩放；实际窗口较小时收缩正文，不让 footer 离开可见范围。
pub(crate) fn host_editor_size(window: &Window) -> Size<Pixels> {
    host_editor_dimensions(window.viewport_size(), window.rem_size())
}

/// 纯尺寸运算可验证极窄窗口与缩放边界，不需要创建系统窗口或依赖特定 DPI。
fn host_editor_dimensions(viewport: Size<Pixels>, base: Pixels) -> Size<Pixels> {
    size(
        (viewport.width - base * 2.0)
            .max(px(0.0))
            .min(base * EDITOR_WIDTH_REM),
        (viewport.height - base * 2.0)
            .max(px(0.0))
            .min(base * EDITOR_HEIGHT_REM),
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_layout.rs"]
mod tests;

/// 根据可用窗口与界面缩放决定表单形态，避免只按物理宽度判断而在大字体下挤压字段。
pub(super) fn compact_editor(window: &Window) -> bool {
    window.viewport_size().width < window.rem_size() * COMPACT_WINDOW_WIDTH_REM
}

/// 字段保留值式渲染，仅由窗口决定标签方向；输入实体仍归原编辑器持有。
#[derive(IntoElement)]
pub(super) struct EditorField {
    label: SharedString,
    input: AnyElement,
    theme: ThemeTokens,
    multiline: bool,
    reveal: Option<(ScrollHandle, SharedString)>,
}

impl EditorField {
    /// 标签与控件作为一组进入布局，让不同协议复用相同对齐轴及窄窗退化规则。
    pub(super) fn new(
        label: impl Into<SharedString>,
        input: impl IntoElement,
        theme: ThemeTokens,
        multiline: bool,
    ) -> Self {
        Self {
            label: label.into(),
            input: input.into_any_element(),
            theme,
            multiline,
            reveal: None,
        }
    }

    /// 每行持有自己的焦点范围，确保 Tab 到屏外字段时只滚动该字段所属正文。
    /// @author kongweiguang
    pub(super) fn reveal_in(
        mut self,
        scroll: ScrollHandle,
        identity: impl Into<SharedString>,
    ) -> Self {
        self.reveal = Some((scroll, identity.into()));
        self
    }
}

impl RenderOnce for EditorField {
    /// 使用相对尺寸保持缩放下的节奏；多行字段顶对齐，普通字段沿同一控件中线排列。
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let compact = compact_editor(window);
        let label = div()
            .text_sm()
            .text_color(self.theme.muted)
            .when(!compact, |label| label.w(rems(6.0)).flex_shrink_0())
            .when(!compact && self.multiline, |label| label.pt_2())
            .child(self.label);
        let row = div()
            .w_full()
            .min_w_0()
            .flex()
            .when(compact, |row| row.flex_col().items_stretch().gap_1())
            .when(!compact, |row| row.gap_3().min_h(rems(2.25)))
            .when(!compact && self.multiline, |row| row.items_start())
            .when(!compact && !self.multiline, |row| row.items_center())
            .child(label)
            .child(div().flex_1().min_w_0().child(self.input));
        match self.reveal {
            Some((scroll, identity)) => {
                EditorFocusReveal::new(row, scroll, identity).into_any_element()
            }
            None => row.into_any_element(),
        }
    }
}

/// 复用同一焦点范围机制包裹没有标签列的按钮行，例如传输开关与串口设备候选。
/// @author kongweiguang
#[derive(IntoElement)]
pub(super) struct EditorFocusReveal {
    content: AnyElement,
    scroll: ScrollHandle,
    identity: SharedString,
    debug_selector: Option<SharedString>,
}

impl EditorFocusReveal {
    /// 显式接收正文滚动句柄和稳定身份，避免跨编辑器复用焦点订阅。
    /// @author kongweiguang
    pub(super) fn new(
        content: impl IntoElement,
        scroll: ScrollHandle,
        identity: impl Into<SharedString>,
    ) -> Self {
        Self {
            content: content.into_any_element(),
            scroll,
            identity: identity.into(),
            debug_selector: None,
        }
    }

    /// 提供稳定的可观测行标识，生产结构不另建测试专用状态接口。
    /// @author kongweiguang
    pub(super) fn debug_selector(mut self, selector: impl Into<SharedString>) -> Self {
        self.debug_selector = Some(selector.into());
        self
    }
}

/// 一份编辑器作用域显式携带正文句柄，字段 key 再叠加协议项或跳板的稳定身份。
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct EditorFieldScope {
    scroll: ScrollHandle,
    prefix: SharedString,
}

impl EditorFieldScope {
    /// 每个编辑器实例使用独立前缀，避免 keyed focus state 跨弹窗或跳板复用。
    /// @author kongweiguang
    pub(super) fn new(scroll: ScrollHandle, prefix: impl Into<SharedString>) -> Self {
        Self {
            scroll,
            prefix: prefix.into(),
        }
    }

    /// label 仅作同作用域行区分；重复控件需由调用者提供实体 ID 作为额外 scope。
    /// @author kongweiguang
    pub(super) fn identity(&self, field: impl AsRef<str>) -> SharedString {
        format!("{}:{}", self.prefix, field.as_ref()).into()
    }

    /// 非标签控件也可沿用同一焦点状态，按钮簇通过独立身份成为一个可滚入区域。
    /// @author kongweiguang
    pub(super) fn wrap(
        &self,
        identity: impl AsRef<str>,
        content: impl IntoElement,
    ) -> EditorFocusReveal {
        EditorFocusReveal::new(content, self.scroll.clone(), self.identity(identity))
    }

    /// 为每个表单控件行分配 stable key 与同一正文滚动句柄。
    /// @author kongweiguang
    pub(super) fn scroll(&self) -> ScrollHandle {
        self.scroll.clone()
    }
}

impl RenderOnce for EditorFocusReveal {
    /// keyed state 随行身份复用；仅在焦点行越出正文视口时按真实 bounds 做最小滚动。
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.identity, cx, |window, cx| {
            FieldRevealState::new(self.scroll, window, cx)
        });
        let focus_handle = state.read(cx).focus_handle.clone();
        let measured = state.clone();
        div()
            .track_focus(&focus_handle)
            .when_some(self.debug_selector, |row, selector| {
                row.debug_selector(move || selector.to_string())
            })
            .on_children_prepainted(move |children, _, cx| {
                if let Some(bounds) = children.first().copied() {
                    measured.update(cx, |state, _| state.bounds = Some(bounds));
                }
            })
            .child(self.content)
    }
}

/// 单行订阅只记录所属行 bounds，避免逐个输入实体维护重复生命周期与控件清单。
/// @author kongweiguang
struct FieldRevealState {
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    bounds: Option<Bounds<Pixels>>,
    _focus_subscription: Subscription,
}

impl FieldRevealState {
    /// 首帧测量前 bounds 无效时跳过；后续焦点变化读取最近一帧的真实行位置。
    /// @author kongweiguang
    fn new(scroll: ScrollHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(false);
        let subscription = cx.on_focus_in(&focus_handle, window, |state, window, _| {
            state.reveal_focused_row(window);
        });
        Self {
            focus_handle,
            scroll,
            bounds: None,
            _focus_subscription: subscription,
        }
    }

    /// 保留当前可见位置；越界时最小移动，过高多行字段按顶部对齐且 offset 始终受限。
    /// 焦点回调在绘制中触发，只有 offset 真正改变才将 refresh 延后到下一帧。
    /// @author kongweiguang
    fn reveal_focused_row(&mut self, window: &mut Window) {
        let Some(row) = self.bounds else { return };
        let viewport = self.scroll.bounds();
        if viewport.size.width <= px(0.) || viewport.size.height <= px(0.) {
            return;
        }
        let too_tall = row.size.height > viewport.size.height;
        let delta = if too_tall || row.top() < viewport.top() {
            viewport.top() - row.top()
        } else if row.bottom() > viewport.bottom() {
            viewport.bottom() - row.bottom()
        } else {
            return;
        };
        let offset = self.scroll.offset();
        let next_y = (offset.y + delta)
            .max(-self.scroll.max_offset().y)
            .min(px(0.));
        if next_y != offset.y {
            self.scroll.set_offset(point(offset.x, next_y));
            window.on_next_frame(|window, _| window.refresh());
        }
    }
}
