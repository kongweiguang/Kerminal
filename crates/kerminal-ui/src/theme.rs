//! 原生工作区与终端画布共用的语义颜色令牌。
//!
//! @author kongweiguang

use std::borrow::BorrowMut;

use gpui_kit::component::{Theme as KitTheme, ThemeMode as KitThemeMode};
use gpui_kit::{App, Context, Global, Hsla, Pixels, Window, WindowAppearance, px};
use kerminal_application::preferences::{AppColorScheme, ThemeMode};

mod kit;
mod materials;
mod palette;
mod terminal;

pub(crate) use materials::{BackgroundMaterialSettings, BackgroundMaterials};
pub(crate) use terminal::{terminal_background_color, terminal_palette_for_mode};

/// 仅缓存当前选择的色板身份；明暗和材质仍从各自的既有状态解析。
#[derive(Clone, Copy, Default)]
struct ActiveAppColorScheme(AppColorScheme);

impl Global for ActiveAppColorScheme {}

/// 浅色、深色和跟随系统的界面都从同一组语义令牌取色。
#[derive(Clone, Copy)]
pub(crate) struct ThemeTokens {
    pub background: Hsla,
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub tool_panel_surface: Hsla,
    pub tool_panel_floating_surface: Hsla,
    pub tool_panel_border: Hsla,
    pub secondary: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    /// 表单字段标签使用当前色板的次级文字角色。
    pub field_label: Hsla,
    pub subtle: Hsla,
    pub accent: Hsla,
    pub primary: Hsla,
    pub primary_hover: Hsla,
    pub primary_foreground: Hsla,
    pub tunnel_action: Hsla,
    pub tunnel_action_hover: Hsla,
    /// 健康点沿用当前色板的成功、警告与错误语义色。
    pub monitor_live: Hsla,
    pub monitor_paused: Hsla,
    pub monitor_error: Hsla,
    pub selected: Hsla,
    pub selected_foreground: Hsla,
    pub hover: Hsla,
    pub border: Hsla,
    pub terminal_background: Hsla,
    pub error: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub scrim: Hsla,
    /// 与 main ModalShell 对齐，避免将抽屉遮罩与决策弹框的层级混用。
    pub dialog_scrim: Hsla,
    /// 对应 main 的 ModalShell 与 floating-surface，颜色和滤镜参数集中管理。
    pub floating_surface: Hsla,
    pub modal_blur_radius: Pixels,
    pub floating_blur_radius: Pixels,
    pub floating_saturation: f32,
    pub tool_drawer_scrim: Hsla,
    pub tool_drawer_blur_radius: Pixels,
    pub radius_sm: Pixels,
    pub radius: Pixels,
    pub control_radius: Pixels,
    pub radius_panel: Pixels,
    pub radius_dialog: Pixels,
    pub text_xs: Pixels,
    pub text_sm: Pixels,
    pub text_body: Pixels,
    pub text_heading: Pixels,
    pub text_title: Pixels,
    pub(crate) materials: BackgroundMaterials,
    dark_foreground: Hsla,
    dark: bool,
}

impl ThemeTokens {
    /// 工作台沿用统一色板，保留入口以兼容已有调用并避免局部副本覆盖用户配色。
    ///
    /// @author kongweiguang
    pub(crate) fn for_workbench(self) -> Self {
        self
    }

    /// 当前连接的强调色随应用色板变化，不干扰终端内容色。
    ///
    /// @author kongweiguang
    pub(crate) fn active_connection_indicator(self) -> Hsla {
        self.accent
    }

    /// 已连接状态复用语义成功色，确保在不同色板中仍可识别。
    ///
    /// @author kongweiguang
    pub(crate) fn ready_connection_indicator(self) -> Hsla {
        self.success
    }

    /// 右栏与工作台共享同一色板；保留入口以避免各面板各自重建主题。
    ///
    /// @author kongweiguang
    pub(crate) fn for_tool_panel(self) -> Self {
        self
    }

