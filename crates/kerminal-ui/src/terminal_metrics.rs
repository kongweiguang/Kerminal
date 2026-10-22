//! 终端绘制、网格尺寸与输入法定位共享的度量来源。
//!
//! @author kongweiguang

use std::sync::{Arc, OnceLock};

use gpui_kit::{FontWeight, Pixels, Point, SharedString, Window, point, px};
use kerminal_application::preferences::{
    TerminalCursorStyle, TerminalFontWeight, TerminalSettings,
};
use kerminal_terminal::snapshot::TerminalCursorShape;

use crate::features::terminal_tools::selection::ViewportCell;

/// 保存设置解析后的字体度量，避免 renderer、PTY resize 与 IME 候选框分别计算参数。
#[derive(Clone, Debug)]
pub struct TerminalMetrics {
    /// GPUI 接受单个字体族；CSS 候选列表按顺序选择首个已安装字体。
    pub font_family: SharedString,
    /// 字体点阵大小以 UI 像素解释，且范围受限以防损坏配置压垮网格布局。
    pub font_size: f32,
    /// 设置中的 lineHeight 是相对字体大小的倍率，转成绝对行高供所有入口共用。
    pub line_height: f32,
    /// 普通文字字重与终端粗体属性分开处理，匹配旧版 fontWeight/fontWeightBold 契约。
    pub font_weight: TerminalFontWeight,
    /// 应用默认光标外观；显式 DECSCUSR 的优先级由快照来源状态决定。
    pub cursor_style: TerminalCursorStyle,
    /// 用户关闭闪烁时优先隐藏闪烁相位，但仍保留终端明确选择的闪烁信息。
    pub cursor_blink: bool,
    /// 绘制边距在 canvas、resize 与鼠标坐标中保持一致。
    pub inset: f32,
    /// 相同设置的克隆共享测量结果；设置更新会创建新实例，使字体变化自然失效。
    cell_width_cache: Arc<OnceLock<f32>>,
}

impl TerminalMetrics {
    /// 按 CSS 候选顺序选择 GPUI 已安装字体；通用 monospace 映射到已安装的平台等宽族，供绘制和测量共用。
    ///
    /// @author kongweiguang
    pub fn from_settings(settings: &TerminalSettings, installed_font_families: &[String]) -> Self {
        let font_family = resolve_font_family(&settings.font_family, installed_font_families);
        let font_size = f32::from(settings.font_size.clamp(8, 72));
        let line_height =
            (font_size * settings.line_height as f32).clamp(font_size, font_size * 3.0);

        Self {
            font_family: SharedString::from(font_family),
            font_size,
            line_height,
            font_weight: settings.font_weight,
            cursor_style: settings.cursor_style,
            cursor_blink: settings.cursor_blink,
            inset: 12.0,
            cell_width_cache: Arc::new(OnceLock::new()),
        }
    }

    /// 用窗口文本系统测量固定宽度字形一次，后续渲染、resize、IME 与鼠标共享结果。
    ///
    /// @author kongweiguang
    pub fn cell_width(&self, window: &mut Window) -> Pixels {
        let width = self.cell_width_cache.get_or_init(|| {
            let mut style = window.text_style();
            style.font_family = self.font_family.clone();
            style.font_size = px(self.font_size).into();
            style.line_height = px(self.line_height).into();
            style.font_weight = font_weight(self.font_weight);
            window
                .text_system()
                .shape_line("M".into(), px(self.font_size), &[style.to_run(1)], None)
                .width()
                .as_f32()
                .max(1.0)
        });
        px(*width)
    }

    /// 将单元格位置换算成窗口坐标；所有入口都在同一处应用终端内边距。
    ///
    /// @author kongweiguang
    pub fn cell_origin(
        &self,
        element_origin: Point<Pixels>,
        row: u16,
        column: u16,
        cell_width: Pixels,
    ) -> Point<Pixels> {
        point(
            element_origin.x + px(self.inset) + cell_width * usize::from(column),
            element_origin.y + px(self.inset) + px(self.line_height) * usize::from(row),
        )
    }

