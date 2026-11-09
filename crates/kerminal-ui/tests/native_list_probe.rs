//! 分页探针的状态回归，可独立于桌面和 GPU 运行。
//!
//! @author kongweiguang

#[path = "../src/plugins/native_probe/state.rs"]
mod state;

use state::{PageCache, PageRequest, PageResponse, ProbeRow};

/// 构造来源明确的页面，覆盖跨页时的真实旧数据。
fn page(id: u64, range: std::ops::Range<usize>) -> PageResponse {
    let rows = range
        .clone()
        .map(|index| ProbeRow::new(index, format!("title {index}"), format!("path {index}")))
        .collect();
    PageResponse::new(PageRequest::new(id, range), rows)
}

/// 在请求完成前保留完整旧页，并在下一次请求使用最新视口。
#[test]
fn preserves_previous_rows_and_coalesces_the_latest_viewport() {
    let mut cache = PageCache::new(100_000, page(0, 0..4)).unwrap();
    let requested = cache.request_visible(5..9).unwrap().unwrap();
    assert!(cache.is_pending());
    assert_eq!(requested.range(), 5..9);
    assert!(cache.request_visible(20..24).unwrap().is_none());
    let stale = cache.project(20..24);
    assert_eq!(
        stale.iter().map(ProbeRow::index).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(stale[0].title(), "title 0");
    assert_eq!(stale[0].subtitle(), "path 0");
    cache.accept(page(requested.id(), 5..9)).unwrap();
    assert!(!cache.is_pending());
    assert_eq!(
        cache.request_visible(20..24).unwrap().unwrap().range(),
        20..24
    );
}

/// 迟到、缺行和错误身份都不能覆盖之前已展示的真实页面。
#[test]
fn rejects_stale_and_invalid_pages_without_losing_existing_rows() {
    let mut cache = PageCache::new(100_000, page(0, 0..3)).unwrap();
    let request = cache.request_visible(50..53).unwrap().unwrap();
    assert!(cache.accept(page(request.id() + 1, 50..53)).is_err());
    assert!(cache.accept(page(request.id(), 50..52)).is_err());
    let wrong_rows = vec![ProbeRow::new(99, "wrong".into(), "wrong".into()); 3];
    assert!(
        cache
            .accept(PageResponse::new(request.clone(), wrong_rows))
            .is_err()
    );
    assert_eq!(
        cache
            .project(50..53)
            .iter()
            .map(ProbeRow::index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    cache.accept(page(request.id(), 50..53)).unwrap();
    assert!(cache.covers(&(50..53)));
    assert_eq!(
        cache
            .project(50..53)
            .iter()
            .map(ProbeRow::index)
            .collect::<Vec<_>>(),
        vec![50, 51, 52]
    );
}

/// 范围必须落在十万行内，首屏也必须是已完成的真实数据。
#[test]
fn validates_boundaries_and_initial_data() {
    assert!(PageCache::new(100_000, page(0, 0..0)).is_err());
    let mut cache = PageCache::new(100_000, page(0, 0..2)).unwrap();
    assert!(cache.request_visible(100_000..100_001).is_err());
    assert!(cache.request_visible(10..10).is_err());
    assert_eq!(
        cache
            .request_visible(99_998..100_000)
            .unwrap()
            .unwrap()
            .range(),
        99_998..100_000
    );
}

/// 视口扩大时保留已有行，不用重复身份冒充尚未收到的新数据。
#[test]
fn expanding_viewport_does_not_duplicate_the_last_cached_row() {
    let mut cache = PageCache::new(100_000, page(0, 0..2)).unwrap();
    let request = cache.request_visible(20..24).unwrap().unwrap();
    assert_eq!(
        cache
            .project(20..24)
            .iter()
            .map(ProbeRow::index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    cache.accept(page(request.id(), 20..24)).unwrap();
    assert_eq!(
        cache
            .project(21..23)
            .iter()
            .map(ProbeRow::index)
            .collect::<Vec<_>>(),
        vec![21, 22]
    );
}
