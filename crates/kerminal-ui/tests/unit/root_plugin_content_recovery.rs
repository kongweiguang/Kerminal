//! 已创建内容的激活、显隐和焦点失败必须保留原实例及可操作的恢复入口。
//!
//! @author kongweiguang

// 事件回归复用真实内容工厂和 Root 装配，避免用手工发布替代用户导航。
#[path = "root_plugin_tab_events.rs"]
mod tab_events;

use super::*;
use crate::features::workspace_controller::{
    WorkspaceRestoreFailure, WorkspaceRestoreItemId, WorkspaceRestoreStatus,
};
use crate::plugins::content::{
    ContentCloseFuture, ContentFactory, ContentInstance, ContentKind, ContentSavedState,
    ContentScope,
};
use gpui_kit::{AnyView, App, Context, Focusable as _, IntoElement, Render, Window, div};
use kerminal_application::plugins::{
    ContentDescriptor, ContentTypeId, PluginDescriptor, PluginId, PluginRegistryBuilder,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct FailureSwitches {
    activation: AtomicBool,
    visibility: AtomicBool,
    focus: AtomicBool,
    creates: AtomicUsize,
}

struct RecoveryView {
    input: Entity<gpui_kit::component::input::InputState>,
}

impl Render for RecoveryView {
    /// 真正绘制输入控件，避免只检查存在实体而漏掉失败视图仍被挂载的问题。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::prelude::*;
        div().size_full().child(
            gpui_kit::component::input::Input::new(&self.input)
                .id("recovery-plugin-input")
                .w_full(),
        )
    }
}

struct RecoveryFactory(Arc<FailureSwitches>);

impl ContentFactory for RecoveryFactory {
    /// 固定稳定身份，让三种生命周期失败走同一生产分派路径。
    fn content_id(&self) -> &str {
        "fixture.recovery-content"
    }

    /// 页面和标签共用真实输入实体，但宿主必须分别确认各自的恢复状态。
    fn supports(&self, _: ContentKind) -> bool {
        true
    }

    /// 创建必定成功，专门复现此前只覆盖创建失败而遗漏的后续生命周期错误。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.0.creates.fetch_add(1, Ordering::SeqCst);
        let text = match (scope, state) {
            (ContentScope::Page(_), None) => "preserved-page-draft",
            (_, Some(state)) => state.value["text"]
                .as_str()
                .ok_or_else(|| AppError::invalid("fixture state missing"))?,
            _ => return Err(AppError::invalid("fixture state missing")),
        }
        .to_owned();
        Ok(Box::new(RecoveryContent {
            view: cx.new(|cx| RecoveryView {
                input: cx.new(|cx| {
                    gpui_kit::component::input::InputState::new(window, cx).default_value(text)
                }),
            }),
            switches: self.0.clone(),
        }))
    }
}

struct RecoveryContent {
    view: Entity<RecoveryView>,
    switches: Arc<FailureSwitches>,
}

impl ContentInstance for RecoveryContent {
    /// 重试必须继续使用这个真实输入实体，而不是丢弃草稿后重新创建。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 从实际输入读取草稿，确认失败恢复没有仅保留预设恢复值。
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState> {
        Ok(ContentSavedState {
            version: 1,
            value: serde_json::json!({"text": self.view.read(cx).input.read(cx).value().to_string()}),
        })
    }

    /// 激活只失败一次，使实际重试按钮能恢复同一个实例。
    fn activate(&mut self, _: &mut Window, _: &mut App) -> AppResult<()> {
        if self.switches.activation.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture activation failed"));
        }
        Ok(())
    }

    /// 模拟无法开始采样的内容，不允许宿主把未确认的显示当成恢复成功。
    fn visibility_changed(&mut self, visible: bool, _: bool, _: &mut App) -> AppResult<()> {
        if visible && self.switches.visibility.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture visibility failed"));
        }
        Ok(())
    }

    /// 焦点失败与创建失败不同；重试成功后仍须能实际输入。
    fn focus(&mut self, window: &mut Window, cx: &mut App) -> AppResult<()> {
        if self.switches.focus.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture focus failed"));
        }
        window.focus(&self.view.read(cx).input.read(cx).focus_handle(cx), cx);
        Ok(())
    }

    /// 内存视图没有外部资源，但仍通过正式异步关闭契约释放 owner。
    fn begin_close(&mut self, _: std::time::Instant, _: &mut App) -> AppResult<ContentCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 从真实按钮重试后，标签、实体、草稿和工厂创建次数都应保持原身份。
