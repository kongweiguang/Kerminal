//! 工具上下文和关闭回执的真实 GPUI 实例契约。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use futures::executor::block_on;
use gpui_kit::{
    AnyView, App, AppContext, Context, Entity, IntoElement, Render, TestAppContext, Window, div,
};
use kerminal_application::{
    AppError, AppResult,
    plugins::{PluginDescriptor, PluginId, PluginRegistryBuilder, ToolDescriptor, ToolId},
    preferences::ToolRailToolId,
};
use kerminal_domain::identity::{HostId, TabId};
use kerminal_ui::plugins::tools::{
    PluginTools, ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPhase,
    ToolPresentation, ToolScope,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Context(&'static str, ToolContext),
    Visibility(&'static str, bool, bool),
}

struct FixtureView;

impl Render for FixtureView {
    /// 保留实体身份，使重复导航能证明复用了同一实例。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct FixtureFactory {
    id: &'static str,
    scope: ToolScope,
    events: Arc<Mutex<Vec<Event>>>,
    fail_next_context: Arc<AtomicBool>,
    fail_next_visibility: Arc<AtomicBool>,
}

impl ToolFactory for FixtureFactory {
    /// 工厂与冻结目录使用同一稳定身份。
    ///
    /// @author kongweiguang
    fn tool_id(&self) -> &str {
        self.id
    }

    /// 界面元数据与上下文状态独立。
    ///
    /// @author kongweiguang
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(self.id, "fixture", gpui_kit::assets::IconName::FileText)
    }

    /// 测试分别覆盖窗口和标签的保留边界。
    ///
    /// @author kongweiguang
    fn scope(&self) -> ToolScope {
        self.scope
    }

    /// 每次创建新实体，使跨标签误共享能够被观察。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        Ok(Box::new(FixtureInstance {
            id: self.id,
            view: cx.new(|_| FixtureView),
            events: self.events.clone(),
            fail_next_context: self.fail_next_context.clone(),
            fail_next_visibility: self.fail_next_visibility.clone(),
        }))
    }
}

struct FixtureInstance {
    id: &'static str,
    view: Entity<FixtureView>,
    events: Arc<Mutex<Vec<Event>>>,
    fail_next_context: Arc<AtomicBool>,
    fail_next_visibility: Arc<AtomicBool>,
}

