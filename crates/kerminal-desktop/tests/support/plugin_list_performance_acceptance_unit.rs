//! 外部插件十万行列表验收的纯逻辑回归：目标规划、范围变化和类型失败关闭。
//! @author kongweiguang

use serde_json::json;

use super::*;

/// 构造最小可解析证据；测试只覆盖解析和判定，不创建renderer或窗口。
fn ready_evidence(range: (usize, usize), rows: Vec<(usize, String)>) -> ListEvidence {
    ListEvidence {
        instance: "instance-a".to_owned(),
        generation: 7,
        window_visible: true,
        total: 100_000,
        range: VisibleRange {
            start: range.0,
            end: range.1,
        },
        rows,
        fetch_requests: 10,
        fetch_successes: 5,
        fetch_errors: 0,
    }
}

/// 连续目标必须领先上一目标至少一整页，不能回退或原地重复。
#[test]
fn continuous_target_advances_by_one_visible_page() {
    let baseline = VisibleRange {
        start: 100,
        end: 130,
    };
    assert_eq!(
        continuous_target(baseline, Some(90), 100_000).expect("forward target"),
        160
    );
}

/// 空范围、未超过上一目标或接近数据末端时都必须显式失败，不能用固定小步距冒充位移。
#[test]
fn continuous_target_fails_closed_without_progress() {
    let baseline = VisibleRange {
        start: 100,
        end: 130,
    };
    assert!(continuous_target(baseline, Some(160), 100_000).is_err());
    assert!(continuous_target(VisibleRange { start: 7, end: 7 }, None, 100_000).is_err());
    assert!(
        continuous_target(
            VisibleRange {
                start: 99_990,
                end: 99_999
            },
            None,
            100_000
        )
        .is_err()
    );
}

/// 请求前的目标检查只拒绝已可见和越界值；向后往返到范围之外仍应允许。
#[test]
fn request_target_rejects_in_place_and_out_of_range() {
    let baseline = VisibleRange {
        start: 100,
        end: 130,
    };
    assert!(validate_request_target(baseline, 130, 100_000).is_ok());
    assert!(validate_request_target(baseline, 50, 100_000).is_ok());
    assert!(validate_request_target(baseline, 129, 100_000).is_err());
    assert!(validate_request_target(baseline, 100_000, 100_000).is_err());
}

/// 15个往返目标必须覆盖首中末，且相邻目标不同，避免把同索引原地滚动计入往返。
#[test]
fn roundtrip_sequence_visits_anchors_without_adjacent_repeats() {
    assert_eq!(ROUNDTRIP_TARGETS.len(), 15);
    assert!(ROUNDTRIP_TARGETS.contains(&0));
    assert!(ROUNDTRIP_TARGETS.contains(&49_999));
    assert!(ROUNDTRIP_TARGETS.contains(&99_999));
    assert!(ROUNDTRIP_TARGETS.windows(2).all(|pair| pair[0] != pair[1]));
}