fn exercise_failure(app: &mut TestAppContext, switches: Arc<FailureSwitches>) {
    let provider = PluginId::new("fixture.recovery-provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    registry.register_content_type(ContentDescriptor::new(
        ContentTypeId::new("fixture.recovery-content").unwrap(),
        provider,
        "Recovery fixture",
    ));
    let registry = Arc::new(registry.freeze().unwrap());
    let factory = Arc::new(RecoveryFactory(switches.clone()));
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services
                .with_plugins(registry)
                .with_content_factories(vec![factory])
        },
    );
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.activate_window();
        window.focus(&root.read(app).focus_handle.clone(), app);
    });
    visual.run_until_parked();
    let (tab_id, entity_id) = visual.update(|window, app| {
        window.draw(app).clear(app);
        root.update(app, |root, cx| {
            root.workspace_controller = Some(WorkspaceController::from_missing().unwrap());
            root.workspace_load_pending = false;
            let tab_id = root
                .open_plugin_tab(
                    &crate::plugins::actions::OpenContentTab {
                        content_id: "fixture.recovery-content".into(),
                        title: "Recovery fixture".into(),
                        state_version: 1,
                        state: serde_json::json!({"text": "preserved-draft"}),
                    },
                    window,
                    cx,
                )
                .unwrap();
            let scope = ContentScope::Tab(tab_id.clone());
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .restore_status(&WorkspaceRestoreItemId::Tab(tab_id.clone())),
                Some(WorkspaceRestoreStatus::Failed(
                    WorkspaceRestoreFailure::ResourceFailure
                ))
            );
            assert!(root.content_failures.contains_key(&scope));
            (tab_id, root.content_host.view(&scope).unwrap().entity_id())
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let input: gpui_kit::ElementId = "recovery-plugin-input".into();
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .all(|node| !node.visible() || !node.path().contains(&input)),
            "生命周期失败的视图不能挂载成正常正文"
        );
    });
    click_root_button(visual, "retry-plugin-content");
    settle(&runtime, visual);
    root.read_with(visual, |root, app| {
        let scope = ContentScope::Tab(tab_id.clone());
        assert!(!root.content_failures.contains_key(&scope));
        assert_eq!(
            root.content_host.view(&scope).unwrap().entity_id(),
            entity_id
        );
        assert_eq!(
            root.content_host.snapshot(&scope, app).unwrap().value["text"],
            "preserved-draft"
        );
        assert_eq!(switches.creates.load(Ordering::SeqCst), 1);
    });
    click_root_button(visual, "recovery-plugin-input");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("edited-after-retry");
    visual.run_until_parked();
    root.read_with(visual, |root, app| {
        assert_eq!(
            root.content_host
                .snapshot(&ContentScope::Tab(tab_id), app)
                .unwrap()
                .value["text"],
            "edited-after-retry"
        );
    });
}

/// 已创建实例激活失败时，原视图不能遮住恢复入口。
#[gpui_kit::test]
fn activation_failure_keeps_a_working_retry(app: &mut TestAppContext) {
    let switches = Arc::new(FailureSwitches::default());
    switches.activation.store(true, Ordering::SeqCst);
    exercise_failure(app, switches);
}

/// 显示回调拒绝后不得登记 Ready 或自动重绘重试成成功。
#[gpui_kit::test]
fn visibility_failure_keeps_a_working_retry(app: &mut TestAppContext) {
    let switches = Arc::new(FailureSwitches::default());
    switches.visibility.store(true, Ordering::SeqCst);
    exercise_failure(app, switches);
}

