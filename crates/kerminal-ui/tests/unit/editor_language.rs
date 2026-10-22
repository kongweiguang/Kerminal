//! 按文件名选择内置 tree-sitter 语言的边界测试。
//!
//! @author kongweiguang

use super::language_for_file_name;

#[test]
/// 常用本地和远程文件名映射到 GPUI Kit 自带 grammar 的规范名称。
fn known_extensions_resolve_case_insensitively() {
    assert_eq!(language_for_file_name("src/main.RS"), Some("rust"));
    assert_eq!(language_for_file_name("settings.JSON"), Some("json"));
    assert_eq!(language_for_file_name("C:\\repo\\app.tsx"), Some("tsx"));
    assert_eq!(language_for_file_name("/srv/config.yml"), Some("yaml"));
    assert_eq!(language_for_file_name("theme.scss"), Some("css"));
    assert_eq!(language_for_file_name("Makefile"), Some("make"));
}

#[test]
/// 没有已包含 grammar 的后缀时保持纯文本编辑，不猜测语法类型。
fn unsupported_names_remain_plain_text() {
    assert_eq!(language_for_file_name("README"), None);
    assert_eq!(language_for_file_name("component.vue"), None);
}

#[test]
/// 常见 shell dotfile 不依赖扩展名也能选择 Bash grammar。
fn shell_dotfiles_resolve_without_an_extension() {
    assert_eq!(language_for_file_name(".zshrc"), Some("bash"));
}
