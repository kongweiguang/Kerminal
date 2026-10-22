//! 编辑器查找与替换共用的有界匹配器和索引。
//!
//! @author kongweiguang

use std::ops::Range;

use kerminal_domain::files::editor::MAX_EDITOR_BYTES;
use regex::{Captures, Regex, RegexBuilder};
use tokio_util::sync::CancellationToken;

/// 用户可见的搜索选项，纯值类型便于输入变化时复用同一规则。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct EditorSearchOptions {
    pub(super) match_case: bool,
    pub(super) whole_word: bool,
    pub(super) regex: bool,
}

/// 搜索栏的正交开关，避免每个控件重复维护选项更新语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorSearchOption {
    MatchCase,
    WholeWord,
    Regex,
}

/// 搜索词编译后的不可变 matcher，查找、当前替换和全量替换共用它。
#[derive(Clone, Debug)]
pub(super) struct EditorSearchMatcher {
    regex: Regex,
    expand_captures: bool,
}

/// 搜索结果只保留少量绘制范围和稀疏定位点，匹配数本身仍保持精确。
#[derive(Clone, Debug, Default)]
pub(super) struct EditorSearchIndex {
    total_matches: usize,
    highlights: Vec<Range<usize>>,
    checkpoints: Vec<SearchCheckpoint>,
}

/// 稀疏检查点记录匹配序号及其后继扫描位置，供远处导航按需重扫。
#[derive(Clone, Debug)]
struct SearchCheckpoint {
    match_index: usize,
    range: Range<usize>,
    next_offset: Option<usize>,
    last_match_end: Option<usize>,
}

/// 后台扫描只会协作取消，捕获不变量错误不会伪装成零匹配。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SearchWorkError {
    Cancelled,
    InvalidCapture,
    InvalidPattern(String),
    ReplacementLimitExceeded,
    OutputAllocationFailed,
}

/// 一个接受的 regex 迭代结果及其后继状态，用于复现 regex::captures_iter 的空匹配规则。
struct SearchStep {
    range: Range<usize>,
    next_offset: Option<usize>,
    last_match_end: Option<usize>,
}

/// 正则替换模板中的捕获组引用，语法与 regex crate 的展开器保持一致。
enum CaptureReference<'a> {
    Index(usize),
    Name(&'a str),
}

const SEARCH_HIGHLIGHT_LIMIT: usize = 256;
const SEARCH_CHECKPOINT_INTERVAL: usize = 1024;
const SEARCH_CHECKPOINT_LIMIT: usize = 16_384;

impl EditorSearchMatcher {
    /// 在编辑器边界一次性编译选项，确保导航和替换不各自解释搜索语法。
    pub(super) fn compile(
        query: &str,
        options: EditorSearchOptions,
    ) -> Result<Option<Self>, SearchWorkError> {
        if query.is_empty() {
            return Ok(None);
        }

        let expression = if options.regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let expression = if options.whole_word {
            format!(r"\b(?:{expression})\b")
        } else {
            expression
        };
        let regex = RegexBuilder::new(&expression)
            .case_insensitive(!options.match_case)
            .unicode(true)
            .build()
            .map_err(|error| SearchWorkError::InvalidPattern(error.to_string()))?;

        Ok(Some(Self {
            regex,
            expand_captures: options.regex,
        }))
    }

