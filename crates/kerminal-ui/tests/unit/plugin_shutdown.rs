//! 退出时插件实例关闭的回执、截止时间和重试边界。
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
use kerminal_domain::identity::TabId;

use super::{PluginCloseToken, begin_plugin_closes, complete_plugin_close, finalize_plugin_closes};
use crate::plugins::{
    content::{
        ContentCloseFuture, ContentFactory, ContentHost, ContentInstance, ContentKind,
        ContentPhase, ContentSavedState, ContentScope,
    },
    tools::{
        PluginTools, ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPhase,
        ToolPresentation, ToolScope,
    },
};

struct FixtureView;

impl Render for FixtureView {
    /// 实体只用于验证宿主真实持有的实例，测试不依赖任何屏幕内容。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct FixtureContentFactory {
    deadlines: Arc<Mutex<Vec<Instant>>>,
}

impl ContentFactory for FixtureContentFactory {
    /// 页面与标签使用同一个实现，确保退出枚举两个作用域。
    fn content_id(&self) -> &str {
        "fixture.content"
    }

    /// 使用同一工厂检查页面和标签不会互相覆盖。
    fn supports(&self, _: ContentKind) -> bool {
        true
    }

    /// 视图由真实 GPUI 实体承载，关闭记录由测试共享读取。
    fn create(
        &self,
        _: &ContentScope,
        _: Option<&ContentSavedState>,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        Ok(Box::new(FixtureContent {
            view: cx.new(|_| FixtureView),
            deadlines: self.deadlines.clone(),
        }))
    }
}

struct FixtureContent {
    view: Entity<FixtureView>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
}

impl ContentInstance for FixtureContent {
    /// 保持真实实体的所有权，直到宿主提交关闭。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 测试仅关注生命周期，状态内容无需依赖真实存储。
    fn snapshot(&self, _: &App) -> AppResult<ContentSavedState> {
        Ok(ContentSavedState {
            version: 1,
            value: serde_json::Value::Null,
        })
    }

    /// 记录每次关闭实际接收到的截止时间，回执由测试按不同顺序完成。
    fn begin_close(&mut self, deadline: Instant, _: &mut App) -> AppResult<ContentCloseFuture> {
        self.deadlines.lock().unwrap().push(deadline);
        Ok(Box::pin(async { Ok(()) }))
    }
}

struct FixtureToolFactory {
    id: &'static str,
    scope: ToolScope,
    deadlines: Arc<Mutex<Vec<Instant>>>,
    fail_once: Arc<AtomicBool>,
}

impl ToolFactory for FixtureToolFactory {
    /// 不同工具身份分别代表窗口和标签作用域。
    fn tool_id(&self) -> &str {
        self.id
    }

    /// 测试不依赖展示内容，但仍使用真实贡献元数据。
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(self.id, "fixture", gpui_kit::assets::IconName::FileText)
    }

    /// 作用域由工厂固定，避免把两个标签合并为一个实例。
    fn scope(&self) -> ToolScope {
        self.scope
    }

    /// 实例共享失败注入和观察记录，避免在退出流程中替换 owner。
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        Ok(Box::new(FixtureTool {
            view: cx.new(|_| FixtureView),
            deadlines: self.deadlines.clone(),
            fail_once: self.fail_once.clone(),
        }))
    }
}

struct FixtureTool {
    view: Entity<FixtureView>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
    fail_once: Arc<AtomicBool>,
}

