//! 中央页面与工作区标签共用的内容工厂和实例所有权。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Instant};

use gpui_kit::{AnyView, App, Window};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::identity::TabId;
use serde_json::Value;

/// 页面与持久标签具有不同身份和生命周期，不能用同一个空标签 ID 表示。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContentScope {
    Page(String),
    Tab(TabId),
}

/// 同一作用域可以关闭后重新打开；代次使旧实例的通知不能影响新实例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentInstanceIdentity {
    scope: ContentScope,
    generation: u64,
}

impl ContentInstanceIdentity {
    /// 调用方只能读取身份所属作用域，代次由宿主分配和校验。
    pub fn scope(&self) -> &ContentScope {
        &self.scope
    }
}

/// 可持久化的内容在状态改变后分发此动作；宿主会先核对实例代次。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.content.stateChanged", no_json, no_register)]
pub struct ContentStateChanged {
    pub identity: ContentInstanceIdentity,
}

/// 工厂必须显式声明允许的宿主类型，防止页面被当作可恢复的工作区标签。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentKind {
    Page,
    Tab,
}

/// 提供者必须看到状态版本；未知版本由提供者拒绝并保留原文等待后续恢复。
#[derive(Clone, Debug, PartialEq)]
pub struct ContentSavedState {
    pub version: u32,
    pub value: Value,
}

/// 关闭操作由共享执行器驱动；实例仍由 UI 宿主持有，直到收到成功回执。
pub type ContentCloseFuture = Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'static>>;

/// 内容实现持有真实视图和资源；插件服务由工厂捕获，不把 Root 暴露给插件。
pub trait ContentFactory: Send + Sync + 'static {
    /// 返回跨重启稳定的贡献身份，宿主据此匹配注册项和保存状态。
    fn content_id(&self) -> &str;

    /// 分别声明页面和标签能力，避免恢复时误把其他类型的贡献当成标签。
    fn supports(&self, kind: ContentKind) -> bool;

    /// 缺省只接受首版状态；提供者显式支持其他版本后才能尝试恢复。
    fn supports_state_version(&self, version: u32) -> bool {
        version == 1
    }

    /// 新建或恢复一个实例；创建失败时工厂应自行保留或回收已申请的资源。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>>;

    /// 可编辑内容覆写此入口并保存身份，以便用 ContentStateChanged 通知宿主保存。
    /// 保留旧入口供没有持久状态变化的内容使用。
    fn create_with_identity(
        &self,
        identity: &ContentInstanceIdentity,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        self.create(identity.scope(), state, window, cx)
    }
}

/// 每个标签拥有独立实体；页面可按自身稳定身份复用，不能共享标签的状态。
pub trait ContentInstance {
    /// 返回同一 GPUI 实体的类型擦除句柄，使重绘不重建视图或丢失焦点。
    fn view(&self) -> AnyView;

    /// 只提取可持久化的插件状态；异步 I/O 和写盘由应用层统一执行。
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState>;

    /// 激活回调发生在导航提交后，避免在绘制过程中启动插件副作用。
    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> AppResult<()> {
        Ok(())
    }

    /// 同时传递面板可见性与窗口激活状态，插件可暂停隐藏时不必要的刷新。
    fn visibility_changed(
        &mut self,
        _visible: bool,
        _window_active: bool,
        _cx: &mut App,
    ) -> AppResult<()> {
        Ok(())
    }

    /// 只有显式焦点导航才调用，普通重绘不应抢走用户正在操作的控件。
    fn focus(&mut self, _window: &mut Window, _cx: &mut App) -> AppResult<()> {
        Ok(())
    }

    /// 用调用方给出的绝对截止时间创建关闭任务，失败时实例仍是资源 owner。
    fn begin_close(&mut self, deadline: Instant, cx: &mut App) -> AppResult<ContentCloseFuture>;
}

/// 操作代次阻止过期关闭回执移除已重试或重新创建的实例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentCloseToken {
    scope: ContentScope,
    generation: u64,
    deadline: Instant,
}

/// 关闭完成和释放实例是两个步骤，确保工作区候选提交失败时不会重建已关闭资源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentPhase {
    Open,
    Closing,
    CloseFailed,
    Closed,
}

struct HostedContent {
    content_id: String,
    identity: ContentInstanceIdentity,
    instance: Box<dyn ContentInstance>,
    phase: ContentPhase,
    closing_generation: Option<u64>,
    visibility: Option<(bool, bool)>,
}

/// 工厂和实例只在所属窗口线程访问，资源关闭任务本身可交给共享执行器。
#[derive(Default)]
pub struct ContentHost {
    factories: BTreeMap<String, Arc<dyn ContentFactory>>,
    instances: BTreeMap<ContentScope, HostedContent>,
    next_generation: u64,
    next_instance_generation: u64,
}