    /// 在后台按 matcher 完整计数，同时将装饰与索引空间限制在文档大小的稀疏上界内。
    ///
    /// @author kongweiguang
    pub(super) fn scan(
        &self,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<EditorSearchIndex, SearchWorkError> {
        let mut index = EditorSearchIndex {
            total_matches: 0,
            highlights: Vec::with_capacity(SEARCH_HIGHLIGHT_LIMIT),
            checkpoints: Vec::new(),
        };
        let mut next_offset = Some(0);
        let mut last_match_end = None;

        while let Some(offset) = next_offset {
            if cancellation.is_cancelled() {
                return Err(SearchWorkError::Cancelled);
            }
            let Some(step) = self.next_match(text, offset, last_match_end, cancellation)? else {
                break;
            };

            if index.highlights.len() < SEARCH_HIGHLIGHT_LIMIT {
                index.highlights.push(step.range.clone());
            }
            if index
                .total_matches
                .is_multiple_of(SEARCH_CHECKPOINT_INTERVAL)
                && index.checkpoints.len() < SEARCH_CHECKPOINT_LIMIT
            {
                index.checkpoints.push(SearchCheckpoint {
                    match_index: index.total_matches,
                    range: step.range,
                    next_offset: step.next_offset,
                    last_match_end: step.last_match_end,
                });
            }
            index.total_matches += 1;
            next_offset = step.next_offset;
            last_match_end = step.last_match_end;
        }

        Ok(index)
    }

    /// 从最近稀疏检查点解析一个匹配，保留正则锚点语义且不持有全量范围表。
    pub(super) fn resolve_range_at(
        &self,
        text: &str,
        index: &EditorSearchIndex,
        target_match: usize,
        cancellation: &CancellationToken,
    ) -> Result<Option<Range<usize>>, SearchWorkError> {
        if target_match >= index.total_matches {
            return Ok(None);
        }
        let Some(checkpoint) = index
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.match_index <= target_match)
        else {
            return Ok(None);
        };
        if checkpoint.match_index == target_match {
            return Ok(Some(checkpoint.range.clone()));
        }

        let Some(mut offset) = checkpoint.next_offset else {
            return Ok(None);
        };
        let mut last_match_end = checkpoint.last_match_end;
        let mut match_index = checkpoint.match_index + 1;
        while match_index <= target_match {
            if cancellation.is_cancelled() {
                return Err(SearchWorkError::Cancelled);
            }
            let Some(step) = self.next_match(text, offset, last_match_end, cancellation)? else {
                return Ok(None);
            };
            if match_index == target_match {
                return Ok(Some(step.range));
            }
            let Some(next_offset) = step.next_offset else {
                return Ok(None);
            };
            offset = next_offset;
            last_match_end = step.last_match_end;
            match_index += 1;
        }

        Ok(None)
    }