/// 快照解析必须区分未就绪和类型错误，字符串代次、字符串状态与错误行索引都不能通过。
#[test]
fn evidence_parser_reports_waiting_missing_and_wrong_types() {
    let ready = json!({
        "list": {
            "instance": "instance-a",
            "generation": 1,
            "visible": true,
            "status": "Ready",
            "error": null,
            "collection": {
                "total": 100000,
                "visible_start": 10,
                "visible_end": 40,
                "visible_rows": [{"index": 10, "key": "record-10"}],
                "error": null,
                "fetch_requests": 2,
                "fetch_successes": 1,
                "fetch_errors": 0
            }
        }
    });
    let ListSnapshot::Ready(evidence) = inspect_list(&ready).expect("ready evidence") else {
        panic!("就绪列表必须解析为证据");
    };
    assert_eq!(evidence.range, VisibleRange { start: 10, end: 40 });
    assert_eq!(evidence.rows, vec![(10, "record-10".to_owned())]);

    assert!(matches!(
        inspect_list(&json!({"list": null})).expect("null list"),
        ListSnapshot::Waiting
    ));
    assert!(matches!(
        inspect_list(&json!({"list": {"status": "Opening"}})).expect("opening list"),
        ListSnapshot::Waiting
    ));

    let mut wrong_generation = ready.clone();
    wrong_generation["list"]["generation"] = json!("1");
    assert!(inspect_list(&wrong_generation).is_err());

    let mut wrong_status = ready.clone();
    wrong_status["list"]["status"] = json!(1);
    assert!(inspect_list(&wrong_status).is_err());

    let mut wrong_visible = ready.clone();
    wrong_visible["list"]["visible"] = json!("true");
    assert!(inspect_list(&wrong_visible).is_err());

    let mut wrong_row = ready.clone();
    wrong_row["list"]["collection"]["visible_rows"][0]["index"] = json!("10");
    assert!(inspect_list(&wrong_row).is_err());

    let mut missing_rows = ready;
    let _ = missing_rows["list"]["collection"]
        .as_object_mut()
        .expect("collection object")
        .remove("visible_rows");
    assert!(inspect_list(&missing_rows).is_err());
}

/// 插件摘要必须覆盖所有包文件并按路径字节序、长度前缀拼接；该向量与 PowerShell 门禁共用，
/// 两边任一实现漂移都会得到不同摘要，而不是各自生成“看起来合法”的独立结果。
#[test]
fn plugin_content_digest_matches_shared_known_vector() {
    let mut files = std::collections::BTreeMap::new();
    files.insert("views/v.json".to_owned(), b"{}".to_vec());
    files.insert("a.js".to_owned(), b"abc".to_vec());
    assert_eq!(
        plugin_content_digest(&files),
        "b58ce47fdb58840d6acd856e0607cadbb6e5cb26d4542cb5a18de43c5c2cdd5f"
    );

    // 空文件也必须参与摘要；只摘要非空内容会让新增的空入口脚本完全不可见。
    files.insert("main.js".to_owned(), Vec::new());
    assert_eq!(
        plugin_content_digest(&files),
        "3a5353342fad7f85b81165915f1fd68216a1a3d833fda0f6cf08564a40064fde"
    );
}

/// 同路径的内容变化必须改变摘要，且内容边界不能靠拼接“借用”另一文件的长度。
#[test]
fn plugin_content_digest_changes_with_content_and_path_boundaries() {
    let mut original = std::collections::BTreeMap::new();
    original.insert("main.js".to_owned(), b"render()".to_vec());
    let mut changed = original.clone();
    changed.insert("main.js".to_owned(), b"render(0)".to_vec());
    assert_ne!(
        plugin_content_digest(&original),
        plugin_content_digest(&changed)
    );

    let mut split = std::collections::BTreeMap::new();
    split.insert("a".to_owned(), b"bc".to_vec());
    let mut joined = std::collections::BTreeMap::new();
    joined.insert("ab".to_owned(), b"c".to_vec());
    assert_ne!(
        plugin_content_digest(&split),
        plugin_content_digest(&joined)
    );
}

/// 证据链只接受与上一观测完全相等的请求前范围；任何未记录的中间滚动都必须失败关闭。
#[test]
fn contiguous_baseline_rejects_unrecorded_scroll() {
    let previous = VisibleRange {
        start: 100,
        end: 140,
    };
    assert!(require_contiguous_baseline(previous, previous).is_ok());
    let jumped = VisibleRange {
        start: 200,
        end: 240,
    };
    let error = require_contiguous_baseline(jumped, previous).expect_err("跳步必须失败");
    assert!(error.to_string().contains("不连续"));
}

/// renderer的真实失败状态带诊断文本，解析必须按前缀识别并原样保留原因。
#[test]
fn failed_status_preserves_renderer_diagnostic() {
    let snapshot = json!({"list": {"status": "Failed(\"rpc timeout\")"}});
    let error = inspect_list(&snapshot).expect_err("Failed状态必须报错");
    assert!(error.to_string().contains("Failed(\"rpc timeout\")"));
    assert!(error.to_string().contains("rpc timeout"));
}

