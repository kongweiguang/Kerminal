//! 插件命令纯状态的定向回归；这些测试不启动窗口或调用插件进程。
//!
//! @author kongweiguang

use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Instant};

use super::command_logic::{
    EnqueueResult, InFlight, PluginCommandContext, PluginCommandId, PluginCommandInvocation,
    TabEventQueue, localized_title, sequences_overlap,
};

/// 测试身份由插件与本地命令共同组成，避免偶然依赖显示标题。
fn command(plugin: &str, name: &str) -> PluginCommandId {
    PluginCommandId {
        plugin: plugin.to_owned(),
        command: name.to_owned(),
    }
}

/// 观察资源释放，验证取消确实触及原请求且迟到回执不会释放新请求。
struct TrackedDrop(Rc<Cell<usize>>);

impl Drop for TrackedDrop {
    /// 使用计数模拟取消令牌的释放，避免测试依赖运行时或桌面环境。
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// 非空本地化优先，缺失或空白时回退英文，普通标题不按翻译键解释。
#[test]
fn command_titles_resolve_locale_and_fallback() {
    let translations = BTreeMap::from([
        (
            "zh-CN".to_owned(),
            BTreeMap::from([
                ("open".to_owned(), "打开".to_owned()),
                ("save".to_owned(), " ".to_owned()),
            ]),
        ),
        (
            "en".to_owned(),
            BTreeMap::from([
                ("open".to_owned(), "Open".to_owned()),
                ("save".to_owned(), "Save".to_owned()),
            ]),
        ),
    ]);
    assert_eq!(
        localized_title(&translations, "zh-CN", "%open%").as_deref(),
        Some("打开")
    );
    assert_eq!(
        localized_title(&translations, "zh-CN", "%save%").as_deref(),
        Some("Save")
    );
    assert_eq!(
        localized_title(&translations, "fr", "%open%").as_deref(),
        Some("Open")
    );
    assert_eq!(
        localized_title(&translations, "zh-CN", "Literal title").as_deref(),
        Some("Literal title")
    );
    assert_eq!(localized_title(&translations, "zh-CN", "%missing%"), None);
    assert_eq!(localized_title(&translations, "zh-CN", "   "), None);
}

/// 相同键位和任一侧完整组合前缀均冲突，普通文本前缀不应误判。
#[test]
fn shortcut_conflicts_require_whole_stroke_boundaries() {
    assert!(sequences_overlap("ctrl-a", "ctrl-a"));
    assert!(sequences_overlap("ctrl-a", "ctrl-a ctrl-b"));
    assert!(sequences_overlap("ctrl-a ctrl-b", "ctrl-a"));
    assert!(!sequences_overlap("ctrl-a", "ctrl-alt-a"));
    assert!(!sequences_overlap("ctrl-a ctrl-b", "ctrl-a ctrl-c"));
}

/// 插件身份和命令身份同时参与并发门禁，不能因同名而阻断其它插件。
#[test]
fn duplicate_command_is_rejected_without_blocking_other_identities() {
    let mut active = InFlight::default();
    let a = command("example.one", "open");
    let b = command("example.two", "open");
    let c = command("example.one", "save");
    assert!(active.begin(a.clone(), ()).is_some());
    assert!(active.begin(a, ()).is_none());
    assert!(active.begin(b, ()).is_some());
    assert!(active.begin(c, ()).is_some());
}

/// 取消和重试之间的旧响应不得解除新调用的门禁或释放新取消令牌。
#[test]
fn old_completion_cannot_finish_a_retried_command() {
    let count = Rc::new(Cell::new(0));
    let mut active = InFlight::default();
    let id = command("example.one", "open");
    let first = active
        .begin(id.clone(), TrackedDrop(count.clone()))
        .unwrap();
    active.clear();
    assert_eq!(count.get(), 1);
    let second = active
        .begin(id.clone(), TrackedDrop(count.clone()))
        .unwrap();
    assert_ne!(first, second);
    assert!(!active.finish(&id, first));
    assert_eq!(count.get(), 1);
    assert!(active.finish(&id, second));
    assert_eq!(count.get(), 2);
    assert!(!active.finish(&id, second));
}

/// 已捕获的面板目标不会随之后的活动标签或主机变化而被替换。
#[test]
fn invocation_keeps_the_original_context() {
    let mut live = PluginCommandContext {
        tab: Some("tab-a".to_owned()),
        host: Some("host-a".to_owned()),
    };
    let invocation = PluginCommandInvocation {
        id: command("example.one", "open"),
        context: live.clone(),
    };
    live.tab = Some("tab-b".to_owned());
    live.host = None;
    assert_eq!(invocation.context.tab.as_deref(), Some("tab-a"));
    assert_eq!(invocation.context.host.as_deref(), Some("host-a"));
}

/// 标签变化按提交顺序发送，重复值去重，满队列不提前改变最后接收的目标。
#[test]
fn tab_events_are_ordered_deduplicated_and_bounded() {
    let mut queue = TabEventQueue::default();
    let deadline = Instant::now();
    assert_eq!(
        queue.enqueue(Some("a".into()), deadline, 1),
        EnqueueResult::Queued
    );
    let (first, tab, _) = queue.begin(()).unwrap().unwrap();
    assert_eq!(tab.as_deref(), Some("a"));
    assert_eq!(
        queue.enqueue(Some("b".into()), deadline, 1),
        EnqueueResult::Queued
    );
    assert_eq!(
        queue.enqueue(Some("b".into()), deadline, 1),
        EnqueueResult::Unchanged
    );
    assert_eq!(
        queue.enqueue(Some("c".into()), deadline, 1),
        EnqueueResult::Full
    );
    assert!(queue.begin(()).unwrap().is_none());
    assert!(queue.finish(first));
    let (second, tab, _) = queue.begin(()).unwrap().unwrap();
    assert_eq!(tab.as_deref(), Some("b"));
    assert_eq!(
        queue.enqueue(Some("c".into()), deadline, 1),
        EnqueueResult::Queued
    );
    assert!(queue.finish(second));
    let (third, tab, _) = queue.begin(()).unwrap().unwrap();
    assert_eq!(tab.as_deref(), Some("c"));
    assert!(queue.finish(third));
}

/// 关闭后释放活动通知的资源，旧事件回执不能结束新一轮队列的通知。
#[test]
fn cancelled_tab_event_does_not_finish_its_successor() {
    let count = Rc::new(Cell::new(0));
    let mut queue = TabEventQueue::default();
    let deadline = Instant::now();
    assert_eq!(queue.enqueue(None, deadline, 2), EnqueueResult::Queued);
    let (old, tab, _) = queue.begin(TrackedDrop(count.clone())).unwrap().unwrap();
    assert_eq!(tab, None);
    queue.clear();
    assert_eq!(count.get(), 1);
    assert_eq!(queue.enqueue(None, deadline, 2), EnqueueResult::Queued);
    let (new, _, _) = queue.begin(TrackedDrop(count.clone())).unwrap().unwrap();
    assert_ne!(old, new);
    assert!(!queue.finish(old));
    assert_eq!(count.get(), 1);
    assert!(queue.finish(new));
    assert_eq!(count.get(), 2);
}
