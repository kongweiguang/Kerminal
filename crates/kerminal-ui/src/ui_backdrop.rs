//! main 的背景滤镜只装饰现有元素；布局、命中、焦点和生命周期仍由原元素管理。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, App, Bounds, Corners, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Window, px,
};

use crate::theme::ThemeTokens;

struct BackdropFilterElement {
    content: AnyElement,
    radius: Pixels,
    corners: Corners<Pixels>,
    saturation: f32,
}

/// 全窗模态滤镜先于原遮罩绘制，避免把遮罩颜色混入背景采样或增加第二个交互层。
///
/// @author kongweiguang
pub(crate) fn modal_backdrop(content: impl IntoElement, theme: ThemeTokens) -> AnyElement {
    BackdropFilterElement {
        content: content.into_any_element(),
        radius: theme.modal_blur_radius,
        corners: Corners::all(px(0.)),
        saturation: 1.0,
    }
    .into_any_element()
}

/// 紧凑工具抽屉使用 main 的轻量 2px 遮罩滤镜，不能误用决策弹框的 12px 强度。
///
/// @author kongweiguang
pub(crate) fn compact_backdrop(content: impl IntoElement, theme: ThemeTokens) -> AnyElement {
    BackdropFilterElement {
        content: content.into_any_element(),
        radius: theme.tool_drawer_blur_radius,
        corners: Corners::all(px(0.)),
        saturation: 1.0,
    }
    .into_any_element()
}

/// 浮动表面的滤镜使用真实布局边界，并在表面自身背景、边框和子内容之前采样。
///
/// @author kongweiguang
pub(crate) fn floating_surface(
    content: impl IntoElement,
    theme: ThemeTokens,
    corner_radius: Pixels,
) -> AnyElement {
    BackdropFilterElement {
        content: content.into_any_element(),
        radius: theme.floating_blur_radius,
        corners: Corners::all(corner_radius),
        saturation: theme.floating_saturation,
    }
    .into_any_element()
}

/// 菜单滤镜匹配 main 的 30px 玻璃模糊，同时让原 PopupMenu 保持唯一布局和交互树。
///
/// @author kongweiguang
pub(crate) fn context_menu_surface(content: impl IntoElement, theme: ThemeTokens) -> AnyElement {
    BackdropFilterElement {
        content: content.into_any_element(),
        radius: px(30.),
        corners: Corners::all(theme.radius),
        saturation: theme.floating_saturation,
    }
    .into_any_element()
}

impl IntoElement for BackdropFilterElement {
    type Element = Self;

    /// 装饰器保留自身到绘制阶段，不创建额外 Entity 或窗口。
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BackdropFilterElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    /// 无状态装饰器不引入 ID，原控件的持久身份与焦点路径因此保持稳定。
    fn id(&self) -> Option<ElementId> {
        None
    }

    /// 调试源仍由被装饰的真实控件提供，避免形成虚假的可交互节点。
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// 复用子元素唯一的布局节点，防止绝对定位或受限表单被额外容器改变尺寸。
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }

    /// 命中、可访问性和焦点仍由原树预绘制，不为材质注册鼠标监听器。
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.prepaint(window, cx);
    }

    /// GPU 采样必须发生在被装饰表面之前；不支持的平台沿用 Window 的无滤镜回退。
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_backdrop_filter(bounds, self.radius, self.corners, self.saturation);
        self.content.paint(window, cx);
    }
}