    /// 仅在后台核对目标范围及展开正则捕获，调用方仍需复核文档代次后再提交编辑。
    pub(super) fn replace_current(
        &self,
        text: &str,
        range: &Range<usize>,
        replacement: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, SearchWorkError> {
        self.replace_current_with_limit(text, range, replacement, cancellation, MAX_EDITOR_BYTES)
    }

    /// 以同一算法注入文档预算，使当前替换的最终正文大小可用小型样本验证。
    fn replace_current_with_limit(
        &self,
        text: &str,
        range: &Range<usize>,
        replacement: &str,
        cancellation: &CancellationToken,
        max_bytes: usize,
    ) -> Result<Option<String>, SearchWorkError> {
        if cancellation.is_cancelled() {
            return Err(SearchWorkError::Cancelled);
        }
        if text.len() > max_bytes {
            return Err(SearchWorkError::ReplacementLimitExceeded);
        }
        let Some(captures) = self.regex.captures_at(text, range.start) else {
            return Ok(None);
        };
        let Some(matched) = captures.get(0) else {
            return Err(SearchWorkError::InvalidCapture);
        };
        if matched.start() != range.start || matched.end() != range.end {
            return Ok(None);
        }

        let preserved_bytes = text
            .len()
            .checked_sub(range.end.saturating_sub(range.start))
            .ok_or(SearchWorkError::InvalidCapture)?;
        let expansion_limit = max_bytes.saturating_sub(preserved_bytes);
        let mut output = String::new();
        self.expand_replacement_bounded(
            &captures,
            replacement,
            &mut output,
            expansion_limit,
            cancellation,
        )?;
        Ok(Some(output))
    }

    /// 在后台流式生成完整正文与真实替换数，不物化范围列表或静默截断替换。
    pub(super) fn replace_all(
        &self,
        text: &str,
        replacement: &str,
        cancellation: &CancellationToken,
    ) -> Result<(String, usize), SearchWorkError> {
        self.replace_all_with_limit(text, replacement, cancellation, MAX_EDITOR_BYTES)
    }

    /// 将输出预算作为生产算法参数，便于小预算回归验证而不构造巨型输入。
    fn replace_all_with_limit(
        &self,
        text: &str,
        replacement: &str,
        cancellation: &CancellationToken,
        max_bytes: usize,
    ) -> Result<(String, usize), SearchWorkError> {
        if text.len() > max_bytes {
            return Err(SearchWorkError::ReplacementLimitExceeded);
        }
        let mut output = String::new();
        let mut cursor = 0;
        let mut count = 0;

        for captures in self.regex.captures_iter(text) {
            if count % 128 == 0 && cancellation.is_cancelled() {
                return Err(SearchWorkError::Cancelled);
            }
            let Some(matched) = captures.get(0) else {
                return Err(SearchWorkError::InvalidCapture);
            };
            if matched.start() < cursor {
                continue;
            }
            append_bounded(&mut output, &text[cursor..matched.start()], max_bytes)?;
            self.expand_replacement_bounded(
                &captures,
                replacement,
                &mut output,
                max_bytes,
                cancellation,
            )?;
            cursor = matched.end();
            count += 1;
        }

        if cancellation.is_cancelled() {
            return Err(SearchWorkError::Cancelled);
        }
        append_bounded(&mut output, &text[cursor..], max_bytes)?;
        Ok((output, count))
    }

    /// 以原文完整上下文重试匹配，跳过 regex 迭代器会压制的相邻空匹配。
    fn next_match(
        &self,
        text: &str,
        mut offset: usize,
        last_match_end: Option<usize>,
        cancellation: &CancellationToken,
    ) -> Result<Option<SearchStep>, SearchWorkError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(SearchWorkError::Cancelled);
            }
            let Some(captures) = self.regex.captures_at(text, offset) else {
                return Ok(None);
            };
            let matched = captures.get(0).ok_or(SearchWorkError::InvalidCapture)?;
            let range = matched.start()..matched.end();

            if range.is_empty() && Some(range.end) == last_match_end {
                let Some(next_offset) = next_search_offset(text, &range) else {
                    return Ok(None);
                };
                offset = next_offset;
                continue;
            }

