//! 终端搜索控件只发出 SessionService 搜索请求和命中导航意图。
//!
//! @author kongweiguang

use std::{sync::Arc, time::Duration};

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, ClickEvent, Context, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, Pixels, Render,
    Subscription, Task, Window,
    assets::IconName,
    base::Selectable as _,
    component::button::{Button, ButtonVariants as _},
    component::{
        Disableable,
        input::{Input, InputEvent, InputState},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{
    TerminalSearchMatch, TerminalSearchRequest, TerminalSearchResult,
};

use crate::{gpui::Focusable as _, theme::ThemeTokens};

const MAX_SEARCH_QUERY_BYTES: usize = 1024;
const MAX_SEARCH_MATCHES: usize = 512;
const SEARCH_PREVIEW_DEBOUNCE: Duration = Duration::from_millis(200);
const SEARCH_IME_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SEARCH_COMPACT_BREAKPOINT: f32 = 400.0;
const SEARCH_OVERLAY_WIDTH: f32 = 440.0;

/// 搜索栏将异步查找与当前命中定位交给 Root；跨输出导航只传查询与目标索引，不传旧快照坐标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalSearchEvent {
    Search {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        navigate: bool,
        safe_top_rows: u16,
    },
    SelectMatch {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        selected_match: usize,
        safe_top_rows: u16,
    },
    Refresh {
        session_id: SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        selected_match: Option<usize>,
    },
    Clear {
        session_id: SessionId,
    },
    Close {
        session_id: SessionId,
    },
}

/// 搜索浮层在顶部无法避开命中时转到底部，位置保存在 pane 的稳定搜索实体上。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchOverlayPlacement {
    #[default]
    TopRight,
    BottomRight,
}

/// 查询未变化但结果为空时，Enter 仍需重新读取终端历史，避免零命中结果永久停留。
///
/// @author kongweiguang
fn should_submit_before_navigation(
    submitted_query_revision: Option<u64>,
    query_revision: u64,
    match_count: usize,
) -> bool {
    submitted_query_revision != Some(query_revision) || match_count == 0
}

/// 将本地环绕导航结果转成新结果集的目标索引；Root 会在新快照上重查并夹取该索引。
///
/// @author kongweiguang
fn next_search_match_index(
    selected: Option<usize>,
    count: usize,
    backwards: bool,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let selected = selected.unwrap_or(0).min(count - 1);
    Some(if backwards {
        (selected + count - 1) % count
    } else {
        (selected + 1) % count
    })
}

/// 只在输入代次仍匹配时复用精确提交过的查询和命中索引，防止视图刷新重放草稿。
///
/// @author kongweiguang
fn submitted_search_refresh_request(
    submitted_query_revision: Option<u64>,
    query_revision: u64,
    request: Option<&TerminalSearchRequest>,
    selected_match: Option<usize>,
) -> Option<(TerminalSearchRequest, Option<usize>)> {
    if submitted_query_revision != Some(query_revision) {
        return None;
    }
    Some((request?.clone(), selected_match))
}

/// 搜索栏的输入与命中索引属于视图状态，终端会话和快照仍由 Root 持有。
pub struct TerminalSearchToolbar {
    session_id: Option<SessionId>,
    query: Entity<InputState>,
    window_handle: AnyWindowHandle,
    case_sensitive: bool,
    regex: bool,
    matches: Arc<[TerminalSearchMatch]>,
    selected_match: Option<usize>,
    request_id: u64,
    query_revision: u64,
    submitted_query_revision: Option<u64>,
    last_submitted_request: Option<TerminalSearchRequest>,
    last_request_navigates: bool,
    /// 记录当前按键周期内是否由 IME 组合态发起 Enter，避免组合确认被误当成搜索导航。
    suppress_composition_enter: bool,
    /// 输入动作会先于元素 capture 回调执行，记录组合态 Esc 供后续回调放行给输入法。
    suppress_composition_escape: bool,
    truncated: bool,
    busy: bool,
    error: Option<String>,
    is_open: bool,
    suspended: bool,
    debounce_task: Option<Task<()>>,
    debounce_generation: u64,
    available_width: Pixels,
    safe_top_rows: u16,
    overlay_placement: SearchOverlayPlacement,
    observed_query: String,
    observed_composition: bool,
    _query_subscription: Subscription,
    _query_observer: Subscription,
    _ime_key_subscription: Subscription,
}

