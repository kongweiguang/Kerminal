//! 编辑器 matcher 的 UTF-8、选项、稀疏索引和替换语义回归。
//!
//! @author kongweiguang

use regex::Regex;
use tokio_util::sync::CancellationToken;

use super::{EditorSearchMatcher, EditorSearchOptions, SearchWorkError};

/// 测试中复用生产编译入口，确保查询选项只有一套解释规则。
fn matcher(query: &str, options: EditorSearchOptions) -> EditorSearchMatcher {
    EditorSearchMatcher::compile(query, options)
        .expect("matcher expression should be valid")
        .expect("nonempty query should have a matcher")
}

/// 用生产扫描入口生成结果，避免测试自行拼造稀疏索引。
fn indexed(matcher: &EditorSearchMatcher, text: &str) -> super::EditorSearchIndex {
    matcher
        .scan(text, &CancellationToken::new())
        .expect("uncancelled scan should complete")
}

/// 通过生产稀疏定位器取结果，验证未缓存的远处命中仍可精确导航。
fn range_at(
    matcher: &EditorSearchMatcher,
    text: &str,
    index: &super::EditorSearchIndex,
    match_index: usize,
) -> std::ops::Range<usize> {
    matcher
        .resolve_range_at(text, index, match_index, &CancellationToken::new())
        .expect("uncancelled navigation should complete")
        .expect("indexed match should be resolvable")
}

#[test]
/// 同一个 literal matcher 同时决定大小写与完整单词命中范围。
fn literal_match_obeys_case_and_word_boundaries() {
    let matcher = matcher(
        "cat",
        EditorSearchOptions {
            match_case: false,
            whole_word: true,
            regex: false,
        },
    );
    let text = "Cat scatter cat";
    let index = indexed(&matcher, text);

    assert_eq!(index.total_matches(), 2);
    assert_eq!(range_at(&matcher, text, &index, 0), 0..3);
    assert_eq!(range_at(&matcher, text, &index, 1), 12..15);
}

#[test]
/// Match Case 开启后，大小写差异不再命中。
fn match_case_option_is_respected() {
    let matcher = matcher(
        "cat",
        EditorSearchOptions {
            match_case: true,
            ..EditorSearchOptions::default()
        },
    );
    let text = "Cat cat";
    let index = indexed(&matcher, text);

    assert_eq!(index.total_matches(), 1);
    assert_eq!(range_at(&matcher, text, &index, 0), 4..7);
}

#[test]
/// Unicode 字母连续组成同一单词时，完整单词模式不会误切出内部字符。
fn whole_word_treats_adjacent_cjk_letters_as_one_word() {
    let matcher = matcher(
        "猫",
        EditorSearchOptions {
            whole_word: true,
            ..EditorSearchOptions::default()
        },
    );
    let text = "猫猫 猫";
    let index = indexed(&matcher, text);

    assert_eq!(index.total_matches(), 1);
    assert_eq!(range_at(&matcher, text, &index, 0), 7..10);
}

#[test]
/// 搜索范围按 UTF-8 字节定位，补充字符与中文不会按字符下标错位。
fn literal_ranges_are_utf8_byte_ranges() {
    let matcher = matcher("猫", EditorSearchOptions::default());
    let text = "😀 猫猫";
    let index = indexed(&matcher, text);

    assert_eq!(range_at(&matcher, text, &index, 0), 5..8);
    assert_eq!(range_at(&matcher, text, &index, 1), 8..11);
}

#[test]
/// 正则捕获组展开、当前项替换和全量替换共用同一组匹配语义。
fn regex_replacement_expands_capture_groups() {
    let matcher = matcher(
        r"(?m)^猫=(\d+)$",
        EditorSearchOptions {
            match_case: true,
            regex: true,
            ..EditorSearchOptions::default()
        },
    );
    let text = "猫=7\n猫=12";
    let index = indexed(&matcher, text);
    let first = range_at(&matcher, text, &index, 0);

    assert_eq!(index.total_matches(), 2);
    assert_eq!(
        matcher.replace_current(text, &first, "$1 元", &CancellationToken::new()),
        Ok(Some("7 元".into()))
    );
    assert_eq!(
        matcher.replace_all(text, "$1 元", &CancellationToken::new()),
        Ok(("7 元\n12 元".into(), 2))
    );
}

