//! Agent landing 的文字呈现契约，防止窄筛选段丢失零计数。
//!
//! @author kongweiguang

use super::history_filter_display_text;

/// 当前目标和全部筛选都在标签后显示计数，零值也必须可见。
///
/// @author kongweiguang
#[test]
fn history_filter_text_keeps_zero_count_visible() {
    assert_eq!(history_filter_display_text("当前目标", 0), "当前目标 0");
    assert_eq!(history_filter_display_text("全部", 0), "全部 0");
}