impl EventEmitter<TerminalSearchEvent> for TerminalSearchToolbar {}

impl TerminalSearchToolbar {
    /// 返回 toolbar 自己维护的请求代次，供父 view 在目标切换后同步门禁基线而不重复猜测递增次数。
    /// 代次只由搜索实体推进，避免异步 Clear 被旧 session 过滤时父子计数分叉。
    ///
    /// @author kongweiguang
    pub(crate) fn current_request_id(&self) -> u64 {
        self.request_id
    }

    /// 以固定 session 和 GPUI 输入状态创建控件，避免搜索因 Tab 切换转投其它终端。
    ///
    /// @author kongweiguang
    pub fn new(session_id: Option<SessionId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索终端内容",
                "Search terminal output",
            ))
        });
        let query_for_events = query.clone();
        let window_handle = window.window_handle();
        let ime_query = query.clone();
        let ime_focus_handle = query.read(cx).focus_handle(cx);
        let ime_toolbar = cx.weak_entity();
        let ime_key_subscription = cx.intercept_keystrokes(move |event, window, app| {
            let key = event.keystroke.key.as_str();
            let is_enter = key.eq_ignore_ascii_case("enter");
            let is_escape = key.eq_ignore_ascii_case("escape");
            if (!is_enter && !is_escape) || !ime_focus_handle.is_focused(window) {
                return;
            }
            let is_composing = ime_query.update(app, |input, cx| {
                input.marked_text_range(window, cx).is_some()
            });
            if !is_composing {
                return;
            }

            if let Some(toolbar) = ime_toolbar.upgrade() {
                toolbar.update(app, |toolbar, _| {
                    if is_enter {
                        toolbar.suppress_composition_enter = true;
                    } else {
                        toolbar.suppress_composition_escape = true;
                    }
                });
            }

            // Interceptors run before InputState dispatches Enter/Escape. Clear after that
            // dispatch so an IME-consumed key cannot leave a stale one-shot suppression flag.
            let toolbar_for_cleanup = ime_toolbar.clone();
            window.defer(app, move |_, app| {
                if let Some(toolbar) = toolbar_for_cleanup.upgrade() {
                    toolbar.update(app, |toolbar, _| {
                        if is_enter {
                            toolbar.suppress_composition_enter = false;
                        } else {
                            toolbar.suppress_composition_escape = false;
                        }
                    });
                }
            });
        });
        let query_subscription = cx.subscribe(&query, move |toolbar, _, event, cx| match event {
            InputEvent::Change => toolbar.observe_query_input(cx),
            InputEvent::PressEnter {
                secondary: false,
                shift,
            } => {
                if toolbar.suppress_composition_enter {
                    toolbar.suppress_composition_enter = false;
                    return;
                }
                let composition_state = window_handle.update(cx, |_, window, app| {
                    query_for_events.update(app, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    })
                });
                if matches!(composition_state, Ok(false)) {
                    toolbar.navigate_or_submit(*shift, cx);
                }
            }
            InputEvent::PressEnter { .. } => {}
            InputEvent::Focus | InputEvent::Blur => {}
        });
        let query_observer = cx.observe(&query, |toolbar, _, cx| toolbar.observe_query_input(cx));
        Self {
            session_id,
            query,
            window_handle,
            case_sensitive: false,
            regex: false,
            matches: Arc::from([]),
            selected_match: None,
            request_id: 0,
            query_revision: 0,
            submitted_query_revision: None,
            last_submitted_request: None,
            last_request_navigates: false,
            suppress_composition_enter: false,
            suppress_composition_escape: false,
            truncated: false,
            busy: false,
            error: None,
            is_open: false,
            suspended: false,
            debounce_task: None,
            debounce_generation: 0,
            available_width: px(SEARCH_OVERLAY_WIDTH),
            safe_top_rows: 0,
            overlay_placement: SearchOverlayPlacement::TopRight,
            observed_query: String::new(),
            observed_composition: false,
            _query_subscription: query_subscription,
            _query_observer: query_observer,
            _ime_key_subscription: ime_key_subscription,
        }
    }

    /// 切换目标 session 时清除旧结果，防止把上一个终端的坐标滚到新终端。
    pub fn set_session(
        &mut self,
        session_id: Option<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_id == session_id {
            return;
        }
        self.session_id = session_id;
        self.clear_local(window, cx);
    }

    /// 只更新固定搜索实体的提示文案，保留查询草稿、焦点、订阅和命中结果。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索终端内容", "Search terminal output"),
                window,
                cx,
            );
        });
    }

    /// 打开时稳定复用同一输入实体并选中查询；只有重开时重启预览，避免焦点抖动重扫。
    ///
    /// @author kongweiguang
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reopened = !self.is_open;
        self.is_open = true;
        self.query.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        if reopened && !self.suspended {
            self.schedule_query_preview(cx);
        }
    }

    /// 目标暂时隐藏或失效时撤销所有在途搜索，同时保留编辑草稿和浮层开关状态供恢复使用。
    /// 无 session 时不存在可回投的有效请求，因此只推进查询代次；request_id 留给 view 与后续绑定共同递增。
    ///
    /// @author kongweiguang
    pub fn suspend_pending(&mut self, cx: &mut Context<Self>) -> bool {
        if self.suspended {
            return false;
        }
        self.cancel_debounce_task();
        self.suspended = true;
        self.query_revision = self.query_revision.wrapping_add(1);
        self.submitted_query_revision = None;
        self.last_submitted_request = None;
        self.last_request_navigates = false;
        self.suppress_composition_enter = false;
        self.suppress_composition_escape = false;
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.truncated = false;
        self.busy = false;
        self.error = None;
        if let Some(session_id) = self.session_id.clone() {
            // 只有绑定目标后才存在可撤销请求；无 session 时让后续 set_session 与 view 同步递增。
            self.request_id = self.request_id.wrapping_add(1);
            cx.emit(TerminalSearchEvent::Clear { session_id });
        }
        cx.notify();
        true
    }

    /// 会话重新可见时仅恢复仍打开浮层的当前查询，先清除挂起标志再启动防抖避免重复恢复。
    ///
    /// @author kongweiguang
    pub fn resume_pending(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.suspended {
            return false;
        }
        self.suspended = false;
        if self.is_open {
            self.schedule_query_preview(cx);
        }
        cx.notify();
        true
    }

    /// 接收 host 测得的可用宽度与顶部安全行；尺寸未变时不通知以避免布局回调循环。
    ///
    /// @author kongweiguang
    pub fn set_overlay_metrics(
        &mut self,
        available_width: Pixels,
        safe_top_rows: u16,
        cx: &mut Context<Self>,
    ) {
        if self.available_width == available_width && self.safe_top_rows == safe_top_rows {
            return;
        }
        self.available_width = available_width;
        self.safe_top_rows = safe_top_rows;
        cx.notify();
    }

    /// 返回 pane 当前的浮层锚点，host 可按命中避让结果换到右下角而不重建输入。
    ///
    /// @author kongweiguang
    pub fn overlay_placement(&self) -> SearchOverlayPlacement {
        self.overlay_placement
    }

    /// 返回查询框自己的焦点句柄，供关闭 ACK 精确判断用户仍把焦点留在该终端搜索框。
    /// 句柄按搜索 Entity 生成，不能用通用 Editor scope 推断，否则会把其它输入框纳入交接。
    ///
    /// @author kongweiguang
    pub(crate) fn query_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    /// 切换 session 时清空查询草稿并撤销全部异步代次，避免旧 pane 状态串到新目标。
    ///
    /// @author kongweiguang
    pub fn clear_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_debounce_task();
        self.is_open = false;
        self.suspended = false;
        self.request_id = self.request_id.wrapping_add(1);
        self.query_revision = self.query_revision.wrapping_add(1);
        self.submitted_query_revision = None;
        self.last_submitted_request = None;
        self.last_request_navigates = false;
        self.suppress_composition_enter = false;
        self.suppress_composition_escape = false;
        self.query
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.truncated = false;
        self.busy = false;
        self.error = None;
        self.overlay_placement = SearchOverlayPlacement::TopRight;
        cx.notify();
    }

    /// 关闭结束原输入法标记并撤销运行态任务，保留文字与选项，避免重开后残留组合态阻塞查询。
    ///
    /// @author kongweiguang
    pub fn dismiss_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_debounce_task();
        self.query
            .update(cx, |input, cx| input.unmark_text(window, cx));
        self.observed_composition = false;
        self.is_open = false;
        self.suspended = false;
        self.request_id = self.request_id.wrapping_add(1);
        self.query_revision = self.query_revision.wrapping_add(1);
        self.submitted_query_revision = None;
        self.last_submitted_request = None;
        self.last_request_navigates = false;
        self.suppress_composition_enter = false;
        self.suppress_composition_escape = false;
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.truncated = false;
        self.busy = false;
        self.error = None;
        cx.notify();
    }

    /// 仅接收仍有效的 session/request 回执；安全边距结果同时决定浮层是否需要移到底部。
    ///
    /// @author kongweiguang
    pub fn set_result(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        result: TerminalSearchResult,
        reveal_safe_margin_reached: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.suspended
            || self.session_id.as_ref() != Some(session_id)
            || self.request_id != request_id
        {
            return;
        }
        self.matches = result.matches;
        self.selected_match = result
            .selected_match
            .filter(|index| *index < self.matches.len());
        self.truncated = result.truncated;
        self.busy = false;
        self.error = None;
        match reveal_safe_margin_reached {
            Some(false) => self.overlay_placement = SearchOverlayPlacement::BottomRight,
            Some(true) => {
                self.overlay_placement = SearchOverlayPlacement::TopRight;
            }
            None => {}
        }
        cx.notify();
    }

    /// 显示查询或应用层错误并丢弃旧查询基线，避免视图刷新重放失败请求。
    ///
    /// @author kongweiguang
    pub fn set_error(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if self.suspended
            || self.session_id.as_ref() != Some(session_id)
            || self.request_id != request_id
        {
            return;
        }
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.last_submitted_request = None;
        self.truncated = false;
        self.busy = false;
        self.error = Some(message.into());
        cx.notify();
    }

    /// 仅投影当前异步代次的忙碌状态，过期回执不能覆盖挂起或新查询状态。
    ///
    /// @author kongweiguang
    pub fn set_busy(
        &mut self,
        session_id: &SessionId,
        request_id: u64,
        busy: bool,
        cx: &mut Context<Self>,
    ) {
        if self.suspended
            || self.session_id.as_ref() != Some(session_id)
            || self.request_id != request_id
        {
            return;
        }
        self.busy = busy;
        cx.notify();
    }

    /// 读取当前输入构造请求；保存最后一次已提交选项，供纯展示刷新复用而不重建查询。
    ///
    /// @author kongweiguang
    fn submit(&mut self, backwards: bool, navigate: bool, cx: &mut Context<Self>) {
        self.cancel_debounce_task();
        let query = self.query.read(cx).value().to_owned();
        let Some(session_id) = self.session_id.clone().filter(|_| !self.suspended) else {
            return;
        };
        if query.trim().is_empty() {
            self.request_id = self.request_id.wrapping_add(1);
            self.submitted_query_revision = None;
            self.last_submitted_request = None;
            self.last_request_navigates = false;
            self.matches = Arc::from([]);
            self.selected_match = None;
            self.truncated = false;
            self.error = None;
            self.busy = false;
            cx.emit(TerminalSearchEvent::Clear { session_id });
            cx.notify();
            return;
        }
        if query.len() > MAX_SEARCH_QUERY_BYTES {
            self.request_id = self.request_id.wrapping_add(1);
            self.submitted_query_revision = Some(self.query_revision);
            self.last_submitted_request = None;
            self.last_request_navigates = false;
            self.matches = Arc::from([]);
            self.selected_match = None;
            self.truncated = false;
            self.busy = false;
            cx.emit(TerminalSearchEvent::Clear {
                session_id: session_id.clone(),
            });
            self.set_error(
                &session_id,
                self.request_id,
                crate::locale::text(
                    "搜索内容不能超过 1 KiB",
                    "Search queries cannot exceed 1 KiB.",
                ),
                cx,
            );
            return;
        }
        self.error = None;
        self.busy = true;
        self.request_id = self.request_id.wrapping_add(1);
        self.submitted_query_revision = Some(self.query_revision);
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.truncated = false;
        let request = TerminalSearchRequest {
            query: query.to_string(),
            case_sensitive: self.case_sensitive,
            regex: self.regex,
            backwards,
            max_matches: MAX_SEARCH_MATCHES,
        };
        self.last_submitted_request = Some(request.clone());
        self.last_request_navigates = navigate;
        cx.emit(TerminalSearchEvent::Search {
            session_id,
            request_id: self.request_id,
            request,
            navigate,
            safe_top_rows: self.safe_top_rows,
        });
        cx.notify();
    }

    /// 仅对仍对应当前输入的已提交查询重发刷新事件，保留旧命中索引且不改焦点。
    ///
    /// @author kongweiguang
    pub(crate) fn refresh_after_view_revision(&mut self, cx: &mut Context<Self>) -> bool {
        if self.suspended {
            return false;
        }
        let Some(session_id) = self.session_id.clone() else {
            return false;
        };
        let Some((request, selected_match)) = submitted_search_refresh_request(
            self.submitted_query_revision,
            self.query_revision,
            self.last_submitted_request.as_ref(),
            self.selected_match,
        ) else {
            return false;
        };

        self.request_id = self.request_id.wrapping_add(1);
        self.busy = true;
        self.error = None;
        cx.emit(TerminalSearchEvent::Refresh {
            session_id,
            request_id: self.request_id,
            request,
            selected_match,
        });
        cx.notify();
        true
    }

    /// IME 预编辑只 notify、不一定发 Change；按文字和组合标记去重观察，光标闪烁或选择不重查。
    ///
    /// @author kongweiguang
    fn observe_query_input(&mut self, cx: &mut Context<Self>) {
        let value = self.query.read(cx).value().to_string();
        let query = self.query.clone();
        let composing = self
            .window_handle
            .update(cx, |_, window, app| {
                query.update(app, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                })
            })
            .unwrap_or(false);
        if self.observed_query == value && self.observed_composition == composing {
            return;
        }
        self.observed_query = value;
        self.observed_composition = composing;
        self.invalidate_query(cx);
    }

    /// 输入或搜索选项变化时作废旧命中与刷新基线，避免主题回调重放过期查询。
    ///
    /// @author kongweiguang
    fn invalidate_query(&mut self, cx: &mut Context<Self>) {
        self.cancel_debounce_task();
        self.query_revision = self.query_revision.wrapping_add(1);
        self.request_id = self.request_id.wrapping_add(1);
        self.submitted_query_revision = None;
        self.last_submitted_request = None;
        self.matches = Arc::from([]);
        self.selected_match = None;
        self.truncated = false;
        self.busy = false;
        self.error = None;
        if let Some(session_id) = self.session_id.clone() {
            cx.emit(TerminalSearchEvent::Clear { session_id });
        }
        self.schedule_query_preview(cx);
        cx.notify();
    }

    /// 输入编辑先撤销上一代任务，再延迟提交最新查询；IME 标记事务结束后重新等待完整间隔。
    ///
    /// @author kongweiguang
    fn schedule_query_preview(&mut self, cx: &mut Context<Self>) {
        self.cancel_debounce_task();
        if !self.is_open
            || self.suspended
            || self.session_id.is_none()
            || self.query.read(cx).value().trim().is_empty()
        {
            return;
        }

        let query_revision = self.query_revision;
        let debounce_generation = self.debounce_generation;
        let query_entity = self.query.clone();
        let mut await_composition_commit = self
            .window_handle
            .update(cx, |_, window, app| {
                query_entity.update(app, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                })
            })
            .unwrap_or(false);
        let initial_delay = if await_composition_commit {
            SEARCH_IME_POLL_INTERVAL
        } else {
            SEARCH_PREVIEW_DEBOUNCE
        };
        let window_handle = self.window_handle;
        self.debounce_task = Some(cx.spawn(async move |toolbar_entity, cx| {
            let mut delay = initial_delay;
            loop {
                cx.background_executor().timer(delay).await;
                let task_is_current = toolbar_entity.update(cx, |toolbar, _| {
                    toolbar.is_open
                        && !toolbar.suspended
                        && toolbar.query_revision == query_revision
                        && toolbar.debounce_generation == debounce_generation
                });
                if !matches!(task_is_current, Ok(true)) {
                    break;
                }
                let composition_state = window_handle.update(cx, |_, window, app| {
                    query_entity.update(app, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    })
                });
                let Ok(is_composing) = composition_state else {
                    break;
                };
                if is_composing {
                    await_composition_commit = true;
                    delay = SEARCH_IME_POLL_INTERVAL;
                    continue;
                }
                if await_composition_commit {
                    let _ = toolbar_entity.update(cx, |toolbar, _| {
                        if toolbar.debounce_generation == debounce_generation {
                            toolbar.observed_composition = false;
                        }
                    });
                    await_composition_commit = false;
                    delay = SEARCH_PREVIEW_DEBOUNCE;
                    continue;
                }

                let _ = toolbar_entity.update(cx, |toolbar, cx| {
                    if toolbar.is_open
                        && !toolbar.suspended
                        && toolbar.query_revision == query_revision
                        && toolbar.debounce_generation == debounce_generation
                        && toolbar.submitted_query_revision != Some(query_revision)
                    {
                        toolbar.submit(false, false, cx);
                    }
                });
                break;
            }
        }));
    }

    /// 句柄丢弃可能遇到已排队的 timer 回调，因此递增代次并在每次回投处校验。
    ///
    /// @author kongweiguang
    fn cancel_debounce_task(&mut self) {
        self.debounce_task = None;
        self.debounce_generation = self.debounce_generation.wrapping_add(1);
    }

    /// Enter 在查询变化后启动搜索，否则环绕导航；Shift+Enter 沿用旧版反向查找习惯。
    ///
    /// @author kongweiguang
    fn navigate_or_submit(&mut self, backwards: bool, cx: &mut Context<Self>) {
        if self.busy || self.session_id.is_none() || self.suspended {
            if self.busy && !self.last_request_navigates && self.selected_match.is_none() {
                self.submit(backwards, true, cx);
            }
            return;
        }
        let query = self.query.read(cx).value().to_owned();
        if query.trim().is_empty() {
            self.submit(backwards, true, cx);
            return;
        }
        if should_submit_before_navigation(
            self.submitted_query_revision,
            self.query_revision,
            self.matches.len(),
        ) || self.selected_match.is_none()
        {
            self.submit(backwards, true, cx);
        } else {
            self.move_match(backwards, cx);
        }
    }

    /// × 与 Esc 共用关闭事件；组合态拦截只属于键盘 Esc，显式点击关闭仍应可靠生效。
    ///
    /// @author kongweiguang
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(session_id) = self.session_id.clone() {
            cx.emit(TerminalSearchEvent::Close { session_id });
        }
        self.dismiss_local(window, cx);
        true
    }

    /// 读取真实 IME marked range，让 Esc 在组合事务中优先回到原生输入处理。
    ///
    /// @author kongweiguang
    fn query_is_composing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.query.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    /// Next/Prev 保存目标索引和提交请求，主题刷新替换尚在途的导航时仍保留用户的新目标。
    ///
    /// @author kongweiguang
    fn move_match(&mut self, backwards: bool, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let count = self.matches.len();
        let Some(next) = next_search_match_index(self.selected_match, count, backwards) else {
            return;
        };
        self.selected_match = Some(next);
        self.request_id = self.request_id.wrapping_add(1);
        self.busy = true;
        self.error = None;
        let request = TerminalSearchRequest {
            query: self.query.read(cx).value().to_string(),
            case_sensitive: self.case_sensitive,
            regex: self.regex,
            backwards,
            max_matches: MAX_SEARCH_MATCHES,
        };
        self.last_submitted_request = Some(request.clone());
        self.last_request_navigates = true;
        self.selected_match = Some(next);
        cx.emit(TerminalSearchEvent::SelectMatch {
            session_id,
            request_id: self.request_id,
            request,
            selected_match: next,
            safe_top_rows: self.safe_top_rows,
        });
        cx.notify();
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_search_toolbar.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/terminal_search_interaction.rs"]
mod interaction_tests;

impl Render for TerminalSearchToolbar {
    /// 搜索卡片只展示本地查询状态；自身拦截交互事件，外层定位容器保持透明以允许点击终端。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let selected = self.selected_match.map(|index| index + 1);
        let total = self.matches.len();
        let query_is_empty = self.query.read(cx).value().trim().is_empty();
        let can_interrupt_preview =
            self.busy && !self.last_request_navigates && self.selected_match.is_none();
        let can_navigate = self.session_id.is_some()
            && !query_is_empty
            && !self.suspended
            && (!self.busy || can_interrupt_preview);
        let count_label = if total == 0 {
            if self.error.is_none()
                && !query_is_empty
                && self.submitted_query_revision == Some(self.query_revision)
                && !self.busy
            {
                crate::locale::text("无匹配", "No matches").to_string()
            } else {
                String::new()
            }
        } else if let Some(selected) = selected {
            if self.truncated {
                format!("{selected}/{total}+")
            } else {
                format!("{selected}/{total}")
            }
        } else if self.truncated {
            format!("{total}+")
        } else {
            total.to_string()
        };
        let count_label = if self.busy && count_label.is_empty() {
            crate::locale::text("搜索中", "Searching").to_string()
        } else {
            count_label
        };
        let case_accessibility_label = if self.case_sensitive {
            crate::locale::text("区分大小写，已开启", "Match case, enabled")
        } else {
            crate::locale::text("区分大小写，已关闭", "Match case, disabled")
        };
        let regex_accessibility_label = if self.regex {
            crate::locale::text("正则表达式，已开启", "Regular expression, enabled")
        } else {
            crate::locale::text("正则表达式，已关闭", "Regular expression, disabled")
        };
        let width = if self.available_width < px(SEARCH_OVERLAY_WIDTH) {
            self.available_width
        } else {
            px(SEARCH_OVERLAY_WIDTH)
        };
        let compact = width < px(SEARCH_COMPACT_BREAKPOINT);
        let close_button = Button::new("terminal-search-close")
            .compact()
            .ghost()
            .flex_shrink_0()
            .icon(IconName::Close)
            .accessibility_label(crate::locale::text("关闭搜索", "Close search"))
            .tooltip(crate::locale::text("关闭搜索", "Close search"))
            .on_click(cx.listener(|toolbar, _: &ClickEvent, window, cx| {
                toolbar.close(window, cx);
            }));
        let case_button = Button::new("terminal-search-case")
            .compact()
            .ghost()
            .flex_shrink_0()
            .label("Aa")
            .selected(self.case_sensitive)
            .accessibility_label(case_accessibility_label)
            .tooltip(crate::locale::text("区分大小写", "Match case"))
            .on_click(cx.listener(|toolbar, _: &ClickEvent, _, cx| {
                toolbar.case_sensitive = !toolbar.case_sensitive;
                toolbar.invalidate_query(cx);
            }));
        let regex_button = Button::new("terminal-search-regex")
            .compact()
            .ghost()
            .flex_shrink_0()
            .label(".*")
            .selected(self.regex)
            .accessibility_label(regex_accessibility_label)
            .tooltip(crate::locale::text(
                "使用正则表达式",
                "Use regular expressions",
            ))
            .on_click(cx.listener(|toolbar, _: &ClickEvent, _, cx| {
                toolbar.regex = !toolbar.regex;
                toolbar.invalidate_query(cx);
            }));
        let count = div()
            .px_1()
            .flex_shrink_0()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(count_label);
        let previous_button = Button::new("terminal-search-previous")
            .compact()
            .ghost()
            .flex_shrink_0()
            .icon(IconName::ArrowUp)
            .accessibility_label(crate::locale::text("上一个匹配项", "Previous match"))
            .tooltip(crate::locale::text("上一个匹配项", "Previous match"))
            .disabled(!can_navigate)
            .on_click(cx.listener(|toolbar, _: &ClickEvent, window, cx| {
                if !toolbar.query_is_composing(window, cx) {
                    toolbar.navigate_or_submit(true, cx);
                }
            }));
        let next_button = Button::new("terminal-search-next")
            .compact()
            .ghost()
            .flex_shrink_0()
            .icon(IconName::ArrowDown)
            .accessibility_label(crate::locale::text("下一个匹配项", "Next match"))
            .tooltip(crate::locale::text("下一个匹配项", "Next match"))
            .disabled(!can_navigate)
            .on_click(cx.listener(|toolbar, _: &ClickEvent, window, cx| {
                if !toolbar.query_is_composing(window, cx) {
                    toolbar.navigate_or_submit(false, cx);
                }
            }));
        let mut card = div()
            .id("terminal-search-toolbar")
            .w(width)
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_1()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_sm()
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(|toolbar, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                let is_search_shortcut = event.keystroke.key.eq_ignore_ascii_case("f")
                    && modifiers.secondary()
                    && !modifiers.alt
                    && !modifiers.shift
                    && modifiers.number_of_modifiers() == 1;
                if is_search_shortcut && !toolbar.query_is_composing(window, cx) {
                    toolbar.focus(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                } else if event.keystroke.key.eq_ignore_ascii_case("enter")
                    && (modifiers.number_of_modifiers() == 0
                        || (modifiers.shift && modifiers.number_of_modifiers() == 1))
                    && !toolbar.suppress_composition_enter
                    && !toolbar.query_is_composing(window, cx)
                {
                    // InputState 已发出 PressEnter；阻止其 propagate 后再走字符输入而产生虚假的 Change。
                    window.prevent_default();
                    cx.stop_propagation();
                } else if event.keystroke.key.eq_ignore_ascii_case("escape")
                    && modifiers.number_of_modifiers() == 0
                {
                    if toolbar.suppress_composition_escape {
                        toolbar.suppress_composition_escape = false;
                    } else if toolbar.close(window, cx) {
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());

        let search_input = Input::new(&self.query).flex_1().min_w(px(80.));
        if compact {
            card = card.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(search_input)
                    .child(close_button),
            );
            card = card.child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(case_button)
                    .child(regex_button)
                    .child(count)
                    .child(previous_button)
                    .child(next_button),
            );
        } else {
            card = card.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(search_input)
                    .child(case_button)
                    .child(regex_button)
                    .child(count)
                    .child(previous_button)
                    .child(next_button)
                    .child(close_button),
            );
        }
        if let Some(error) = self.error.as_ref() {
            card = card.child(
                div()
                    .w_full()
                    .px_1()
                    .pb_1()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(error.clone()),
            );
        }
        card
    }
}
