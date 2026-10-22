//! 主机编辑器共享尺寸和响应式字段；把缩放与窄窗规则集中，避免每个协议各自修补像素。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, App, IntoElement, Pixels, RenderOnce, SharedString, Size, Window, div, prelude::*,
    px, rems, size,
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
        }
    }
}

impl RenderOnce for EditorField {
    /// 使用相对尺寸保持缩放下的节奏；多行字段顶对齐，普通字段沿同一控件中线排列。
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let compact = compact_editor(window);
        let label = div()
            .text_sm()
            .text_color(self.theme.muted)
            .when(!compact, |label| label.w(rems(6.0)).flex_shrink_0())
            .when(!compact && self.multiline, |label| label.pt_2())
            .child(self.label);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .when(compact, |row| row.flex_col().items_stretch().gap_1())
            .when(!compact, |row| row.gap_3().min_h(rems(2.25)))
            .when(!compact && self.multiline, |row| row.items_start())
            .when(!compact && !self.multiline, |row| row.items_center())
            .child(label)
            .child(div().flex_1().min_w_0().child(self.input))
    }
}
