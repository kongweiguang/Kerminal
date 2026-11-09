//! 真实 GPUI 实体与内容工厂的身份、恢复和关闭回执约束。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::executor::block_on;
use gpui_kit::{
    AnyView, App, AppContext, Context, Entity, IntoElement, Render, TestAppContext, Window, div,
    prelude::*,
};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::identity::TabId;
use kerminal_ui::plugins::content::{
    ContentCloseFuture, ContentFactory, ContentHost, ContentInstance, ContentInstanceIdentity,
    ContentKind, ContentPhase, ContentSavedState, ContentScope,
};
use serde_json::{Value, json};

struct FixtureView {
    value: Value,
    identity: Option<ContentInstanceIdentity>,
}

impl Render for FixtureView {
    /// 用可保留状态的真实实体验证宿主持有的是视图，而非空元数据。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(self.value.to_string())
    }
}

struct FixtureFactory {
    creates: Arc<AtomicUsize>,
    close_attempts: Arc<AtomicUsize>,
    visibility_changes: Arc<AtomicUsize>,
}

impl ContentFactory for FixtureFactory {
    /// 测试工厂使用固定身份，使恢复过程只依赖注册表而不依赖具体类型分支。
    fn content_id(&self) -> &str {
        "example.notes"
    }

    /// 同一实现允许页面和标签，但每个作用域持有自己的实体。
    fn supports(&self, _: ContentKind) -> bool {
        true
    }

    /// 从提供者状态创建实体，证明内容状态实际进入新实例。
    fn create(
        &self,
        _: &ContentScope,
        state: Option<&ContentSavedState>,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        let value = state.map_or_else(|| json!({"text": "new"}), |state| state.value.clone());
        Ok(Box::new(FixtureInstance {
            view: cx.new(|_| FixtureView {
                value,
                identity: None,
            }),
            close_attempts: self.close_attempts.clone(),
            visibility_changes: self.visibility_changes.clone(),
        }))
    }

    /// 把宿主身份交给可编辑视图，模拟视图随后分发状态变化动作。
    fn create_with_identity(
        &self,
        identity: &ContentInstanceIdentity,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        let instance = self.create(identity.scope(), state, window, cx)?;
        let view = instance
            .view()
            .downcast::<FixtureView>()
            .map_err(|_| AppError::invalid("fixture view type mismatch"))?;
        view.update(cx, |view, _| view.identity = Some(identity.clone()));
        Ok(instance)
    }
}

struct FixtureInstance {
    view: Entity<FixtureView>,
    close_attempts: Arc<AtomicUsize>,
    visibility_changes: Arc<AtomicUsize>,
}

struct FailingIdentityFactory {
    inner: FixtureFactory,
    fail_once: AtomicBool,
    identities: Arc<Mutex<Vec<ContentInstanceIdentity>>>,
}

impl ContentFactory for FailingIdentityFactory {
    /// 保持同一贡献身份，使测试只改变创建代次。
    fn content_id(&self) -> &str {
        self.inner.content_id()
    }

    /// 继续复用正式 fixture 的作用域支持规则。
    fn supports(&self, kind: ContentKind) -> bool {
        self.inner.supports(kind)
    }

    /// 旧入口保留原实体构造，供默认工厂行为复用。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.inner.create(scope, state, window, cx)
    }

    /// 模拟失败工厂已经向外发送动作，但在返回实例前报告错误。
    fn create_with_identity(
        &self,
        identity: &ContentInstanceIdentity,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.identities.lock().unwrap().push(identity.clone());
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture creation failed"));
        }
        self.inner.create_with_identity(identity, state, window, cx)
    }
}

impl ContentInstance for FixtureInstance {
    /// 每次返回同一个实体身份，确保重绘和切换不会丢失草稿。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 直接从实体提取当前状态，模拟实际插件的内存草稿。
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState> {
        Ok(ContentSavedState {
            version: 1,
            value: self.view.read(cx).value.clone(),
        })
    }

    /// 重复的可见性通知不应重复触发提供者工作。
    fn visibility_changed(&mut self, _: bool, _: bool, _: &mut App) -> AppResult<()> {
        self.visibility_changes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// 首次失败、重试成功，使测试验证失败期间资源 owner 仍然存在。
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ContentCloseFuture> {
        let attempt = self.close_attempts.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(async move {
            if attempt == 0 {
                Err(AppError::infrastructure("fixture close failed"))
            } else {
                Ok(())
            }
        }))
    }
}