    /// 设置与其他区域共用语义角色，切换色板时不覆盖 Kit 已应用的值。
    ///
    /// @author kongweiguang
    pub(crate) fn for_settings(self) -> Self {
        self
    }

    /// main 禁止合成字重；Windows 的 Segoe/YaHei 在 400 与 600/700 间缺少 500，CSS 会取 400。
    /// DirectWrite 的 500 匹配会偏向粗体，因此仅设置表单使用平台映射，终端字重设置保持真实值。
    ///
    /// @author kongweiguang
    pub(crate) fn settings_medium_font_weight(self) -> gpui_kit::FontWeight {
        if cfg!(target_os = "windows") {
            gpui_kit::FontWeight::NORMAL
        } else {
            gpui_kit::FontWeight::MEDIUM
        }
    }

    /// 保存提示使用语义成功文字色，确保浅深色板与普通正文保持足够区分。
    ///
    /// @author kongweiguang
    pub(crate) fn settings_saved_foreground(self) -> Hsla {
        self.success
    }

    /// Agent 外壳采用应用面板色与现有透明系数，内部终端仍使用其独立色盘。
    ///
    /// @author kongweiguang
    pub(crate) fn agent_surface(self) -> Hsla {
        self.panel.opacity(self.materials.terminal_body_opacity)
    }

    /// 选择卡的文字根据选中状态使用正文或次要角色，不依赖固定品牌色。
    ///
    /// @author kongweiguang
    pub(crate) fn settings_choice_foreground(self, selected: bool) -> Hsla {
        if selected {
            self.foreground
        } else {
            self.muted
        }
    }

    /// 系统色盘使用不透明面板色，避免继承壁纸透明度影响颜色判断。
    ///
    /// @author kongweiguang
    pub(crate) fn settings_native_color_picker_surface(self) -> Hsla {
        self.panel
    }

    /// 用正文角色的低透明度构造内衬，保留底层材质与各主题的色相。
    ///
    /// @author kongweiguang
    pub(crate) fn muted_surface(self) -> Hsla {
        self.foreground
            .opacity(if self.dark { 0.06 } else { 0.035 })
    }

    /// 字段使用当前主题的背景角色，透明度允许壁纸与浮层共同显示。
    ///
    /// @author kongweiguang
    pub(crate) fn field_surface(self) -> Hsla {
        self.background.opacity(if self.dark { 0.55 } else { 0.78 })
    }

    /// 字段悬停使用中性次级表面，避免与持久选中态混淆。
    ///
    /// @author kongweiguang
    pub(crate) fn field_hover_surface(self) -> Hsla {
        self.secondary
    }

    /// 输入边框使用次要文字角色，透明度保留与面板分隔线不同的层级。
    ///
    /// @author kongweiguang
    pub(crate) fn strong_border(self) -> Hsla {
        self.muted.opacity(0.70)
    }

    /// 在可叠加的表面上使用色板强调色，使选中态也能与背景图片组合。
    ///
    /// @author kongweiguang
    pub(crate) fn surface_selected(self) -> Hsla {
        self.accent.opacity(if self.dark { 0.16 } else { 0.12 })
    }

    /// 悬停使用前景透明叠层，让不同的面板与壁纸仍可辨认。
    ///
    /// @author kongweiguang
    pub(crate) fn surface_hover(self) -> Hsla {
        self.foreground
            .opacity(if self.dark { 0.08 } else { 0.045 })
    }

    /// 菜单使用中性叠色，避免将悬停误认作持久选择。
    ///
    /// @author kongweiguang
    pub(crate) fn context_menu_hover_surface(self) -> Hsla {
        self.foreground
            .opacity(if self.dark { 0.09 } else { 0.055 })
    }

