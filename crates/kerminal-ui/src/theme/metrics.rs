//! 密度只改变呈现度量，窗口 rem 同时缩放自绘区域和 Kit 控件。
//!
//! @author kongweiguang

use gpui_kit::{App, Global, Pixels, px};
use kerminal_application::preferences::InterfaceDensity;

/// 当前已确认的密度是呈现偏好，不承载设置事务或业务状态。
#[derive(Clone, Copy)]
pub(crate) struct ActiveInterfaceDensity(pub(crate) InterfaceDensity);

impl Global for ActiveInterfaceDensity {}

/// 同类区域共享尺寸，控件仍保留 Kit 的焦点、命中与状态行为。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct UiMetrics {
    pub(crate) panel_padding: Pixels,
    pub(crate) control_height: Pixels,
    pub(crate) toolbar_height: Pixels,
    pub(crate) row_height: Pixels,
    pub(crate) icon_size: Pixels,
    pub(crate) gap: Pixels,
    pub(crate) section_gap: Pixels,
    pub(crate) dialog_padding: Pixels,
}

impl UiMetrics {
    /// 以默认 16px rem 为设计基准；放大文字时同步留白和命中区，避免只放大字形后裁切。
    /// 对话框保持适当留白，密度变化主要服务高频工具栏与列表，不扩大表单的固定宽度。
    pub(crate) fn resolve(density: InterfaceDensity, rem_size: Pixels) -> Self {
        let scale = rem_size.as_f32() / 16.;
        let (padding, control, toolbar, row, gap, section) = match density {
            InterfaceDensity::Compact => (12., 32., 40., 32., 8., 16.),
            InterfaceDensity::Comfortable => (16., 36., 44., 36., 12., 20.),
            InterfaceDensity::Spacious => (20., 40., 48., 40., 16., 24.),
        };
        Self {
            panel_padding: px(padding * scale),
            control_height: px(control * scale),
            toolbar_height: px(toolbar * scale),
            row_height: px(row * scale),
            icon_size: px(16. * scale),
            gap: px(gap * scale),
            section_gap: px(section * scale),
            dialog_padding: px(24. * scale),
        }
    }

    /// 首次设置尚未读取时采用产品紧凑缺省值，之后只消费 Root 已确认的密度。
    pub(super) fn current(rem_size: Pixels, cx: &App) -> Self {
        let density = cx
            .try_global::<ActiveInterfaceDensity>()
            .map(|density| density.0)
            .unwrap_or(InterfaceDensity::Compact);
        Self::resolve(density, rem_size)
    }
}