/// 预暖观测必须同时记录请求前范围和真实可见范围，报告才能独立重放预暖而非口头声明。
#[test]
fn warmup_observation_records_both_ranges() {
    let snapshot = json!({"list": {"status": "Ready"}});
    let value = warmup_observation(
        1,
        20_000,
        VisibleRange {
            start: 50_000,
            end: 50_040,
        },
        VisibleRange {
            start: 19_980,
            end: 20_020,
        },
        &snapshot,
    );
    assert_eq!(value["phase"], "warmup");
    assert_eq!(value["step"], 1);
    assert_eq!(value["index"], 20_000);
    assert_eq!(value["baselineVisibleStart"], 50_000);
    assert_eq!(value["baselineVisibleEnd"], 50_040);
    assert_eq!(value["visibleStart"], 19_980);
    assert_eq!(value["visibleEnd"], 20_020);
    assert_eq!(value["list"]["status"], "Ready");
}

/// 帧样本下限与 PowerShell 门禁保持同一数值，前后两半各需一半，避免脚本只检查单帧。
#[test]
fn frame_sample_floor_is_one_hundred_with_each_half_checked() {
    assert_eq!(MIN_FRAME_SAMPLES, 100);
    assert_eq!(MIN_FRAME_SAMPLES_PER_HALF, 50);
}

/// 视口高度与字体大小共同给出观测跨度下限，且不低于八行绝对下限。
#[test]
fn minimum_observed_span_uses_half_of_estimated_rows_and_never_below_eight() {
    let metadata = json!({"viewport": [1024.0, 720.0], "fontSize": 14.5});
    // 720 / (14.5 * 2.5) = 19.86 -> 19 行，取一半为 9。
    assert_eq!(minimum_observed_span(&metadata).expect("span floor"), 9);
    let small = json!({"viewport": [800.0, 300.0], "fontSize": 20.0});
    // 300 / 50 = 6 行，一半为 3，仍取八行绝对下限。
    assert_eq!(minimum_observed_span(&small).expect("span floor"), 8);
    assert!(minimum_observed_span(&json!({"viewport": [800.0, 600.0], "fontSize": 0.0})).is_err());
}

/// 完成条件必须同时满足新范围、目标包含、真实业务键和同一实例代次；任何旧帧都只算未完成。
#[test]
fn observation_requires_changed_range_real_key_and_same_identity() {
    let baseline = VisibleRange {
        start: 100,
        end: 130,
    };
    let evidence = ready_evidence((130, 160), vec![(130, "record-130".to_owned())]);
    let expected = ExpectedInstance {
        instance: "instance-a".to_owned(),
        generation: 7,
    };
    assert!(same_instance(&expected, &evidence));
    assert!(target_observed(&evidence, 130, baseline));

    let mut stale = evidence.clone();
    stale.range = baseline;
    assert!(!target_observed(&stale, 130, baseline));

    let mut missing_row = evidence.clone();
    missing_row.rows.clear();
    assert!(!target_observed(&missing_row, 130, baseline));

    let mut wrong_key = evidence.clone();
    wrong_key.rows = vec![(130, "record-999".to_owned())];
    assert!(!target_observed(&wrong_key, 130, baseline));

    let mut target_outside = evidence.clone();
    target_outside.range = VisibleRange {
        start: 131,
        end: 161,
    };
    assert!(!target_observed(&target_outside, 130, baseline));

    let mut other_instance = evidence.clone();
    other_instance.instance = "instance-b".to_owned();
    assert!(!same_instance(&expected, &other_instance));

    let mut other_generation = evidence;
    other_generation.generation = 8;
    assert!(!same_instance(&expected, &other_generation));
}

