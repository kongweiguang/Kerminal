//! P0.4 探针的分页状态，不依赖 GPUI 或进程传输。
//!
//! @author kongweiguang

use std::ops::Range;

/// 一行的身份由数据源提供；滚动位置不能代替该身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRow {
    index: usize,
    title: String,
    subtitle: String,
}

impl ProbeRow {
    /// 由 RPC 适配器建立完整行，避免 UI 在缺少数据时伪造行内容。
    pub fn new(index: usize, title: String, subtitle: String) -> Self {
        Self {
            index,
            title,
            subtitle,
        }
    }

    /// 对照响应范围校验稳定身份，防止错误页面污染现有内容。
    pub fn index(&self) -> usize {
        self.index
    }

    /// 返回数据源的标题，渲染时不重新计算或触发数据读取。
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 副标题与标题来自同一响应，保留完整上一屏数据。
    pub fn subtitle(&self) -> &str {
        &self.subtitle
    }
}

/// RPC 请求固定实际可视范围，id 区分迟到或重复响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    id: u64,
    range: Range<usize>,
}

impl PageRequest {
    /// 启动预热和逐帧可视请求共用同一协议表示。
    pub fn new(id: u64, range: Range<usize>) -> Self {
        Self { id, range }
    }

    /// 请求身份用于校验跨进程回执。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 返回半开区间，与 GPUI 可视 range 的边界一致。
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }
}

/// 回执保留原始请求，接收方仍需逐项核对行身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageResponse {
    request: PageRequest,
    rows: Vec<ProbeRow>,
}

impl PageResponse {
    /// RPC 传输只封装返回值，页面归属校验留给状态所有者。
    pub fn new(request: PageRequest, rows: Vec<ProbeRow>) -> Self {
        Self { request, rows }
    }
}

/// 最多保留一个在途请求与一页真实数据，避免滚动时清空旧内容。
#[derive(Debug)]
pub struct PageCache {
    total: usize,
    page: PageResponse,
    pending: Option<PageRequest>,
    next_id: u64,
}

impl PageCache {
    /// 首屏必须先由相同 RPC 拉取成功，避免构造空白占位行。
    pub fn new(total: usize, initial: PageResponse) -> Result<Self, &'static str> {
        Self::validate_page(total, &initial)?;
        Ok(Self {
            total,
            next_id: initial.request.id.saturating_add(1),
            page: initial,
            pending: None,
        })
    }

    /// 一次仅发一个可视区间，滚动中间范围自然合并到下一次请求。
    pub fn request_visible(
        &mut self,
        visible: Range<usize>,
    ) -> Result<Option<PageRequest>, &'static str> {
        if visible.start >= visible.end || visible.end > self.total {
            return Err("invalid visible range");
        }
        if self.pending.is_some()
            || (self.page.request.range.start <= visible.start
                && self.page.request.range.end >= visible.end)
        {
            return Ok(None);
        }
        let request = PageRequest::new(self.next_id, visible);
        self.next_id = self.next_id.checked_add(1).ok_or("request id exhausted")?;
        self.pending = Some(request.clone());
        Ok(Some(request))
    }

    /// 只接受对应在途请求且逐行身份正确的结果，失败时保持旧页。
    pub fn accept(&mut self, response: PageResponse) -> Result<(), &'static str> {
        if self.pending.as_ref() != Some(&response.request) {
            return Err("stale or mismatched response");
        }
        Self::validate_page(self.total, &response)?;
        self.pending = None;
        self.page = response;
        Ok(())
    }

    /// 未命中时沿用上一页的真实行；视口扩大时不重复旧行来填充新位置。
    pub fn project(&self, visible: Range<usize>) -> Vec<ProbeRow> {
        let page_range = &self.page.request.range;
        if page_range.start <= visible.start && page_range.end >= visible.end {
            visible
                .map(|index| self.page.rows[index - page_range.start].clone())
                .collect()
        } else {
            self.page.rows.iter().take(visible.len()).cloned().collect()
        }
    }

    /// 反馈当前范围是否使用了缓存，供结果区分刷新期间的旧页。
    pub fn covers(&self, visible: &Range<usize>) -> bool {
        self.page.request.range.start <= visible.start && self.page.request.range.end >= visible.end
    }

    /// 结束测量前等待最后一个 RPC，避免将未完成的页面计作成功。
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 按半开区间和行身份校验边界，任何异常都不能替换上一页。
    fn validate_page(total: usize, page: &PageResponse) -> Result<(), &'static str> {
        let range = &page.request.range;
        if range.start >= range.end
            || range.end > total
            || page.rows.len() != range.end - range.start
        {
            return Err("invalid page range or row count");
        }
        if page
            .rows
            .iter()
            .enumerate()
            .any(|(offset, row)| row.index != range.start + offset)
        {
            return Err("page row identity mismatch");
        }
        Ok(())
    }
}
