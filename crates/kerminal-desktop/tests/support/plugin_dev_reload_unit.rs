//! 开发信号与冻结边界的回归；进程恢复由应用层真实生命周期测试覆盖。
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_domain::plugins::{PluginId, PluginVersion};
use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};

use super::*;

/// 构造真实协议解析过的最小快照，测试边界不依赖用户安装目录。
fn prepared(directory: PathBuf) -> PreparedPlugin {
    let manifest = parse_manifest(
        include_bytes!("../../../../tests/fixtures/plugins/manifests/valid.json"),
        ManifestPolicy::ThirdParty,
    )
    .expect("valid fixture");
    PreparedPlugin {
        id: PluginId::parse(&manifest.id).expect("id"),
        version: PluginVersion::parse(&manifest.version).expect("version"),
        directory,
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    }
}

/// 信号文件是协调协议，旧代次和重复代次都不能造成额外进程重启。
#[test]
fn signal_requires_a_new_positive_generation_and_absolute_directory() {
    let directory = std::env::temp_dir();
    let payload = |generation| {
        serde_json::to_vec(&json!({"generation": generation, "plugin_dir": directory}))
            .expect("signal")
    };
    assert!(decode_signal(&payload(0), 0).is_err());
    assert!(decode_signal(&payload(2), 1).expect("new").is_some());
    assert!(decode_signal(&payload(2), 2).expect("same").is_none());
    assert!(decode_signal(&payload(1), 2).expect("old").is_none());
    assert!(decode_signal(br#"{"generation":1,"plugin_dir":"relative"}"#, 0).is_err());
    assert!(decode_signal(br#"{"generation":1,"plugin_dir":"relative","other":1}"#, 0).is_err());
}

/// 普通视图与脚本内容可以变化，注册器持有的清单始终保持同一契约。
#[test]
fn frozen_contract_allows_view_and_resource_body_changes() {
    let frozen = prepared(std::env::temp_dir());
    let mut changed = frozen.clone();
    changed.native_views.insert(
        "note".into(),
        Arc::new(json!({"root":{"kind":"text","text":"new"}})),
    );
    changed
        .resources
        .insert("dist/index.js".into(), Arc::from(&b"new script"[..]));
    assert!(validate_frozen_contract(&frozen, &changed).is_ok());
}

/// 权限和贡献变化必须显式重启，不能让热加载绕过启动时冻结的注册器。
#[test]
fn frozen_contract_rejects_manifest_or_translation_changes() {
    let frozen = prepared(std::env::temp_dir());
    let mut changed = frozen.clone();
    Arc::make_mut(&mut changed.package).manifest.publisher = "other".into();
    assert!(validate_frozen_contract(&frozen, &changed).is_err());
    let mut changed = frozen.clone();
    changed.localizations.insert(
        "en".into(),
        BTreeMap::from([("title".into(), "Other".into())]),
    );
    assert!(validate_frozen_contract(&frozen, &changed).is_err());
}

/// 信号上限与缺失文件在实际文件 API 上验证，避免无限等待或无界读取。
#[tokio::test]
async fn signal_reader_bounds_input_and_accepts_missing_marker() {
    let directory = tempfile::tempdir().expect("temp root");
    let signal = directory.path().join("dev-reload.json");
    assert!(read_signal(&signal).await.expect("missing").is_none());
    tokio::fs::write(&signal, vec![b'x'; MAX_SIGNAL_BYTES as usize + 1])
        .await
        .expect("write");
    assert!(read_signal(&signal).await.is_err());
}

/// 外部写入者只能请求启动时选定的目录；另一个有效插件目录也不能替代它。
#[tokio::test]
async fn reload_validation_rejects_a_different_directory_before_reading_package() {
    let original = tempfile::tempdir().expect("original");
    let other = tempfile::tempdir().expect("other");
    let result = validate_reload(
        Arc::new(prepared(original.path().to_owned())),
        ReloadSignal {
            generation: 1,
            plugin_dir: other.path().to_owned(),
        },
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(AppError::Conflict { .. })));
}

/// 已取消的重载不会开始包校验，关闭流程无需等待未请求的磁盘扫描。
#[tokio::test]
async fn cancelled_reload_does_not_inspect_a_missing_directory() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let directory = std::env::temp_dir().join("kerminal-dev-reload-missing");
    let result = validate_reload(
        Arc::new(prepared(directory.clone())),
        ReloadSignal {
            generation: 1,
            plugin_dir: directory,
        },
        cancellation,
    )
    .await;
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
}

/// 回执必须表达已完成或已拒绝；安全文案不带目录及错误源。
#[tokio::test]
async fn receipt_distinguishes_completed_and_rejected_generations() {
    let directory = tempfile::tempdir().expect("root");
    let receipt = directory.path().join("dev-reload-result.json");
    write_receipt(&receipt, Some(4), Ok(()))
        .await
        .expect("success");
    let value: Value =
        serde_json::from_slice(&tokio::fs::read(&receipt).await.expect("read")).expect("json");
    assert_eq!(value, json!({"generation":4,"status":"reloaded"}));
    write_receipt(&receipt, Some(5), Err(&AppError::conflict("需要重启")))
        .await
        .expect("failure");
    let value: Value =
        serde_json::from_slice(&tokio::fs::read(&receipt).await.expect("read")).expect("json");
    assert_eq!(
        value,
        json!({"generation":5,"status":"rejected","message":"需要重启"})
    );
}

/// 任务已返回业务错误时必须丢弃已消费的句柄，第二次关闭不能再次 poll 它。
#[tokio::test]
async fn shutdown_clears_completed_error_before_retry() {
    let mut reloader = DevPluginReloader {
        cancellation: CancellationToken::new(),
        task: Some(tokio::spawn(async {
            Err(AppError::conflict("监听失败"))
        })),
    };
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    assert!(reloader.task.is_none());
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .is_ok()
    );
}

/// 任务 panic 后 JoinError 已消费句柄，重试仍应安全结束。
#[tokio::test]
async fn shutdown_clears_panicked_task_before_retry() {
    let mut reloader = DevPluginReloader {
        cancellation: CancellationToken::new(),
        task: Some(tokio::spawn(async { panic!("simulated watcher failure") })),
    };
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    assert!(reloader.task.is_none());
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .is_ok()
    );
}

/// 截止时间耗尽时任务尚未结束，必须保留原句柄等待后续明确释放。
#[tokio::test]
async fn shutdown_timeout_keeps_task_for_retry() {
    let (release, waiting) = tokio::sync::oneshot::channel::<()>();
    let mut reloader = DevPluginReloader {
        cancellation: CancellationToken::new(),
        task: Some(tokio::spawn(async move {
            let _ = waiting.await;
            Ok(())
        })),
    };
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_millis(20))
            .await
            .is_err()
    );
    assert!(reloader.task.is_some());
    release.send(()).expect("release task");
    assert!(
        reloader
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .is_ok()
    );
    assert!(reloader.task.is_none());
}