impl ContentHost {
    /// 注册唯一内容身份；重复注册必须报错，不能悄悄覆盖已有插件。
    pub fn register(&mut self, factory: Arc<dyn ContentFactory>) -> AppResult<()> {
        let content_id = factory.content_id();
        if content_id.trim().is_empty() || content_id.chars().any(char::is_control) {
            return Err(AppError::invalid("内容贡献身份无效"));
        }
        if self.factories.contains_key(content_id) {
            return Err(AppError::conflict("内容贡献身份重复"));
        }
        self.factories.insert(content_id.to_owned(), factory);
        Ok(())
    }

    /// 通过注册的工厂创建作用域内唯一实例；未知内容返回不可用，调用方保留原状态。
    pub fn open(
        &mut self,
        scope: ContentScope,
        content_id: &str,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<()> {
        if self.instances.contains_key(&scope) {
            return Err(AppError::conflict("内容实例已存在"));
        }
        let kind = match &scope {
            ContentScope::Page(page_id) if page_id == content_id && state.is_none() => {
                ContentKind::Page
            }
            ContentScope::Page(_) => return Err(AppError::invalid("中央页面身份或状态无效")),
            ContentScope::Tab(_) => ContentKind::Tab,
        };
        let factory = self
            .factories
            .get(content_id)
            .ok_or_else(|| AppError::not_found("内容贡献不可用"))?;
        if !factory.supports(kind) {
            return Err(AppError::unsupported("内容贡献不支持此作用域"));
        }
        if state.is_some_and(|saved| !factory.supports_state_version(saved.version)) {
            return Err(AppError::unsupported("内容状态版本不受支持"));
        }
        let generation = self
            .next_instance_generation
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("内容实例代次已耗尽"))?;
        // 创建失败也消耗代次，防止工厂遗留的迟到通知命中下一次创建。
        self.next_instance_generation = generation;
        let identity = ContentInstanceIdentity {
            scope: scope.clone(),
            generation,
        };
        let instance = factory.create_with_identity(&identity, state, window, cx)?;
        self.instances.insert(
            scope,
            HostedContent {
                content_id: content_id.to_owned(),
                identity,
                instance,
                phase: ContentPhase::Open,
                closing_generation: None,
                visibility: None,
            },
        );
        Ok(())
    }

    /// 复用已创建的实体，关闭失败的视图继续可见并可由宿主提供重试。
    pub fn view(&self, scope: &ContentScope) -> Option<AnyView> {
        self.instances
            .get(scope)
            .filter(|entry| entry.phase != ContentPhase::Closed)
            .map(|entry| entry.instance.view())
    }

    /// 保留实体不代表可以展示；只有对应窗口的显示通知成功后才允许挂载正文。
    /// 原始 view 仍供快照、关闭及恢复使用，失败时不丢弃资源 owner。
    pub fn view_for_mount(&self, scope: &ContentScope, window_active: bool) -> Option<AnyView> {
        self.instances
            .get(scope)
            .filter(|entry| {
                entry.phase != ContentPhase::Closed
                    && entry.visibility == Some((true, window_active))
            })
            .map(|entry| entry.instance.view())
    }

    /// 按实例身份核对提供者，避免同一 TabId 的错误注册项覆盖已有视图。
    pub fn content_id(&self, scope: &ContentScope) -> Option<&str> {
        self.instances
            .get(scope)
            .map(|entry| entry.content_id.as_str())
    }

    /// 只接受仍可编辑的原实例通知，关闭或重建后旧动作不能更新新标签状态。
    pub fn matches_identity(&self, identity: &ContentInstanceIdentity) -> bool {
        self.instances.get(identity.scope()).is_some_and(|entry| {
            entry.identity == *identity
                && matches!(entry.phase, ContentPhase::Open | ContentPhase::CloseFailed)
        })
    }

    /// 返回实例身份快照供桌面退出逐项关闭；包括失败和等待提交的实例。
    pub fn scopes(&self) -> Vec<ContentScope> {
        self.instances.keys().cloned().collect()
    }

    /// 显式暴露关闭阶段，调用方可区分进行中、可重试和等待提交的资源。
    pub fn phase(&self, scope: &ContentScope) -> Option<ContentPhase> {
        self.instances.get(scope).map(|entry| entry.phase)
    }

