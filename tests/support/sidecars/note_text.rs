// 笔记夹具共用的 Unicode 码点截断，单独成文件便于验收测试直接编译验证。
// @author kongweiguang

const MAX_NOTE_TEXT: usize = 32_000;

/// 按 Unicode 标量截断而不是按 UTF-16 码元，避免把代理对切断后生成孤立代理；
/// 夹具与宿主协议上限一致，超长内容只影响本实例草稿。
fn note_text(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut result = String::new();
    for character in value.chars().take(MAX_NOTE_TEXT) {
        result.push(character);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 边界输入必须保留完整补充平面字符，超长输入只截断到 32000 个标量。
    #[test]
    fn truncates_at_unicode_scalar_boundary() {
        let exact = format!("{}🙂", "😀".repeat(MAX_NOTE_TEXT - 1));
        let over = format!("{}猫🙂", "😀".repeat(MAX_NOTE_TEXT - 1));
        assert_eq!(note_text(None), "");
        assert_eq!(note_text(Some(&exact)), exact);
        let truncated = note_text(Some(&over));
        assert_eq!(truncated.chars().count(), MAX_NOTE_TEXT);
        assert_eq!(truncated.chars().last(), Some('猫'));
        assert!(truncated.chars().all(|character| character.len_utf8() <= 4));
    }
}