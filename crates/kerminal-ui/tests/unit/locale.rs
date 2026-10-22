//! 共享界面语言策略测试。
//!
//! @author kongweiguang

use kerminal_application::preferences::InterfaceLanguage;

use super::{UiLanguage, resolve_language};

/// 显式设置优先于系统 locale，避免用户保存后界面仍使用启动时语言。
#[test]
fn explicit_language_overrides_system_locale() {
    assert_eq!(
        resolve_language(InterfaceLanguage::EnUs, "zh-CN"),
        UiLanguage::EnUs
    );
    assert_eq!(
        resolve_language(InterfaceLanguage::ZhCn, "en-US"),
        UiLanguage::ZhCn
    );
}

/// System 模式将平台提供的 BCP-47 locale 收敛到当前产品支持的两种语言。
#[test]
fn system_language_uses_platform_locale_prefix() {
    assert_eq!(
        resolve_language(InterfaceLanguage::System, "zh-Hans-CN"),
        UiLanguage::ZhCn
    );
    assert_eq!(
        resolve_language(InterfaceLanguage::System, "en-GB"),
        UiLanguage::EnUs
    );
}