/// 首屏 Ready 但分页未到时集合仍是 total=0、0..40 的占位形状；它必须继续等待，
/// 不能被 end>total 误判为硬失败，也不能在已有业务行时放行。
#[test]
fn ready_with_zero_total_and_empty_rows_is_waiting() {
    let pending = json!({
        "list": {
            "status": "Ready",
            "error": null,
            "instance": "instance-a",
            "generation": 1,
            "visible": true,
            "collection": {
                "total": 0,
                "visible_start": 0,
                "visible_end": 40,
                "visible_rows": [],
                "error": null,
                "fetch_requests": 1,
                "fetch_successes": 0,
                "fetch_errors": 0
            }
        }
    });
    assert!(matches!(
        inspect_list(&pending).expect("零总数占位必须继续等待"),
        ListSnapshot::Waiting
    ));

    let mut contradictory = pending.clone();
    contradictory["list"]["collection"]["visible_rows"] = json!([{"index": 0, "key": "record-0"}]);
    let error = inspect_list(&contradictory).expect_err("零总数却已有行必须失败");
    assert!(error.to_string().contains("零"));
}

/// 分页错误必须从集合快照进入失败关闭并保留原文，不能因为 renderer 仍是 Ready 就继续采样。
#[test]
fn collection_error_fails_and_preserves_diagnostic() {
    let snapshot = json!({
        "list": {
            "status": "Ready",
            "error": null,
            "instance": "instance-a",
            "generation": 1,
            "visible": true,
            "collection": {
                "total": 100000,
                "visible_start": 0,
                "visible_end": 40,
                "visible_rows": [{"index": 0, "key": "record-0"}],
                "error": "rpc timeout",
                "fetch_requests": 2,
                "fetch_successes": 1,
                "fetch_errors": 1
            }
        }
    });
    let error = inspect_list(&snapshot).expect_err("集合错误必须失败");
    assert!(error.to_string().contains("rpc timeout"));

    let mut missing = snapshot;
    missing["list"]["collection"]
        .as_object_mut()
        .expect("collection object")
        .remove("error");
    assert!(inspect_list(&missing).is_err());

    let mut wrong_type = json!({
        "list": {
            "status": "Ready",
            "error": null,
            "instance": "instance-a",
            "generation": 1,
            "visible": true,
            "collection": {
                "total": 100000,
                "visible_start": 0,
                "visible_end": 40,
                "visible_rows": [{"index": 0, "key": "record-0"}],
                "error": true,
                "fetch_requests": 2,
                "fetch_successes": 1,
                "fetch_errors": 0
            }
        }
    });
    assert!(inspect_list(&wrong_type).is_err());
    wrong_type["list"]["collection"]["error"] = json!("");
    assert!(inspect_list(&wrong_type).is_err());
}

/// 成功页会清除可见错误文本；累计错误计数必须继续失败，不能把旧错误洗成无错误样本。
#[test]
fn cleared_error_still_fails_via_cumulative_counter() {
    let snapshot = json!({
        "list": {
            "status": "Ready",
            "error": null,
            "instance": "instance-a",
            "generation": 1,
            "visible": true,
            "collection": {
                "total": 100000,
                "visible_start": 0,
                "visible_end": 40,
                "visible_rows": [{"index": 0, "key": "record-0"}],
                "error": null,
                "fetch_requests": 3,
                "fetch_successes": 2,
                "fetch_errors": 1
            }
        }
    });
    let error = inspect_list(&snapshot).expect_err("累计错误必须失败");
    assert!(error.to_string().contains("1次分页错误"));
}