#[test]
/// 预先计长器与 regex 官方展开器对转义、数字、命名和无效捕获引用保持一致。
fn bounded_expansion_length_matches_regex_replacement_syntax() {
    let text = "a";
    let pattern = r"(?P<part>a)";
    let matcher = matcher(
        pattern,
        EditorSearchOptions {
            match_case: true,
            regex: true,
            ..EditorSearchOptions::default()
        },
    );
    let oracle = Regex::new(pattern).expect("named capture should compile");
    let replacements = [
        "$0",
        "$1",
        "${1}a",
        "$$",
        "${missing}",
        "$missing",
        "${part}",
        "$part!",
        "${part",
        "$",
    ];

    for replacement in replacements {
        let (actual, count) = matcher
            .replace_all(text, replacement, &CancellationToken::new())
            .expect("small replacement should fit");
        assert_eq!(count, 1, "replacement {replacement:?}");
        assert_eq!(
            actual,
            oracle.replace_all(text, replacement).to_string(),
            "replacement {replacement:?}"
        );
    }
}

#[test]
/// 普通文本替换把美元符号当字面字符，避免误解释为正则捕获组。
fn literal_replacement_does_not_expand_dollar_references() {
    let matcher = matcher("cat", EditorSearchOptions::default());

    assert_eq!(
        matcher.replace_all("cat CAT", "$1", &CancellationToken::new()),
        Ok(("$1 $1".into(), 2))
    );
}

#[test]
/// 零宽正则按 Unicode 字符推进，保证索引可导航且不会卡在相同字节偏移。
fn zero_width_matches_advance_on_utf8_boundaries() {
    let matcher = matcher(
        r"\b",
        EditorSearchOptions {
            regex: true,
            ..EditorSearchOptions::default()
        },
    );
    // Rust regex 不支持 look-around；\b 在此文本的实际边界为 0、1、2、5，2 到 5 跨过完整的猫字符。
    let text = "a 猫 😀";
    let index = indexed(&matcher, text);

    assert_eq!(index.total_matches(), 4);
    assert_eq!(range_at(&matcher, text, &index, 0), 0..0);
    assert_eq!(range_at(&matcher, text, &index, 1), 1..1);
    assert_eq!(range_at(&matcher, text, &index, 2), 2..2);
    assert_eq!(range_at(&matcher, text, &index, 3), 5..5);
}

#[test]
/// 扫描、稀疏导航与 Replace All 对紧邻空匹配遵循 regex::captures_iter 的统一语义。
fn zero_width_iteration_matches_regex_iterator_for_empty_and_nonempty_alternation() {
    let cases = [
        (r"a*", "a"),
        (r"a*", "ab"),
        (r"a?", "a"),
        (r"a|(?:)", "ab"),
        (r"(?:)|a", "a"),
        (r"(?:)|猫", "猫猫"),
    ];
    let options = EditorSearchOptions {
        match_case: true,
        regex: true,
        ..EditorSearchOptions::default()
    };

    for (pattern, text) in cases {
        let matcher = matcher(pattern, options);
        let oracle = Regex::new(pattern).expect("test pattern should compile");
        let expected: Vec<_> = oracle
            .captures_iter(text)
            .map(|captures| {
                let matched = captures.get(0).expect("regex match has group zero");
                matched.start()..matched.end()
            })
            .collect();
        let index = indexed(&matcher, text);
        let actual: Vec<_> = (0..index.total_matches())
            .map(|match_index| range_at(&matcher, text, &index, match_index))
            .collect();
        let (replaced, replace_count) = matcher
            .replace_all(text, "X", &CancellationToken::new())
            .expect("small replacement should fit");

        assert_eq!(actual, expected, "pattern {pattern:?} on {text:?}");
        assert_eq!(replace_count, expected.len(), "pattern {pattern:?}");
        assert_eq!(replaced, oracle.replace_all(text, "X").to_string());
    }
}

