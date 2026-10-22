//! 全窗口共用的双语文案选择入口，界面统一消费 GPUI Kit locale。
//!
//! @author kongweiguang

use gpui_kit::SharedString;
use kerminal_application::preferences::InterfaceLanguage;

/// 当前应用实际选中的简体中文或美式英语。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiLanguage {
    ZhCn,
    EnUs,
}

/// 应用设置和已解析的系统 locale 在一个入口合并后写入 Kit 全局 locale。
pub fn apply_interface_language(preference: InterfaceLanguage, system_locale: &str) -> UiLanguage {
    let language = resolve_language(preference, system_locale);
    let locale = match language {
        UiLanguage::ZhCn => "zh-CN",
        UiLanguage::EnUs => "en-US",
    };
    gpui_kit::component::set_locale(locale);
    language
}

/// 读取 Kit 当前 locale，使所有 feature 共用同一语言选择而不各自探测系统。
pub fn current_language() -> UiLanguage {
    let locale = gpui_kit::component::locale();
    if locale.to_ascii_lowercase().starts_with("zh") {
        UiLanguage::ZhCn
    } else {
        UiLanguage::EnUs
    }
}

/// 选择一组双语文本并返回 GPUI 可直接渲染的字符串。
pub fn text(zh_cn: impl Into<SharedString>, en_us: impl Into<SharedString>) -> SharedString {
    match current_language() {
        UiLanguage::ZhCn => zh_cn.into(),
        UiLanguage::EnUs => en_us.into(),
    }
}

/// 映射设置和系统语言到当前仅维护的两种产品语言。
fn resolve_language(preference: InterfaceLanguage, system_locale: &str) -> UiLanguage {
    match preference {
        InterfaceLanguage::ZhCn => UiLanguage::ZhCn,
        InterfaceLanguage::EnUs => UiLanguage::EnUs,
        InterfaceLanguage::System => {
            if system_locale.trim().to_ascii_lowercase().starts_with("zh") {
                UiLanguage::ZhCn
            } else {
                UiLanguage::EnUs
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/locale.rs"]
mod tests;