/// 连续阶段不接受回退整页、回到第0行、落后目标超过两行或跨度明显漂移的观测；
/// 目标前两行的渲染超采样仍属于同一页前进。
#[test]
fn continuous_forward_rejects_backward_and_unstable_ranges() {
    let baseline = VisibleRange {
        start: 100,
        end: 140,
    };
    let target = 180;
    assert!(
        validate_continuous_forward(
            baseline,
            target,
            VisibleRange {
                start: 180,
                end: 220
            },
            8
        )
        .is_ok()
    );
    assert!(
        validate_continuous_forward(
            baseline,
            target,
            VisibleRange {
                start: 178,
                end: 218
            },
            8
        )
        .is_ok()
    );
    // 回到更早范围或第0行都不是前向。
    assert!(
        validate_continuous_forward(
            baseline,
            target,
            VisibleRange {
                start: 100,
                end: 140
            },
            8
        )
        .is_err()
    );
    assert!(
        validate_continuous_forward(baseline, target, VisibleRange { start: 0, end: 40 }, 8)
            .is_err()
    );
    // 目标位于新视口起点之后一页内的正常渲染仍属于前向。
    assert!(
        validate_continuous_forward(
            baseline,
            target,
            VisibleRange {
                start: 177,
                end: 217
            },
            8
        )
        .is_ok()
    );
    // 跨度差超过两行说明采样的是另一视口尺寸。
    assert!(
        validate_continuous_forward(
            baseline,
            target,
            VisibleRange {
                start: 180,
                end: 200
            },
            8
        )
        .is_err()
    );
    // 两行残片低于绝对下限，即使目标恰在其中也不能通过。
    assert!(
        validate_observed_span(
            VisibleRange {
                start: 180,
                end: 182
            },
            8
        )
        .is_err()
    );
}

/// 来源快照必须绑定本轮标签和正式外部插件；宿主 fixture、其它提供方、活动标签变化和
/// 绑定错误都必须立即失败，列表尚未出现时才保持等待。
#[test]
fn source_binding_rejects_host_fixture_and_wrong_tab() {
    let tab = TabId::parse("tab-list").expect("tab id");
    let valid = json!({
        "content": {
            "tabId": "tab-list",
            "activeTabId": "tab-list",
            "activeBinding": true,
            "id": LIST_CONTENT_ID,
            "provider": LIST_PLUGIN_ID,
            "plugin": LIST_PLUGIN_ID,
            "view": LIST_VIEW,
            "runtime": LIST_RUNTIME,
            "externalPlugin": true
        },
        "list": {
            "plugin": LIST_PLUGIN_ID,
            "view": LIST_VIEW,
            "runtime": LIST_RUNTIME,
            "external": true
        }
    });
    let source = inspect_source(&valid, &tab)
        .expect("正式外部插件来源")
        .expect("列表已就绪");
    assert_eq!(source["contentId"].as_str(), Some(LIST_CONTENT_ID));
    assert_eq!(source["externalPlugin"].as_bool(), Some(true));

    let pending = json!({
        "content": {
            "tabId": "tab-list",
            "activeTabId": "tab-list",
            "activeBinding": true,
            "id": LIST_CONTENT_ID,
            "provider": LIST_PLUGIN_ID,
            "plugin": null,
            "view": null,
            "runtime": null,
            "externalPlugin": false
        },
        "list": null
    });
    assert!(
        inspect_source(&pending, &tab)
            .expect("待分页来源")
            .is_none()
    );

    let mut host = valid.clone();
    host["content"]["id"] = json!(HOST_LIST_CONTENT_ID);
    host["content"]["provider"] = json!(HOST_LIST_PLUGIN_ID);
    assert!(inspect_source(&host, &tab).is_err());

    let mut other_tab = valid.clone();
    other_tab["content"]["activeTabId"] = json!("tab-other");
    assert!(inspect_source(&other_tab, &tab).is_err());

    let mut other_plugin = valid.clone();
    other_plugin["list"]["plugin"] = json!("acme.notes");
    other_plugin["content"]["provider"] = json!("acme.notes");
    assert!(inspect_source(&other_plugin, &tab).is_err());

    let mut host_runtime = valid.clone();
    host_runtime["list"]["runtime"] = json!("none");
    assert!(inspect_source(&host_runtime, &tab).is_err());

    let mut unbound = valid;
    unbound["bindingError"] = json!("active tab changed");
    assert!(inspect_source(&unbound, &tab).is_err());
}