impl ToolInstance for FixtureInstance {
    /// 始终返回已有句柄，避免测试把重建视图误判为上下文更新。
    ///
    /// @author kongweiguang
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 允许一次注入失败，验证宿主只能缓存已确认的上下文。
    ///
    /// @author kongweiguang
    fn context_changed(&mut self, context: &ToolContext, _: &mut App) -> AppResult<()> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Context(self.id, context.clone()));
        if self.fail_next_context.swap(false, Ordering::SeqCst) {
            Err(AppError::infrastructure("fixture context failed"))
        } else {
            Ok(())
        }
    }

    /// 记录真实通知顺序，验证失败的旧上下文不会保持可见。
    ///
    /// @author kongweiguang
    fn visibility_changed(&mut self, visible: bool, active: bool, _: &mut App) -> AppResult<()> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Visibility(self.id, visible, active));
        if self.fail_next_visibility.swap(false, Ordering::SeqCst) {
            Err(AppError::infrastructure("fixture visibility failed"))
        } else {
            Ok(())
        }
    }

    /// 使用立即完成的回执核对关闭确认和最终释放是两个步骤。
    ///
    /// @author kongweiguang
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 窗口实例接收活动上下文，后台标签保留原上下文；失败不提交并且可以重试。
///
/// @author kongweiguang
#[gpui_kit::test]
fn context_changes_follow_scope_and_retry_without_recreating_views(app: &mut TestAppContext) {
    let provider = PluginId::new("fixture.provider").unwrap();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    for id in ["fixture.window", "fixture.tab"] {
        builder.register_tool(ToolDescriptor::new(
            ToolId::new(id).unwrap(),
            provider.clone(),
            id,
            None,
        ));
    }
    let registry = builder.freeze().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let fail_window = Arc::new(AtomicBool::new(false));
    let fail_visibility = Arc::new(AtomicBool::new(false));
    let (_, visual) = app.add_window_view(|_, _| FixtureView);
    visual.update(|window, cx| {
        let mut tools = PluginTools::default();
        for (id, scope, fail) in [
            ("fixture.window", ToolScope::Window, fail_window.clone()),
            (
                "fixture.tab",
                ToolScope::Tab,
                Arc::new(AtomicBool::new(false)),
            ),
        ] {
            tools
                .register(
                    &registry,
                    Arc::new(FixtureFactory {
                        id,
                        scope,
                        events: events.clone(),
                        fail_next_context: fail,
                        fail_next_visibility: if id == "fixture.window" {
                            fail_visibility.clone()
                        } else {
                            Arc::new(AtomicBool::new(false))
                        },
                    }),
                )
                .unwrap();
        }
        let window_tool = ToolRailToolId::from_id("fixture.window").unwrap();
        let tab_tool = ToolRailToolId::from_id("fixture.tab").unwrap();
        let tab_a = TabId::parse("fixture-a").unwrap();
        let tab_b = TabId::parse("fixture-b").unwrap();
        let host_a = HostId::new();
        let host_b = HostId::new();
        let host_c = HostId::new();
        let a = ToolContext::new(Some(tab_a.clone()), Some(host_a));
        let b = ToolContext::new(Some(tab_b.clone()), Some(host_b));
        let b_host = ToolContext::new(Some(tab_b.clone()), Some(host_c));
        tools.ensure(window_tool, &a, window, cx).unwrap();
        tools.ensure(tab_tool, &a, window, cx).unwrap();
        let window_id = tools.view(window_tool, &a).unwrap().entity_id();
        let tab_a_id = tools.view(tab_tool, &a).unwrap().entity_id();
        tools
            .sync_visibility(&a, &[window_tool, tab_tool], true, cx)
            .unwrap();
        tools
            .sync_visibility(&a, &[window_tool, tab_tool], true, cx)
            .unwrap();
        assert_eq!(events.lock().unwrap().len(), 2);
        assert!(tools.view_for_mount(window_tool, &a, true).is_some());
        assert!(tools.view_for_mount(window_tool, &a, false).is_none());
        assert!(tools.view(window_tool, &b).is_none());

        fail_visibility.store(true, Ordering::SeqCst);
        assert!(
            tools
                .sync_visibility(&a, &[window_tool, tab_tool], false, cx)
                .is_err()
        );
        assert!(tools.view_for_mount(window_tool, &a, false).is_none());
        tools
            .sync_visibility(&a, &[window_tool, tab_tool], false, cx)
            .unwrap();
        assert_eq!(
            tools
                .view_for_mount(window_tool, &a, false)
                .unwrap()
                .entity_id(),
            window_id
        );
        tools
            .sync_visibility(&a, &[window_tool, tab_tool], true, cx)
            .unwrap();

        fail_window.store(true, Ordering::SeqCst);
        assert!(tools.sync_visibility(&b, &[window_tool], true, cx).is_err());
        assert_eq!(tools.phase(window_tool, &b), Some(ToolPhase::Open));
        assert!(tools.view(window_tool, &b).is_none());
        assert!(tools.view_for_mount(window_tool, &b, true).is_none());
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&Event::Visibility("fixture.tab", false, true))
        );
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&Event::Visibility("fixture.window", false, true))
        );
        tools.sync_visibility(&b, &[window_tool], true, cx).unwrap();
        assert_eq!(tools.view(window_tool, &b).unwrap().entity_id(), window_id);
        assert_eq!(
            tools
                .view_for_mount(window_tool, &b, true)
                .unwrap()
                .entity_id(),
            window_id
        );
        tools.ensure(tab_tool, &b, window, cx).unwrap();
        let tab_b_id = tools.view(tab_tool, &b).unwrap().entity_id();
        assert_ne!(tab_b_id, tab_a_id);
        tools
            .sync_visibility(&b, &[window_tool, tab_tool], true, cx)
            .unwrap();
        tools
            .sync_visibility(&b_host, &[window_tool, tab_tool], true, cx)
            .unwrap();
        tools
            .sync_visibility(&b_host, &[window_tool, tab_tool], true, cx)
            .unwrap();
        assert_eq!(tools.tools_for_tab(&tab_a), vec![tab_tool]);
        assert_eq!(tools.tools_for_tab(&tab_b), vec![tab_tool]);
        let recorded = events.lock().unwrap().clone();
        assert_eq!(
            recorded
                .iter()
                .filter(|event| **event == Event::Context("fixture.window", b.clone()))
                .count(),
            2
        );
        assert!(
            !recorded
                .iter()
                .any(|event| *event == Event::Context("fixture.tab", b.clone()))
        );
        assert_eq!(
            recorded
                .iter()
                .filter(|event| **event == Event::Context("fixture.tab", b_host.clone()))
                .count(),
            1
        );
        assert_eq!(
            recorded
                .iter()
                .filter(|event| **event == Event::Context("fixture.window", b_host.clone()))
                .count(),
            1
        );

        let all = tools.all_instance_contexts();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&(tab_tool, a.clone())));
        assert!(all.contains(&(tab_tool, b_host.clone())));
        let deadline = Instant::now() + Duration::from_secs(5);
        let (token, close) = tools.begin_close(tab_tool, &a, deadline, cx).unwrap();
        tools.complete_close(token, block_on(close)).unwrap();
        assert_eq!(tools.phase(tab_tool, &a), Some(ToolPhase::Closed));
        assert!(
            tools
                .all_instance_contexts()
                .contains(&(tab_tool, a.clone()))
        );
        tools.finalize_close(tab_tool, &a).unwrap();
        assert_eq!(tools.phase(tab_tool, &a), None);
        assert_eq!(tools.view(tab_tool, &b_host).unwrap().entity_id(), tab_b_id);
    });
}