    /// 把窗口指针坐标映射为视口 cell；负坐标和无效度量返回 None，由调用方按网格大小裁剪。
    ///
    /// @author kongweiguang
    pub fn point_to_cell(
        &self,
        element_origin: Point<Pixels>,
        position: Point<Pixels>,
        cell_width: Pixels,
    ) -> Option<ViewportCell> {
        let width = cell_width.as_f32();
        let height = self.line_height;
        let local_x = (position.x - element_origin.x).as_f32() - self.inset;
        let local_y = (position.y - element_origin.y).as_f32() - self.inset;
        if !width.is_finite()
            || width <= 0.0
            || !height.is_finite()
            || height <= 0.0
            || !local_x.is_finite()
            || !local_y.is_finite()
            || local_x < 0.0
            || local_y < 0.0
        {
            return None;
        }

        let column = (local_x / width).floor();
        let row = (local_y / height).floor();
        if column > f32::from(u16::MAX) || row > f32::from(u16::MAX) {
            return None;
        }
        Some(ViewportCell {
            row: row as u16,
            column: column as u16,
        })
    }
}

/// 把设置的普通字重换成 GPUI 权重；粗体 ANSI 属性会在具体 TextRun 再覆盖为 700。
///
/// @author kongweiguang
pub(crate) fn font_weight(value: TerminalFontWeight) -> FontWeight {
    let weight = match value {
        TerminalFontWeight::Normal => 400.0,
        TerminalFontWeight::Medium => 500.0,
        TerminalFontWeight::Bold => 600.0,
    };
    FontWeight::from(weight)
}

/// 仅在终端未发送 DECSCUSR 时采用应用光标默认值，避免设置覆盖 shell 显式选择。
///
/// @author kongweiguang
pub(crate) fn effective_cursor_shape(
    configured: TerminalCursorStyle,
    terminal: TerminalCursorShape,
    style_from_terminal: bool,
) -> TerminalCursorShape {
    if style_from_terminal {
        return terminal;
    }
    match configured {
        TerminalCursorStyle::Block => TerminalCursorShape::Block,
        TerminalCursorStyle::Bar => TerminalCursorShape::Beam,
        TerminalCursorStyle::Underline => TerminalCursorShape::Underline,
    }
}

/// 先用 GPUI 返回的实际字体族解析配置；系统没有已知等宽字体时才退回通用族并记录限制。
///
/// @author kongweiguang
fn resolve_font_family(configured: &str, installed_font_families: &[String]) -> String {
    let find_installed = |candidate: &str| {
        installed_font_families
            .iter()
            .find(|family| family.eq_ignore_ascii_case(candidate))
            .cloned()
    };

    for candidate in configured.split(',') {
        let candidate = candidate
            .trim()
            .trim_matches(|character| character == '"' || character == '\'')
            .trim();
        if candidate.is_empty() {
            continue;
        }
        if candidate.eq_ignore_ascii_case("monospace") {
            if let Some(family) = installed_monospace_fallback(installed_font_families) {
                return family;
            }
        } else if let Some(family) = find_installed(candidate) {
            return family;
        }
    }

    if let Some(family) = installed_monospace_fallback(installed_font_families) {
        return family;
    }

    tracing::warn!(
        "GPUI reported no known installed monospace family; using the generic monospace family as a last resort"
    );
    "monospace".to_owned()
}

/// 仅从 GPUI 实际列出的字体族中选平台等宽候选，避免未安装的系统名字再次触发比例字体回退。
///
/// @author kongweiguang
fn installed_monospace_fallback(installed_font_families: &[String]) -> Option<String> {
    platform_monospace_fallbacks().iter().find_map(|candidate| {
        installed_font_families
            .iter()
            .find(|family| family.eq_ignore_ascii_case(candidate))
            .cloned()
    })
}

/// 列出各平台常见等宽字体的优先顺序；调用者仍须通过 GPUI 字体清单确认已安装。
///
/// @author kongweiguang
fn platform_monospace_fallbacks() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["Cascadia Code", "Cascadia Mono", "Consolas", "Courier New"]
    }
    #[cfg(target_os = "macos")]
    {
        &["Menlo", "SF Mono", "Monaco"]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[
            "Ubuntu Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Noto Sans Mono",
        ]
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        &[
            "Cascadia Code",
            "Consolas",
            "Menlo",
            "DejaVu Sans Mono",
            "Liberation Mono",
        ]
    }
}
