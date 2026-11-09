//! 桥接端口只接受原实例并把取消和关闭传到所属窗口。
//!
//! @author kongweiguang

use super::*;
use std::task::{Context as TaskContext, Waker};

/// 只同步检查已知不会等待的结果，避免为端口测试创建额外运行时。
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future should already be ready"),
    }
}

/// 测试身份显式包含实例代次，以便重开后不会继承旧请求。
fn identity(plugin: &PluginId, generation: u64) -> PluginViewIdentity {
    PluginViewIdentity {
        plugin: plugin.clone(),
        view: "panel".to_owned(),
        instance: "one".to_owned(),
        generation,
    }
}

/// 一个桥接器的主窗口接收权只可被取走一次，避免窗口互相抢占。
#[test]
fn main_window_receiver_can_be_taken_only_once() {
    let bridge = PluginUiBridge::new().unwrap();
    let _receiver = bridge.take_receiver().unwrap();
    assert!(bridge.take_receiver().is_err());
}

/// 应用传来的插件身份与实例不一致时立即拒绝，不得进入其他插件的窗口队列。
#[test]
fn mismatched_plugin_and_instance_are_rejected_before_enqueue() {
    let bridge = PluginUiBridge::new().unwrap();
    let plugin = PluginId::parse("acme.notes").unwrap();
    let other = PluginId::parse("acme.other").unwrap();
    let instance = identity(&other, 1);
    let _guard = bridge.register_instance(instance.clone()).unwrap();
    let result = ready(bridge.invoke(
        &plugin,
        Some(&instance),
        "ui.notify",
        json!({"level": "info", "message": "hello"}),
        CancellationToken::new(),
    ));
    assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
}

/// 实例关闭会唤醒调用方并使旧请求失效，不能转发给新代次。
#[test]
fn closing_an_instance_cancels_its_pending_request() {
    let bridge = PluginUiBridge::new().unwrap();
    let plugin = PluginId::parse("acme.notes").unwrap();
    let instance = identity(&plugin, 1);
    let mut guard = bridge.register_instance(instance.clone()).unwrap();
    let mut receiver = bridge.take_receiver().unwrap();
    let mut call = bridge.invoke(
        &plugin,
        Some(&instance),
        "ui.notify",
        json!({"level": "info", "message": "hello"}),
        CancellationToken::new(),
    );
    let mut context = TaskContext::from_waker(Waker::noop());
    assert!(matches!(call.as_mut().poll(&mut context), Poll::Pending));
    let request = ready(receiver.next()).unwrap().unwrap();
    guard.close();
    assert_eq!(ready(call).unwrap_err().code, error_code::STALE);
    assert!(request.is_cancelled());
    assert!(!request.complete_with(|| Ok(json!({}))));

    let _next = bridge.register_instance(identity(&plugin, 2)).unwrap();
    let old = ready(bridge.invoke(
        &plugin,
        Some(&instance),
        "ui.notify",
        json!({"level": "info", "message": "late"}),
        CancellationToken::new(),
    ));
    assert_eq!(old.unwrap_err().code, error_code::STALE);
}

/// 调用方取消后，窗口持有的请求也会失效，避免迟到提交。
#[test]
fn token_cancellation_reaches_the_window_request() {
    let bridge = PluginUiBridge::new().unwrap();
    let plugin = PluginId::parse("acme.notes").unwrap();
    let mut receiver = bridge.take_receiver().unwrap();
    let token = CancellationToken::new();
    let mut call = bridge.invoke(
        &plugin,
        None,
        "ui.notify",
        json!({"level": "info", "message": "hello"}),
        token.clone(),
    );
    let mut context = TaskContext::from_waker(Waker::noop());
    assert!(matches!(call.as_mut().poll(&mut context), Poll::Pending));
    let request = ready(receiver.next()).unwrap().unwrap();
    token.cancel();
    assert_eq!(ready(call).unwrap_err().code, error_code::CANCELLED);
    assert!(request.is_cancelled());
    assert!(!request.complete_with(|| Ok(json!({}))));
}

/// 无视图调用不能把需要用户交互的能力隐式绑定到当前窗口。
#[test]
fn interactive_methods_require_an_explicit_instance() {
    let bridge = PluginUiBridge::new().unwrap();
    let plugin = PluginId::parse("acme.notes").unwrap();
    for method in ["ui.confirm", "ui.prompt", "ui.pickFile", "terminal.paste"] {
        let result =
            ready(bridge.invoke(&plugin, None, method, json!({}), CancellationToken::new()));
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }
}

/// 剪贴板文本必须保留空字符串和多条文本的顺序，空内容不能被当作无文本。
#[test]
fn clipboard_read_preserves_empty_and_multiple_text_entries() {
    assert_eq!(clipboard_text(None), None);
    assert_eq!(
        clipboard_text(Some(ClipboardItem::new_string(String::new()))),
        Some(String::new())
    );
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::String(gpui_kit::ClipboardString::new("a".to_owned())),
            ClipboardEntry::String(gpui_kit::ClipboardString::new("中".to_owned())),
        ],
    };
    assert_eq!(clipboard_text(Some(item)), Some("a中".to_owned()));
}

/// 文件列表不属于文本，避免把本地文件路径隐式暴露为剪贴板字符串。
#[test]
fn clipboard_read_does_not_project_file_paths_as_text() {
    let item = ClipboardItem {
        entries: vec![ClipboardEntry::ExternalPaths(gpui_kit::ExternalPaths(
            vec![std::path::PathBuf::from("private.txt")].into(),
        ))],
    };
    assert_eq!(clipboard_text(Some(item)), None);
}