    /// 右键菜单复用应用面板色并保留独立透明度，避免遮挡底层背景。
    ///
    /// @author kongweiguang
    pub(crate) fn context_menu_surface(self) -> Hsla {
        self.panel.opacity(0.86)
    }

    /// 上传动作遵循当前色板的强调色，避免菜单在非默认主题中残留蓝色。
    ///
    /// @author kongweiguang
    pub(crate) fn file_upload_menu_icon(self) -> Hsla {
        self.accent
    }

    /// Inspector 使用相同语义色板；保持投影入口避免调用方依赖具体颜色。
    ///
    /// @author kongweiguang
    pub(crate) fn for_inspector(self) -> Self {
        self
    }

    /// main 的弱背景按主题调节前景透明度；将明暗判断留在令牌层，避免视图读取主题内部状态。
    ///
    /// @author kongweiguang
    pub(crate) fn subtle_background(self) -> Hsla {
        self.foreground
            .opacity(if self.dark { 0.045 } else { 0.025 })
    }

    /// 为高亮背景选择对比度更高的正文色；半透明背景先按当前面板实色合成。
    ///
    /// @author kongweiguang
    pub(crate) fn foreground_on(self, background: Hsla) -> Hsla {
        palette::contrasting_foreground(
            self.panel,
            background,
            self.dark_foreground,
            self.primary_foreground,
        )
    }

    /// 从当前色板、Kit 明暗模式和原始材质设置一次性投影，避免局部控件回落默认色。
    ///
    /// @author kongweiguang
    pub(crate) fn current(_window: &Window, cx: &App) -> Self {
        let dark = cx.global::<KitTheme>().mode == KitThemeMode::Dark;
        let settings = cx
            .try_global::<BackgroundMaterialSettings>()
            .copied()
            .unwrap_or_default();

        let scheme = cx
            .try_global::<ActiveAppColorScheme>()
            .copied()
            .unwrap_or_default()
            .0;
        Self::for_palette_with_materials(dark, scheme, settings)
    }