impl ToolInstance for FixtureTool {
    /// 保留实体句柄使失败期间仍可核对原 owner。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 一次同步拒绝模拟插件不能启动清理，其他实例仍须启动。
    fn begin_close(&mut self, deadline: Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        self.deadlines.lock().unwrap().push(deadline);
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture cannot start close"));
        }
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 不同实例同时遭遇同步失败与超时，成功实例仍等待整体确认，重试只重新关闭失败项。
///
/// @author kongweiguang
#[gpui_kit::test]
fn shutdown_retains_all_owners_until_failed_instances_are_retried(app: &mut TestAppContext) {
    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let provider = PluginId::new("fixture.provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    for id in ["fixture.window", "fixture.tab"] {
        registry.register_tool(ToolDescriptor::new(
            ToolId::new(id).unwrap(),
            provider.clone(),
            id,
            None,
        ));
    }
    let registry = registry.freeze().unwrap();
    let (_, visual) = app.add_window_view(|_, _| FixtureView);
    visual.update(|window, cx| {
        let mut contents = ContentHost::default();
        contents
            .register(Arc::new(FixtureContentFactory {
                deadlines: deadlines.clone(),
            }))
            .unwrap();
        let page = ContentScope::Page("fixture.content".to_owned());
        let tab = ContentScope::Tab(TabId::parse("fixture-content").unwrap());
        for scope in [page.clone(), tab.clone()] {
            contents
                .open(scope, "fixture.content", None, window, cx)
                .unwrap();
        }
        let mut tools = PluginTools::default();
        for (id, scope, fail) in [
            ("fixture.window", ToolScope::Window, true),
            ("fixture.tab", ToolScope::Tab, false),
        ] {
            tools
                .register(
                    &registry,
                    Arc::new(FixtureToolFactory {
                        id,
                        scope,
                        deadlines: deadlines.clone(),
                        fail_once: Arc::new(AtomicBool::new(fail)),
                    }),
                )
                .unwrap();
        }
        let window_tool = ToolRailToolId::from_id("fixture.window").unwrap();
        let tab_tool = ToolRailToolId::from_id("fixture.tab").unwrap();
        let a = ToolContext::new(Some(TabId::parse("fixture-a").unwrap()), None);
        let b = ToolContext::new(Some(TabId::parse("fixture-b").unwrap()), None);
        tools.ensure(window_tool, &a, window, cx).unwrap();
        tools.ensure(tab_tool, &a, window, cx).unwrap();
        tools.ensure(tab_tool, &b, window, cx).unwrap();

        let first_deadline = Instant::now() + Duration::from_secs(5);
        let (jobs, error) = begin_plugin_closes(&mut contents, &mut tools, first_deadline, cx);
        assert!(error.is_some());
        assert_eq!(jobs.len(), 4);
        assert!(
            deadlines
                .lock()
                .unwrap()
                .iter()
                .all(|value| *value == first_deadline)
        );
        let mut timed_out = false;
        let mut rejected = false;
        for (token, future) in jobs {
            let should_timeout = !timed_out && matches!(&token, PluginCloseToken::Content(_));
            let should_reject = !rejected && matches!(&token, PluginCloseToken::Tool(_));
            let result = if should_timeout {
                timed_out = true;
                Err(AppError::Timeout {
                    operation: "fixture timeout",
                })
            } else if should_reject {
                rejected = true;
                Err(AppError::cancelled("fixture executor rejected task"))
            } else {
                block_on(future)
            };
            let result = complete_plugin_close(&mut contents, &mut tools, token, result);
            assert_eq!(result.is_err(), should_timeout || should_reject);
        }
        assert!(timed_out);
        assert!(rejected);
        assert_eq!(
            [&page, &tab]
                .iter()
                .filter(|scope| contents.phase(scope) == Some(ContentPhase::CloseFailed))
                .count(),
            1,
        );
        assert_eq!(
            [&page, &tab]
                .iter()
                .filter(|scope| contents.phase(scope) == Some(ContentPhase::Closed))
                .count(),
            1,
        );
        assert_eq!(tools.phase(window_tool, &a), Some(ToolPhase::CloseFailed));
        assert!(finalize_plugin_closes(&mut contents, &mut tools).is_err());
        assert_eq!(contents.scopes().len(), 2);
        assert_eq!(tools.all_instance_contexts().len(), 3);

        let retry_deadline = Instant::now() + Duration::from_secs(10);
        let (jobs, error) = begin_plugin_closes(&mut contents, &mut tools, retry_deadline, cx);
        assert!(error.is_none());
        assert_eq!(jobs.len(), 3);
        for (token, future) in jobs {
            complete_plugin_close(&mut contents, &mut tools, token, block_on(future)).unwrap();
        }
        let recorded = deadlines.lock().unwrap().clone();
        assert_eq!(recorded.len(), 8);
        assert!(recorded[5..].iter().all(|value| *value == retry_deadline));
        finalize_plugin_closes(&mut contents, &mut tools).unwrap();
        assert!(contents.scopes().is_empty());
        assert!(tools.all_instance_contexts().is_empty());
    });
}

/// 超时必须返回明确错误，不能把未返回的 future 当作资源已清理。
///
/// @author kongweiguang
#[test]
fn pending_close_returns_timeout_at_the_given_deadline() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let future: ContentCloseFuture = Box::pin(std::future::pending());
    let result = runtime.block_on(kerminal_application::executor::await_until(
        Instant::now(),
        "fixture timeout",
        future,
    ));
    assert!(matches!(result, Err(AppError::Timeout { .. })));
}
