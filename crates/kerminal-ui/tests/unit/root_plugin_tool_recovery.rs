//! 恢复中的工具创建失败只能由新的用户意图重试，普通绘制不重复建立资源。
//!
//! @author kongweiguang

use super::*;
use crate::plugins::tools::{
    ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope,
};
use gpui_kit::{AnyView, App, Context, IntoElement, ParentElement as _, Render, Window, div};
use kerminal_application::plugins::{
    PluginDescriptor, PluginId, PluginRegistryBuilder, ToolDescriptor, ToolId,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct FixtureFactory {
    fail: Arc<AtomicBool>,
    creates: Arc<AtomicUsize>,
}

impl ToolFactory for FixtureFactory {
    /// 固定工具身份，使布局恢复与显式打开命中同一工厂。
    fn tool_id(&self) -> &str {
        "fixture.retry-tool"
    }

    /// 使用真实 Kit 图标及名称，与普通插件工具进入同一栏位。
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(
            "Retry fixture",
            "Retry fixture",
            gpui_kit::assets::IconName::FileText,
        )
    }

    /// 窗口作用域排除活动终端变化，保证普通重绘没有新建实例的理由。
    fn scope(&self) -> ToolScope {
        ToolScope::Window
    }

    /// 计数反映真实创建调用，模拟暂时无法初始化的插件资源。
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture tool unavailable"));
        }
        Ok(Box::new(FixtureInstance(cx.new(|_| FixtureView))))
    }
}

struct FixtureView;

impl Render for FixtureView {
    /// 成功重试后提供实际正文，避免只登记一个没有视图的成功实例。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child("tool restored")
    }
}

struct FixtureInstance(Entity<FixtureView>);

impl ToolInstance for FixtureInstance {
    /// 窗口作用域始终复用同一个实体。
    fn view(&self) -> AnyView {
        self.0.clone().into()
    }

    /// 内存实例仍返回真实关闭回执供宿主释放。
    fn begin_close(&mut self, _: std::time::Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 首帧恢复失败后反复绘制不再次调用工厂，用户重新打开时才重试并可挂载成功。
#[gpui_kit::test]
fn redraw_does_not_retry_failed_tool_creation(app: &mut TestAppContext) {
    let provider = PluginId::new("fixture.retry-provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    registry.register_tool(ToolDescriptor::new(
        ToolId::new("fixture.retry-tool").unwrap(),
        provider,
        "Retry fixture",
        None,
    ));
    let fail = Arc::new(AtomicBool::new(true));
    let creates = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(FixtureFactory {
        fail: fail.clone(),
        creates: creates.clone(),
    });
    let registry = Arc::new(registry.freeze().unwrap());
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services
                .with_plugins(registry)
                .with_tool_factories(vec![factory])
        },
    );
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.activate_window();
        window.focus(&root.read(app).focus_handle.clone(), app);
    });
    visual.run_until_parked();
    let tool =
        kerminal_application::preferences::ToolRailToolId::from_id("fixture.retry-tool").unwrap();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            // 模拟恢复了已打开槽位但尚未创建实例，不绕过随后生产投影的创建入口。
            let settings = root.current_tool_rail_settings();
            root.tool_rail_state.toggle(&settings, tool);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    settle(&runtime, visual);
    for _ in 0..5 {
        visual.update(|window, app| window.draw(app).clear(app));
        visual.run_until_parked();
    }
    assert_eq!(
        creates.load(Ordering::SeqCst),
        1,
        "重绘不能自动重试失败工厂"
    );
    fail.store(false, Ordering::SeqCst);
    click_root_button(visual, "retry-plugin-tool-fixture.retry-tool");
    settle(&runtime, visual);
    assert_eq!(creates.load(Ordering::SeqCst), 2);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                root.plugin_tools
                    .view_for_mount(
                        tool,
                        &root.plugin_tool_context(cx),
                        window.is_window_active()
                    )
                    .is_some()
            );
        });
    });
}