    /// 将明确的导航提交转发给既有实例，不在渲染回调中重复初始化。
    pub fn activate(
        &mut self,
        scope: &ContentScope,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<()> {
        let entry = self
            .instances
            .get_mut(scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if matches!(entry.phase, ContentPhase::Closing | ContentPhase::Closed) {
            return Err(AppError::conflict("内容实例正在关闭或已关闭"));
        }
        entry.instance.activate(window, cx)
    }

    /// 可见性与窗口活动状态由外层统一决定；关闭中的 owner 只等待回执，不能被窗口事件重新激活。
    pub fn visibility_changed(
        &mut self,
        scope: &ContentScope,
        visible: bool,
        window_active: bool,
        cx: &mut App,
    ) -> AppResult<()> {
        let entry = self
            .instances
            .get_mut(scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if matches!(entry.phase, ContentPhase::Closing | ContentPhase::Closed) {
            return Ok(());
        }
        if entry.visibility == Some((visible, window_active)) {
            return Ok(());
        }
        entry
            .instance
            .visibility_changed(visible, window_active, cx)?;
        entry.visibility = Some((visible, window_active));
        Ok(())
    }

    /// 只向指定实例请求焦点，缺失时不切换到别的实例。
    pub fn focus(
        &mut self,
        scope: &ContentScope,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<()> {
        let entry = self
            .instances
            .get_mut(scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if matches!(entry.phase, ContentPhase::Closing | ContentPhase::Closed) {
            return Err(AppError::conflict("内容实例正在关闭或已关闭"));
        }
        entry.instance.focus(window, cx)
    }

    /// 页面不是持久化标签；只有 Tab 可以生成待交给 CAS 流程的插件状态。
    pub fn snapshot(&self, scope: &ContentScope, cx: &App) -> AppResult<ContentSavedState> {
        if !matches!(scope, ContentScope::Tab(_)) {
            return Err(AppError::invalid("中央页面不属于持久标签"));
        }
        let entry = self
            .instances
            .get(scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if matches!(entry.phase, ContentPhase::Closing | ContentPhase::Closed) {
            return Err(AppError::conflict("内容实例正在关闭或已关闭"));
        }
        let state = entry.instance.snapshot(cx)?;
        let factory = self
            .factories
            .get(&entry.content_id)
            .ok_or_else(|| AppError::not_found("内容贡献不可用"))?;
        if !factory.supports_state_version(state.version) {
            return Err(AppError::unsupported("内容状态版本不受支持"));
        }
        Ok(state)
    }

    /// 先保留实例再启动异步关闭；同一实例在前次回执之前不能重复关闭。
    pub fn begin_close(
        &mut self,
        scope: &ContentScope,
        deadline: Instant,
        cx: &mut App,
    ) -> AppResult<(ContentCloseToken, ContentCloseFuture)> {
        let entry = self
            .instances
            .get_mut(scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if matches!(entry.phase, ContentPhase::Closing | ContentPhase::Closed) {
            return Err(AppError::conflict("内容实例正在关闭或已关闭"));
        }
        if deadline <= Instant::now() {
            return Err(AppError::Timeout {
                operation: "关闭内容实例",
            });
        }
        let generation = self
            .next_generation
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("内容关闭代次已耗尽"))?;
        let future = match entry.instance.begin_close(deadline, cx) {
            Ok(future) => future,
            Err(error) => {
                entry.phase = ContentPhase::CloseFailed;
                return Err(error);
            }
        };
        self.next_generation = generation;
        entry.phase = ContentPhase::Closing;
        entry.closing_generation = Some(generation);
        Ok((
            ContentCloseToken {
                scope: scope.clone(),
                generation,
                deadline,
            },
            future,
        ))
    }

    /// 仅匹配当前操作的成功回执才进入已关闭阶段；失败时继续保留资源 owner。
    pub fn complete_close(
        &mut self,
        token: ContentCloseToken,
        result: AppResult<()>,
    ) -> AppResult<()> {
        let entry = self
            .instances
            .get_mut(&token.scope)
            .ok_or_else(|| AppError::not_found("内容实例不存在"))?;
        if entry.phase != ContentPhase::Closing
            || entry.closing_generation != Some(token.generation)
        {
            return Err(AppError::conflict("内容关闭回执已过期"));
        }
        entry.closing_generation = None;
        if result.is_ok() && Instant::now() > token.deadline {
            entry.phase = ContentPhase::CloseFailed;
            return Err(AppError::Timeout {
                operation: "关闭内容实例",
            });
        }
        match result {
            Ok(()) => {
                entry.phase = ContentPhase::Closed;
                Ok(())
            }
            Err(error) => {
                entry.phase = ContentPhase::CloseFailed;
                Err(error)
            }
        }
    }

    /// 仅在工作区确认关闭后移除已关闭实例，防止候选提交失败时自动恢复旧资源。
    pub fn finalize_close(&mut self, scope: &ContentScope) -> AppResult<()> {
        match self.phase(scope) {
            Some(ContentPhase::Closed) => {
                self.instances.remove(scope);
                Ok(())
            }
            Some(_) => Err(AppError::conflict("内容实例尚未完成关闭")),
            None => Err(AppError::not_found("内容实例不存在")),
        }
    }
}