            return Ok(Some(SearchStep {
                next_offset: next_search_offset(text, &range),
                last_match_end: Some(range.end),
                range,
            }));
        }
    }

    /// 先精确计算捕获展开字节数，再为受限目标预留空间，避免先无界分配后检查。
    fn expand_replacement_bounded(
        &self,
        captures: &Captures<'_>,
        replacement: &str,
        output: &mut String,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<(), SearchWorkError> {
        if cancellation.is_cancelled() {
            return Err(SearchWorkError::Cancelled);
        }
        let remaining = limit
            .checked_sub(output.len())
            .ok_or(SearchWorkError::ReplacementLimitExceeded)?;
        let expanded_len =
            self.expanded_replacement_len(captures, replacement, remaining, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(SearchWorkError::Cancelled);
        }
        output
            .try_reserve(expanded_len)
            .map_err(|_| SearchWorkError::OutputAllocationFailed)?;
        let original_len = output.len();
        if self.expand_captures {
            captures.expand(replacement, output);
        } else {
            output.push_str(replacement);
        }
        debug_assert_eq!(output.len() - original_len, expanded_len);
        Ok(())
    }

    /// 与 regex 的 replacement 模板语法一致地计长，但不创建展开结果缓冲。
    fn expanded_replacement_len(
        &self,
        captures: &Captures<'_>,
        replacement: &str,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<usize, SearchWorkError> {
        if !self.expand_captures {
            return bounded_length(0, replacement.len(), limit);
        }

        let mut length = 0;
        let mut cursor = 0;
        let bytes = replacement.as_bytes();
        while cursor < bytes.len() {
            if cursor % 4096 == 0 && cancellation.is_cancelled() {
                return Err(SearchWorkError::Cancelled);
            }
            let Some(relative_dollar) = bytes[cursor..].iter().position(|byte| *byte == b'$')
            else {
                return bounded_length(length, bytes.len() - cursor, limit);
            };
            let dollar = cursor + relative_dollar;
            length = bounded_length(length, dollar - cursor, limit)?;
            cursor = dollar;

            if bytes.get(cursor + 1) == Some(&b'$') {
                length = bounded_length(length, 1, limit)?;
                cursor += 2;
                continue;
            }

            let Some((reference, end)) = parse_capture_reference(replacement, cursor) else {
                length = bounded_length(length, 1, limit)?;
                cursor += 1;
                continue;
            };
            let value = match reference {
                CaptureReference::Index(index) => captures.get(index),
                CaptureReference::Name(name) => captures.name(name),
            };
            if let Some(value) = value {
                length = bounded_length(length, value.as_str().len(), limit)?;
            }
            cursor = end;
        }
        Ok(length)
    }
}

impl EditorSearchIndex {
    /// 返回文档中真实总数，避免把有界装饰数量误报为匹配总数。
    pub(super) fn total_matches(&self) -> usize {
        self.total_matches
    }

    /// 返回有界首屏范围；当前活动项由工作区另外加入装饰集合。
    pub(super) fn highlights(&self) -> &[Range<usize>] {
        &self.highlights
    }
}

/// 模仿 regex find_iter 对零宽命中的 UTF-8 前进规则，避免锚点查询卡在同一位置。
fn next_search_offset(text: &str, range: &Range<usize>) -> Option<usize> {
    if range.start != range.end {
        return Some(range.end);
    }
    if range.end == text.len() {
        return None;
    }
    text.get(range.end..)?
        .chars()
        .next()
        .map(|character| range.end + character.len_utf8())
}

/// 按 regex crate 的 `$name`、`${name}`、数字组和转义美元符号语法解析引用。
fn parse_capture_reference(
    replacement: &str,
    dollar: usize,
) -> Option<(CaptureReference<'_>, usize)> {
    let bytes = replacement.as_bytes();
    if bytes.get(dollar) != Some(&b'$') || dollar + 1 >= bytes.len() {
        return None;
    }
    let start = dollar + 1;
    let (reference_start, reference_end, end) = if bytes.get(start) == Some(&b'{') {
        let reference_start = start + 1;
        let relative_end = bytes[reference_start..]
            .iter()
            .position(|byte| *byte == b'}')?;
        let reference_end = reference_start + relative_end;
        (reference_start, reference_end, reference_end + 1)
    } else {
        let mut reference_end = start;
        while bytes
            .get(reference_end)
            .is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        {
            reference_end += 1;
        }
        if reference_end == start {
            return None;
        }
        (start, reference_end, reference_end)
    };
    let value = &replacement[reference_start..reference_end];
    let reference = value
        .parse::<usize>()
        .map(CaptureReference::Index)
        .unwrap_or(CaptureReference::Name(value));
    Some((reference, end))
}

/// 在任何 push_str 前检查长度溢出、文档上限和可恢复分配失败。
fn append_bounded(output: &mut String, value: &str, limit: usize) -> Result<(), SearchWorkError> {
    let new_length = bounded_length(output.len(), value.len(), limit)?;
    output
        .try_reserve(new_length - output.len())
        .map_err(|_| SearchWorkError::OutputAllocationFailed)?;
    output.push_str(value);
    Ok(())
}

/// 将字节加法集中校验，防止超限正文在扩容或追加前进入目标缓冲。
fn bounded_length(current: usize, addition: usize, limit: usize) -> Result<usize, SearchWorkError> {
    let length = current
        .checked_add(addition)
        .ok_or(SearchWorkError::ReplacementLimitExceeded)?;
    if length > limit {
        return Err(SearchWorkError::ReplacementLimitExceeded);
    }
    Ok(length)
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_search_matcher.rs"]
mod tests;