#[test]
/// 稀疏定位从检查点恢复上一匹配终点，跨越空匹配抑制边界仍与官方迭代一致。
fn sparse_navigation_preserves_empty_match_iterator_state() {
    let pattern = r"a*";
    let text = "ab".repeat(1_200);
    let matcher = matcher(
        pattern,
        EditorSearchOptions {
            match_case: true,
            regex: true,
            ..EditorSearchOptions::default()
        },
    );
    let oracle = Regex::new(pattern).expect("test pattern should compile");
    let expected: Vec<_> = oracle
        .captures_iter(&text)
        .map(|captures| {
            let matched = captures.get(0).expect("regex match has group zero");
            matched.start()..matched.end()
        })
        .collect();
    let index = indexed(&matcher, &text);
    let actual: Vec<_> = (0..index.total_matches())
        .map(|match_index| range_at(&matcher, &text, &index, match_index))
        .collect();

    assert_eq!(index.total_matches(), expected.len());
    assert_eq!(actual, expected);
    assert_eq!(range_at(&matcher, &text, &index, 1024), expected[1024]);
}

#[test]
/// 捕获展开先按上限计算长度，超限当前项和全量替换都在产生正文前拒绝。
fn replacement_expansion_is_bounded_before_output_is_built() {
    let matcher = matcher(
        r"(?P<part>a)",
        EditorSearchOptions {
            match_case: true,
            regex: true,
            ..EditorSearchOptions::default()
        },
    );
    let oracle = Regex::new(r"(?P<part>a)").expect("named capture should compile");
    let captures = oracle.captures("a").expect("test input should match");
    let cancellation = CancellationToken::new();
    let replacement = "$part".repeat(32);

    assert_eq!(
        matcher.expanded_replacement_len(&captures, &replacement, 8, &cancellation),
        Err(SearchWorkError::ReplacementLimitExceeded)
    );
    let mut output = "keep".to_owned();
    assert_eq!(
        matcher.expand_replacement_bounded(
            &captures,
            "$part$part$part$part",
            &mut output,
            7,
            &cancellation,
        ),
        Err(SearchWorkError::ReplacementLimitExceeded)
    );
    assert_eq!(output, "keep");

    assert_eq!(
        matcher.replace_all_with_limit("a", "$part$part$part", &cancellation, 2),
        Err(SearchWorkError::ReplacementLimitExceeded)
    );
    assert_eq!(
        matcher.replace_all_with_limit("a", "$part$part$part", &cancellation, 3),
        Ok(("aaa".to_owned(), 1))
    );
    assert_eq!(
        matcher.replace_current_with_limit("a!", &(0..1), "$part$part", &cancellation, 2),
        Err(SearchWorkError::ReplacementLimitExceeded)
    );
}

#[test]
/// 大量命中只保存有界装饰和稀疏检查点，同时仍能精确定位末尾命中。
fn large_result_set_keeps_exact_count_and_bounded_index() {
    let matcher = matcher("a", EditorSearchOptions::default());
    let text = "a".repeat(300_000);
    let index = indexed(&matcher, &text);

    assert_eq!(index.total_matches(), 300_000);
    assert_eq!(index.highlights().len(), 256);
    assert!(index.checkpoints.len() <= 16_384);
    assert_eq!(range_at(&matcher, &text, &index, 1023), 1023..1024);
    assert_eq!(range_at(&matcher, &text, &index, 1024), 1024..1025);
    assert_eq!(range_at(&matcher, &text, &index, 299_999), 299_999..300_000);
}

#[test]
/// 预先取消的扫描明确返回取消，不伪装成零匹配结果。
fn cancelled_scan_does_not_return_an_empty_result() {
    let matcher = matcher("a", EditorSearchOptions::default());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(matches!(
        matcher.scan("aaa", &cancellation),
        Err(SearchWorkError::Cancelled)
    ));
}

#[test]
/// 无效正则和空查询在任何替换工作前被拒绝或解释为空匹配器。
fn invalid_regex_and_empty_query_are_distinct() {
    assert!(
        EditorSearchMatcher::compile(
            "(",
            EditorSearchOptions {
                regex: true,
                ..EditorSearchOptions::default()
            }
        )
        .is_err()
    );
    assert!(
        EditorSearchMatcher::compile("", EditorSearchOptions::default())
            .expect("empty query is not a syntax error")
            .is_none()
    );
}
