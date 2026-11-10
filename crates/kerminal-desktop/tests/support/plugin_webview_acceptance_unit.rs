//! 插件网页验收的纯状态回归；不创建窗口或进程，不读取用户插件目录。
//! @author kongweiguang

use std::{collections::BTreeMap, error::Error, io, time::Instant};

use kerminal_application::AppError;
use kerminal_plugin_protocol::{Activation, Platform, PluginRuntime};

use super::{
    FrameProof, UiAndQuitReceiptFailure, UiAndShutdownFailure, combine_ui_shutdown, frame_proof,
    is_webview_resource_path, require_quit_receipt, sidecar_entry,
};

/// 同时失败必须保留两个真实错误，不能让界面失败掩盖未完成的资源回收。
#[test]
fn simultaneous_ui_and_shutdown_errors_are_preserved() {
    let ui: Box<dyn Error> = Box::new(io::Error::other("synthetic-ui"));
    let result = combine_ui_shutdown::<()>(Err(ui), Err(AppError::timeout("synthetic-shutdown")));
    let error = result.expect_err("同时失败不能返回成功");
    let text = error.to_string();
    assert!(text.contains("synthetic-ui"));
    let combined = error
        .downcast_ref::<UiAndShutdownFailure>()
        .expect("必须保留类型化的双错误");
    assert!(combined.source().is_some());
    // 关闭错误的操作名只留在内部类型，公开文案不得泄露内部细节；用稳定错误码核对保留事实。
    assert_eq!(combined.shutdown.public_code().as_str(), "timeout");
}

/// 任一单独失败也不能被另一个阶段的成功掩盖。
#[test]
fn ui_and_shutdown_individual_results_remain_distinct() {
    let ui: Box<dyn Error> = Box::new(io::Error::other("synthetic-ui-only"));
    assert!(combine_ui_shutdown::<()>(Err(ui), Ok(())).is_err());
    assert!(combine_ui_shutdown(Ok(()), Err(AppError::timeout("synthetic-close"))).is_err());
    assert!(combine_ui_shutdown(Ok(7), Ok(())).is_ok_and(|value| value == 7));
}

/// 界面成功但没有生产退出回执时必须失败，不能因为关闭调用返回成功就写出完成报告。
#[test]
fn successful_ui_without_quit_receipt_is_rejected() {
    assert!(require_quit_receipt::<u8>(Ok(1), Some(Instant::now())).is_ok_and(|value| value == 1));
    let error = require_quit_receipt::<u8>(Ok(1), None).expect_err("缺少退出回执不能返回成功");
    assert!(error.to_string().contains("生产退出截止时间回执"));
}

/// 缺失退出回执不能覆盖已有的网页验收错误，两个失败都必须留在同一条错误链中。
#[test]
fn missing_quit_receipt_does_not_hide_ui_failure() {
    let ui: Box<dyn Error> = Box::new(io::Error::other("synthetic-ui-before-quit"));
    let error = require_quit_receipt::<()>(Err(ui), None).expect_err("两个失败不能返回成功");
    let text = error.to_string();
    assert!(text.contains("synthetic-ui-before-quit"));
    assert!(text.contains("生产退出截止时间回执"));
    assert!(error.downcast_ref::<UiAndQuitReceiptFailure>().is_some());
}

/// 资源白名单必须与基础设施快照规则一致，验收不能私自放宽为任意扩展名。
#[test]
fn webview_resource_extensions_are_closed() {
    for path in [
        "web/index.html",
        "web/app.MJS",
        "l10n/en.json",
        "font.woff2",
    ] {
        assert!(is_webview_resource_path(path), "{path}");
    }
    for path in [
        "src/main.rs",
        "README.md",
        "bin/file-search.exe",
        "no-extension",
    ] {
        assert!(!is_webview_resource_path(path), "{path}");
    }
}

/// 入口必须按实际平台从冻结清单选择，不能把默认文件名或其他平台程序作为证据。
#[test]
fn sidecar_entry_follows_manifest_platform() {
    let runtime = PluginRuntime::Sidecar {
        main: BTreeMap::from([
            (Platform::WindowsX86_64, "bin/custom-worker.exe".to_owned()),
            (Platform::LinuxX86_64, "bin/linux-worker".to_owned()),
        ]),
        activation: Activation::OnDemand,
        extensions: BTreeMap::new(),
    };
    assert_eq!(
        sidecar_entry(&runtime, Platform::WindowsX86_64).unwrap(),
        "bin/custom-worker.exe"
    );
    assert!(sidecar_entry(&runtime, Platform::WindowsAarch64).is_err());
    assert!(
        sidecar_entry(
            &PluginRuntime::None {
                extensions: BTreeMap::new()
            },
            Platform::WindowsX86_64
        )
        .is_err()
    );
}

/// 通知和设置改用生产 Root 状态与 GPUI 离屏帧证明，不再要求私有桌面系统同帧；
/// 未知阶段仍拒绝，搜索页保留独立色块标记。
#[test]
fn notification_and_dialog_use_state_and_offscreen_proof() {
    assert_eq!(frame_proof("search").unwrap(), FrameProof::SearchMarker);
    assert_eq!(
        frame_proof("notification").unwrap(),
        FrameProof::OccludedState
    );
    assert_eq!(frame_proof("dialog").unwrap(), FrameProof::OccludedState);
    assert!(frame_proof("other").is_err());
}
