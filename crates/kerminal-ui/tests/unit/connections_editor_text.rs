//! 表单分隔符、空项及 Windows 字面路径的无损编码回归。
//!
//! @author kongweiguang

use super::{ListSeparator, format_list, parse_list};

/// 组合边界值验证编码双向无损；尤其零项、单空项和尾部空项不能合并。
#[test]
fn list_text_roundtrips_special_values_without_shell_or_json_escaping() {
    let cases = [
        "",
        "plain",
        "中文",
        "ops,prod",
        "a\nb",
        "\r\n",
        "\r",
        "\"quoted\"",
        " leading ",
        r"C:\temp\new",
        r#""C:\Program Files\shell""#,
        "a=first\nNEXT=second",
    ];
    for separator in [ListSeparator::Lines, ListSeparator::Commas] {
        assert_eq!(parse_list("", separator).unwrap(), Vec::<String>::new());
        for first in cases {
            for second in cases {
                for values in [
                    vec![first.to_owned()],
                    vec![first.to_owned(), second.to_owned()],
                ] {
                    let encoded = format_list(&values, separator);
                    assert_eq!(
                        parse_list(&encoded, separator).unwrap(),
                        values,
                        "{encoded:?}"
                    );
                }
            }
        }
    }
}

/// 常见粘贴的 CRLF 只在分隔处规范化；引号内的换行、反斜杠和双引号属于值。
#[test]
fn line_input_keeps_legacy_blank_arguments_and_accepts_quoted_multiline_items() {
    assert_eq!(
        parse_list("--flag\r\n\r\nvalue\r\n", ListSeparator::Lines).unwrap(),
        ["--flag", "", "value", ""]
    );
    assert_eq!(
        parse_list("\"\"\n\"first\nsecond\"\nC:\\temp", ListSeparator::Lines).unwrap(),
        ["", "first\nsecond", "C:\\temp"]
    );
}

/// 标签忽略分隔处的空白和空段，显式引用的空标签及边界空白仍可回填既有合法值。
#[test]
fn comma_input_distinguishes_syntax_whitespace_from_quoted_content() {
    assert_eq!(
        parse_list(
            " ops, , \"prod,cn\", \"\"\"quoted\"\"\", \"\", \" space \"",
            ListSeparator::Commas
        )
        .unwrap(),
        ["ops", "prod,cn", "\"quoted\"", "", " space "]
    );
}

/// 错误引用必须阻止提交，不能静默把一项拆成多个参数或多个标签。
#[test]
fn incomplete_quotes_and_text_after_closing_quote_are_rejected() {
    for separator in [ListSeparator::Lines, ListSeparator::Commas] {
        for invalid in ["\"unclosed", "\"closed\"garbage", "\"one\" \"two\""] {
            assert!(parse_list(invalid, separator).is_err(), "{invalid:?}");
        }
    }
}