/// 焦点交接失败同样保留实例，并允许用户从相同标签明确重试。
#[gpui_kit::test]
fn focus_failure_keeps_a_working_retry(app: &mut TestAppContext) {
    let switches = Arc::new(FailureSwitches::default());
    switches.focus.store(true, Ordering::SeqCst);
    exercise_failure(app, switches);
}

/// 页面没有标签恢复回执；首次焦点失败及再次激活失败也必须露出同实例重试入口。
fn exercise_page_failure(app: &mut TestAppContext, fail_initial_focus: bool) {
    let switches = Arc::new(FailureSwitches::default());
    switches.focus.store(fail_initial_focus, Ordering::SeqCst);
    let provider = PluginId::new("fixture.recovery-provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    registry.register_content_type(ContentDescriptor::new(
        ContentTypeId::new("fixture.recovery-content").unwrap(),
        provider,
        "Recovery fixture",
    ));
    let registry = Arc::new(registry.freeze().unwrap());
    let factory = Arc::new(RecoveryFactory(switches.clone()));
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services
                .with_plugins(registry)
                .with_content_factories(vec![factory])
        },
    );
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.activate_window();
        window.focus(&root.read(app).focus_handle.clone(), app);
    });
    visual.run_until_parked();
    let scope = ContentScope::Page("fixture.recovery-content".into());
    let view = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.workspace_controller = Some(WorkspaceController::from_missing().unwrap());
            root.workspace_load_pending = false;
            let opened = root.open_plugin_page("fixture.recovery-content", window, cx);
            assert_eq!(opened.is_err(), fail_initial_focus);
            root.content_host
                .view(&scope)
                .unwrap()
                .downcast::<RecoveryView>()
                .ok()
                .unwrap()
        })
    });
    settle(&runtime, visual);
    if !fail_initial_focus {
        click_root_button(visual, "recovery-plugin-input");
        visual.simulate_keystrokes("secondary-a backspace");
        visual.simulate_input("user-page-draft");
        visual.run_until_parked();
        switches.activation.store(true, Ordering::SeqCst);
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                assert!(
                    root.open_plugin_page("fixture.recovery-content", window, cx)
                        .is_err()
                );
            });
        });
        settle(&runtime, visual);
    }
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let input: gpui_kit::ElementId = "recovery-plugin-input".into();
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .all(|node| !node.visible() || !node.path().contains(&input)),
            "页面生命周期失败后不能继续挂载正文而遮住重试入口"
        );
    });
    click_root_button(visual, "retry-plugin-content");
    settle(&runtime, visual);
    root.read_with(visual, |root, app| {
        assert!(!root.content_failures.contains_key(&scope));
        assert!(root.plugin_content_is_ready(&scope));
        assert_eq!(
            root.content_host.view(&scope).unwrap().entity_id(),
            view.entity_id()
        );
        assert_eq!(switches.creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            view.read(app).input.read(app).value().as_str(),
            if fail_initial_focus {
                "preserved-page-draft"
            } else {
                "user-page-draft"
            }
        );
    });
    click_root_button(visual, "recovery-plugin-input");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("page-edited-after-retry");
    visual.run_until_parked();
    visual.update(|_, app| {
        assert_eq!(
            view.read(app).input.read(app).value().as_str(),
            "page-edited-after-retry"
        );
    });
}

/// 首次页面焦点失败须保留正文实体，但不能用它遮住可操作的恢复界面。
#[gpui_kit::test]
fn page_focus_failure_keeps_a_working_retry(app: &mut TestAppContext) {
    exercise_page_failure(app, true);
}

/// 已打开页面再次激活失败不能沿用旧的显示确认，重试仍保留用户输入草稿。
#[gpui_kit::test]
fn page_activation_failure_keeps_a_working_retry(app: &mut TestAppContext) {
    exercise_page_failure(app, false);
}