    /// 预览与运行态复用同一色板解析；系统模式仅决定明暗，不替换用户选中的方案。
    ///
    /// @author kongweiguang
    pub(crate) fn for_scheme(
        appearance: WindowAppearance,
        mode: ThemeMode,
        scheme: AppColorScheme,
    ) -> Self {
        let dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => is_dark_appearance(appearance),
        };
        Self::for_palette_with_materials(dark, scheme, BackgroundMaterialSettings::default())
    }

    /// Rail 复用色板选中和悬停角色，保留 Kit 的键盘焦点与按压行为。
    ///
    /// @author kongweiguang
    pub(crate) fn rail_button_variant(
        self,
        selected: bool,
        cx: &gpui_kit::App,
    ) -> gpui_kit::component::button::ButtonCustomVariant {
        let surface = self.surface_selected();
        let foreground = self.settings_choice_foreground(true);
        gpui_kit::component::button::ButtonCustomVariant::new(cx)
            .color(if selected {
                surface
            } else {
                self.panel.alpha(0.)
            })
            .foreground(if selected { foreground } else { self.muted })
            .hover(if selected { surface } else { self.hover })
            .active(surface)
    }

    /// 两侧辅助区共享侧栏角色，浮层使用面板角色；中央与终端正文分别保留自己的底色。
    /// 再一次性投影透明材质，避免区域覆盖和重复应用累乘。
    ///
    /// @author kongweiguang
    fn for_palette_with_materials(
        dark: bool,
        scheme: AppColorScheme,
        settings: BackgroundMaterialSettings,
    ) -> Self {
        let colors = palette::palette(scheme, dark);
        let mut theme = Self {
            background: colors.background,
            sidebar: colors.sidebar,
            panel: colors.panel,
            tool_panel_surface: colors.sidebar,
            tool_panel_floating_surface: colors.panel.opacity(0.92),
            tool_panel_border: colors.border,
            secondary: colors.secondary,
            foreground: colors.foreground,
            muted: colors.muted,
            field_label: colors.muted,
            subtle: colors.muted,
            accent: colors.accent,
            primary: colors.primary,
            primary_hover: colors.primary_hover,
            primary_foreground: colors.primary_foreground,
            tunnel_action: colors.accent,
            tunnel_action_hover: colors.accent,
            monitor_live: colors.success,
            monitor_paused: colors.warning,
            monitor_error: colors.error,
            selected: colors.selected,
            selected_foreground: colors.foreground,
            hover: colors.hover,
            border: colors.border,
            terminal_background: colors.panel,
            error: colors.error,
            success: colors.success,
            warning: colors.warning,
            scrim: colors.scrim.opacity(if dark { 0.40 } else { 0.18 }),
            dialog_scrim: colors.scrim.opacity(if dark { 0.48 } else { 0.30 }),
            floating_surface: colors.panel.opacity(0.92),
            modal_blur_radius: px(12.),
            floating_blur_radius: px(28.),
            floating_saturation: 1.8,
            tool_drawer_scrim: colors.scrim.opacity(if dark { 0.40 } else { 0.18 }),
            tool_drawer_blur_radius: px(2.),
            radius_sm: px(4.),
            radius: px(8.),
            control_radius: px(6.),
            radius_panel: px(8.),
            radius_dialog: px(8.),
            text_xs: px(12.),
            text_sm: px(14.),
            text_body: px(14.),
            text_heading: px(16.),
            text_title: px(20.),
            materials: BackgroundMaterials {
                root_base: colors.background,
                shell: colors.background,
                chrome: colors.sidebar,
                workspace: colors.background,
                panel: colors.panel,
                terminal_body_opacity: 1.0,
                terminal_header: colors.panel,
                image_visible: false,
                image_opacity: 0.0,
                image_veil: 0.0,
                compositing_is_opaque: true,
            },
            dark_foreground: colors.dark_foreground,
            dark,
        };
        theme.materials = BackgroundMaterials::project(&theme, settings);
        theme
    }
}

/// 应用设置变更先更新色板身份，再同步 Kit 与自绘区域，避免窗口刷新看到旧配色。
///
/// @author kongweiguang
pub(crate) fn apply_app_theme<T: 'static>(
    mode: ThemeMode,
    scheme: AppColorScheme,
    appearance: WindowAppearance,
    cx: &mut Context<T>,
) {
    cx.set_global(ActiveAppColorScheme(scheme));
    apply_theme_mode(mode, appearance, cx);
}

/// 系统外观变化仅重新解析明暗版本，继续使用上一次选中的应用色板。
///
/// @author kongweiguang
pub(crate) fn apply_theme_mode<T: 'static>(
    mode: ThemeMode,
    appearance: WindowAppearance,
    cx: &mut Context<T>,
) {
    let kit_mode = match mode {
        ThemeMode::Light => KitThemeMode::Light,
        ThemeMode::Dark => KitThemeMode::Dark,
        ThemeMode::System => {
            if is_dark_appearance(appearance) {
                KitThemeMode::Dark
            } else {
                KitThemeMode::Light
            }
        }
    };
    KitTheme::change(kit_mode, None, cx.borrow_mut());
    let scheme = cx
        .try_global::<ActiveAppColorScheme>()
        .copied()
        .unwrap_or_default()
        .0;
    let tokens = ThemeTokens::for_scheme(appearance, mode, scheme);
    KitTheme::update(cx.borrow_mut(), |theme| {
        kit::apply_kit_palette(theme, tokens);
    });
}

/// GPUI 在 macOS 的 Vibrant appearance 同样表达浅深色，需与普通 appearance 一致映射。
fn is_dark_appearance(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

#[cfg(test)]
#[path = "../tests/unit/background_materials.rs"]
mod background_materials_tests;

#[cfg(test)]
#[path = "../tests/unit/theme_palette.rs"]
mod palette_tests;
