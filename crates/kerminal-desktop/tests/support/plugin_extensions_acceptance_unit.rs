//! 扩展页验收的纯状态回归；不创建窗口、进程或真实插件目录。
//! @author kongweiguang

use std::io;

use kerminal_domain::plugins::PluginStoreState;
use serde_json::json;

use super::{
    ACCEPTANCE_MODE, AcceptanceFailures, EXPECTED_PENDING, FRAME_VALIDATION,
    combine_acceptance_results, frame_evidence, validate_pending_install,
};

/// 用完整待安装条目构造权威状态，使排序、重复和额外项都由真实领域类型判定。
fn state_with_pending(ids: &[&str]) -> PluginStoreState {
    let pending = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            json!({
                "op": "install",
                "id": id,
                "to": "1.0.0",
                "staged": format!("staged-{index}"),
                "source": "local",
                "installed_at": "2026-01-01T00:00:00Z",
            })
        })
        .collect::<Vec<_>>();
    serde_json::from_value(json!({
        "schema": 1,
        "revision": 0,
        "plugins": {},
        "pending": pending,
        "failures": {},
    }))
    .expect("合成状态必须匹配当前持久化格式")
}

/// 只有两个预期安装且各出现一次时才返回排序后的权威身份，供报告与启动器复核。
#[test]
fn pending_install_requires_exact_identity_set() {
    let ids = validate_pending_install(
        &state_with_pending(&["acme.file-search", "acme.market-two"]),
        &EXPECTED_PENDING,
    )
    .expect("完整 pending 集合必须通过");
    assert_eq!(
        ids,
        vec!["acme.market-two".to_owned(), "acme.file-search".to_owned()]
    );

    for (case, pending) in [
        ("missing", vec!["acme.market-two"]),
        (
            "extra",
            vec!["acme.market-two", "acme.file-search", "acme.extra"],
        ),
        ("duplicate", vec!["acme.market-two", "acme.market-two"]),
        ("empty", vec![]),
    ] {
        assert!(
            validate_pending_install(&state_with_pending(&pending), &EXPECTED_PENDING).is_err(),
            "{case} pending 集合不能通过"
        );
    }
}

/// 界面、组合根关闭和权威状态必须分别决议；同时失败时每条真实原因都要留在同一条错误中。
#[test]
fn simultaneous_acceptance_failures_are_all_preserved() {
    let result = combine_acceptance_results::<u8>(
        Ok(7),
        Err(Box::new(io::Error::other("synthetic-shutdown"))),
        Ok(vec!["acme.market-two".to_owned()]),
    );
    assert!(result.is_err(), "关闭失败不能被界面成功掩盖");

    let error = combine_acceptance_results::<u8>(
        Err(Box::new(io::Error::other("synthetic-ui"))),
        Err(Box::new(io::Error::other("synthetic-shutdown"))),
        Err(Box::new(io::Error::other("synthetic-pending"))),
    )
    .expect_err("三个阶段同时失败不能返回成功");
    let text = error.to_string();
    for expected in ["synthetic-ui", "synthetic-shutdown", "synthetic-pending"] {
        assert!(text.contains(expected), "多错误文本缺少 {expected}: {text}");
    }
    let failures = error
        .downcast_ref::<AcceptanceFailures>()
        .expect("必须保留类型化的多错误");
    assert_eq!(failures.failures.len(), 3);
    assert!(
        error
            .source()
            .is_some_and(|source| source.to_string().contains("synthetic-ui")),
        "主错误链必须保留最先发生的界面失败"
    );
}

/// 任一单独失败都必须返回错误，成功路径的两项结果则原样返回。
#[test]
fn acceptance_results_resolve_each_stage_independently() {
    assert_eq!(
        combine_acceptance_results::<u8>(Ok(3), Ok(()), Ok(vec!["id".to_owned()]))
            .expect("全部成功必须返回值"),
        (3, vec!["id".to_owned()])
    );
    assert!(
        combine_acceptance_results::<u8>(
            Err(Box::new(io::Error::other("ui-only"))),
            Ok(()),
            Ok(vec![])
        )
        .is_err()
    );
    assert!(
        combine_acceptance_results::<u8>(
            Ok(1),
            Err(Box::new(io::Error::other("close-only"))),
            Ok(vec![])
        )
        .is_err()
    );
    assert!(
        combine_acceptance_results::<u8>(
            Ok(1),
            Ok(()),
            Err(Box::new(io::Error::other("state-only")))
        )
        .is_err()
    );
}

/// 帧证据必须明确保留“结构已校验、特征像素和系统合成未观察”，不能随成功退出升级为视觉通过。
#[test]
fn frame_evidence_keeps_unobserved_scope() {
    let evidence = frame_evidence();
    assert_eq!(evidence["source"], "gpui-render-to-image");
    assert_eq!(evidence["validation"], FRAME_VALIDATION);
    assert_eq!(evidence["featurePixelsChecked"], false);
    assert_eq!(evidence["systemComposition"], "not-observed");
    assert_eq!(ACCEPTANCE_MODE, "private-desktop-composition");
}