/// 非终端实例可修改并恢复；关闭失败可重试，窗口事件不能重启关闭中的资源。
/// @author kongweiguang
#[gpui_kit::test]
fn registered_factory_owns_page_and_persistent_tab_until_close_ack(app: &mut TestAppContext) {
    let creates = Arc::new(AtomicUsize::new(0));
    let close_attempts = Arc::new(AtomicUsize::new(0));
    let visibility_changes = Arc::new(AtomicUsize::new(0));
    let (_, visual) = app.add_window_view(|_, _| FixtureView {
        value: Value::Null,
        identity: None,
    });
    visual.update(|window, cx| {
        let mut host = ContentHost::default();
        host.register(Arc::new(FixtureFactory {
            creates: creates.clone(),
            close_attempts: close_attempts.clone(),
            visibility_changes: visibility_changes.clone(),
        }))
        .unwrap();
        let scope = ContentScope::Tab(TabId::parse("notes-tab").unwrap());
        let saved = ContentSavedState {
            version: 1,
            value: json!({"text": "restored"}),
        };
        host.open(scope.clone(), "example.notes", Some(&saved), window, cx)
            .unwrap();
        let view = host.view(&scope).unwrap();
        let entity = view.clone().downcast::<FixtureView>().ok().unwrap();
        let identity = entity.read(cx).identity.clone().unwrap();
        assert_eq!(identity.scope(), &scope);
        assert!(host.matches_identity(&identity));
        assert_eq!(entity.read(cx).value, saved.value);
        entity.update(cx, |view, cx| {
            view.value = json!({"text": "draft"});
            cx.notify();
        });
        assert_eq!(
            host.snapshot(&scope, cx).unwrap().value,
            json!({"text": "draft"})
        );
        assert_eq!(host.view(&scope).unwrap().entity_id(), view.entity_id());
        host.visibility_changed(&scope, true, true, cx).unwrap();
        host.visibility_changed(&scope, true, true, cx).unwrap();
        assert_eq!(visibility_changes.load(Ordering::SeqCst), 1);

        let page = ContentScope::Page("example.notes".to_owned());
        host.open(page.clone(), "example.notes", None, window, cx)
            .unwrap();
        assert_ne!(host.view(&page).unwrap().entity_id(), view.entity_id());
        assert!(host.snapshot(&page, cx).is_err());
        let unknown = ContentScope::Tab(TabId::parse("future-tab").unwrap());
        assert!(
            host.open(
                unknown.clone(),
                "example.notes",
                Some(&ContentSavedState {
                    version: 9,
                    value: json!({"future": true}),
                }),
                window,
                cx
            )
            .is_err()
        );
        assert!(host.view(&unknown).is_none());
        assert_eq!(creates.load(Ordering::SeqCst), 2);

        let deadline = Instant::now() + Duration::from_secs(10);
        let (first, future) = host.begin_close(&scope, deadline, cx).unwrap();
        host.visibility_changed(&scope, false, false, cx).unwrap();
        host.visibility_changed(&scope, true, false, cx).unwrap();
        assert_eq!(visibility_changes.load(Ordering::SeqCst), 1);
        assert!(!host.matches_identity(&identity));
        assert!(host.activate(&scope, window, cx).is_err());
        assert!(
            host.complete_close(first.clone(), block_on(future))
                .is_err()
        );
        assert_eq!(host.phase(&scope), Some(ContentPhase::CloseFailed));
        host.visibility_changed(&scope, false, false, cx).unwrap();
        assert_eq!(visibility_changes.load(Ordering::SeqCst), 2);
        assert!(host.matches_identity(&identity));
        assert_eq!(host.view(&scope).unwrap().entity_id(), view.entity_id());
        let (retry, future) = host.begin_close(&scope, deadline, cx).unwrap();
        assert!(host.complete_close(first, Ok(())).is_err());
        host.complete_close(retry, block_on(future)).unwrap();
        assert_eq!(host.phase(&scope), Some(ContentPhase::Closed));
        host.visibility_changed(&scope, true, true, cx).unwrap();
        assert_eq!(visibility_changes.load(Ordering::SeqCst), 2);
        assert!(
            host.open(scope.clone(), "example.notes", Some(&saved), window, cx)
                .is_err()
        );
        host.finalize_close(&scope).unwrap();
        host.open(scope.clone(), "example.notes", Some(&saved), window, cx)
            .unwrap();
        let reopened = host
            .view(&scope)
            .unwrap()
            .downcast::<FixtureView>()
            .ok()
            .unwrap();
        let reopened_identity = reopened.read(cx).identity.clone().unwrap();
        assert_ne!(reopened_identity, identity);
        assert!(!host.matches_identity(&identity));
        assert!(host.matches_identity(&reopened_identity));
        assert_eq!(host.snapshot(&scope, cx).unwrap(), saved);
    });
}

/// 工厂失败后即使同一作用域立即重建，失败实例泄露的旧通知也不能更新新实例。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_creation_consumes_the_instance_identity(app: &mut TestAppContext) {
    let identities = Arc::new(Mutex::new(Vec::new()));
    let (_, visual) = app.add_window_view(|_, _| FixtureView {
        value: Value::Null,
        identity: None,
    });
    visual.update(|window, cx| {
        let mut host = ContentHost::default();
        host.register(Arc::new(FailingIdentityFactory {
            inner: FixtureFactory {
                creates: Arc::new(AtomicUsize::new(0)),
                close_attempts: Arc::new(AtomicUsize::new(0)),
                visibility_changes: Arc::new(AtomicUsize::new(0)),
            },
            fail_once: AtomicBool::new(true),
            identities: identities.clone(),
        }))
        .unwrap();
        let scope = ContentScope::Tab(TabId::parse("identity-retry").unwrap());
        assert!(
            host.open(scope.clone(), "example.notes", None, window, cx)
                .is_err()
        );
        host.open(scope, "example.notes", None, window, cx).unwrap();
        let identities = identities.lock().unwrap();
        assert_eq!(identities.len(), 2);
        assert_ne!(identities[0], identities[1]);
        assert!(!host.matches_identity(&identities[0]));
        assert!(host.matches_identity(&identities[1]));
    });
}
