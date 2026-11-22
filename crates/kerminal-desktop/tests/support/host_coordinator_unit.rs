//! Host 队列背压与安装未知结果的宿主策略回归。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_application::{AppError, PublicErrorCode, updates::UpdateInstallResult};
use kerminal_ui::QuitIntent;
use single_application::{PrimaryInstanceShutdownHandle, SingleApplication};
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};

use super::{
    HostCommand, HostCoordinatorSender, HostRequest, HostResponse, PrimaryListener,
    PrimaryListenerRecovery, UpdateCompletionPath, classify_install_result, restart_arguments,
    restart_extensions_until,
};
use crate::external_activation::{ActivationRequest, activation_queue, encode_message};

/// 验证队列满时发送方会等待容量，因此通过 Root 的退出门禁不会静默丢失请求。
#[tokio::test]
async fn bounded_host_sender_waits_for_queue_capacity() {
    let (sender, mut receiver) = mpsc::channel(1);
    let sender = HostCoordinatorSender { sender };
    let (first_reply, _first_receiver) = tokio::sync::oneshot::channel();
    assert!(
        sender
            .sender
            .send(HostRequest {
                command: HostCommand::Quit {
                    intent: QuitIntent::Exit,
                    deadline: Instant::now() + Duration::from_secs(1),
                },
                completion: first_reply,
            })
            .await
            .is_ok()
    );

    let pending_sender = sender.clone();
    let pending = tokio::spawn(async move {
        pending_sender
            .submit(HostCommand::Finalize {
                deadline: Instant::now() + Duration::from_secs(1),
            })
            .await
    });
    tokio::task::yield_now().await;
    assert!(!pending.is_finished(), "队列满时不应丢弃或提前完成请求");

    let _first_request = receiver
        .recv()
        .await
        .unwrap_or_else(|| panic!("预填请求应存在"));
    let queued_request = receiver
        .recv()
        .await
        .unwrap_or_else(|| panic!("等待后进入队列的请求应存在"));
    assert!(
        queued_request
            .completion
            .send(HostResponse::Finalized(Ok(())))
            .is_ok()
    );
    assert!(matches!(
        pending.await.expect("等待发送任务").expect("收到响应"),
        HostResponse::Finalized(Ok(()))
    ));
}

/// 结果未知必须留在 Root 可见的人工核实状态，避免宿主提前退出隐藏恢复信息。
#[test]
fn unknown_install_result_keeps_the_window_open() {
    let result = UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown(
        "更新安装调用的最终结果未知",
    ));
    assert_eq!(
        classify_install_result(&result),
        UpdateCompletionPath::KeepOpen
    );
}

/// 资源回收耗尽预算后不得启动下一进程；此测试只走拒绝分支，不创建桌面或读取用户配置。
#[test]
fn extensions_restart_rejects_an_expired_shutdown_deadline() {
    let error = restart_extensions_until(
        Instant::now() - Duration::from_millis(1),
        &crate::plugins::arguments::PluginStartupOptions::default(),
    )
    .expect_err("expired restart must not spawn");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
}

/// 重启只保留插件开关，尤其安全模式不能恢复原开发目标；测试不修改 argv 或启动真实桌面。
#[test]
fn restart_arguments_preserve_only_valid_plugin_startup_options() {
    use crate::plugins::arguments::PluginStartupOptions;
    use std::ffi::OsString;

    assert!(
        restart_arguments(&PluginStartupOptions::default())
            .expect("plain restart")
            .is_empty()
    );
    let directory = std::env::temp_dir().join("kerminal-plugin-restart-中文");
    assert_eq!(
        restart_arguments(&PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(directory.clone())
        })
        .expect("absolute dev path"),
        vec![
            OsString::from("--dev-plugin"),
            directory.as_os_str().to_owned()
        ]
    );
    assert_eq!(
        restart_arguments(&PluginStartupOptions {
            safe_mode: true,
            dev_plugin: Some(directory)
        })
        .expect("safe mode wins"),
        vec![OsString::from("--safe-mode")]
    );
    assert!(
        restart_arguments(&PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some("relative-plugin".into())
        })
        .is_err()
    );
}

/// 核对 AppImage 重启命令的入口和环境，避免在并行单实例测试期间 fork 继承监听器。
#[cfg(target_os = "linux")]
#[test]
fn appimage_restart_targets_the_replaced_outer_file() {
    use kerminal_infrastructure::platform::appimage::restart_command;
    use std::{fs, path::Path};

    let sandbox = tempfile::tempdir().expect("创建隔离目录");
    let appdir = sandbox.path().join("old AppDir");
    let executable = appdir.join("usr/bin/kerminal");
    fs::create_dir_all(executable.parent().expect("bin 目录")).expect("创建 AppDir 布局");
    fs::write(appdir.join("AppRun"), "old").expect("写入 AppRun 标记");
    let appimage = sandbox.path().join("Kerminal 中文.AppImage");
    let command = restart_command(&executable, Some(appimage.as_os_str()), &appdir)
        .expect("合法重启入口")
        .expect("识别 AppImage");
    assert_eq!(command.get_program(), appimage.as_os_str());
    assert_eq!(command.get_current_dir(), Some(sandbox.path()));
    for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none())
        );
    }
    assert!(
        restart_command(&executable, None, &appdir)
            .expect("无外层包")
            .is_none()
    );
    assert!(
        restart_command(
            Path::new("/usr/bin/kerminal"),
            Some(appimage.as_os_str()),
            &appdir
        )
        .expect("普通二进制")
        .is_none()
    );
    assert!(restart_command(&executable, Some(executable.as_os_str()), &appdir).is_err());
    assert!(
        restart_command(
            &executable,
            Some(Path::new("relative").as_os_str()),
            &appdir
        )
        .is_err()
    );
}

/// 成功安装后的重启必须先释放单实例入口，避免新版被误判成第二实例后立即退出。
#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn update_restart_releases_the_listener_before_spawning() {
    use super::{UpdateHostAction, restart_after_install};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let instance_id = format!("kerminal-update-restart-test-{}", std::process::id());
    let SingleApplication::PrimaryInstance(primary) =
        SingleApplication::<String>::new(&instance_id)
    else {
        panic!("隔离测试实例必须能监听");
    };
    let (activation_sender, _activation_receiver) = activation_queue();
    let listener = Arc::new(Mutex::new(PrimaryListener::new(
        instance_id,
        activation_sender,
        tokio::runtime::Handle::current(),
        primary,
    )));
    let action = restart_after_install(&listener, || {
        let closed = listener.try_lock().expect("启动前应释放 listener 锁");
        assert!(closed.task.is_none());
        assert!(closed.package_listener.is_none());
        Ok(())
    })
    .await;
    assert!(matches!(action, UpdateHostAction::Quit));
}

/// 验证恢复超时保留其错误分类，并在原 instance ID 上恢复投递队列而不并行留旧 listener。
///
/// @author kongweiguang
#[tokio::test]
async fn primary_listener_restores_same_instance_and_activation_queue() {
    let instance_id = format!(
        "kerminal-host-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let primary = match SingleApplication::<String>::new(&instance_id) {
        SingleApplication::PrimaryInstance(primary) => primary,
        SingleApplication::SecondaryInstance(_) => {
            panic!("new test instance ID should not already be occupied")
        }
    };
    let (activation_sender, mut activation_receiver) = activation_queue();
    let mut listener = PrimaryListener::new(
        instance_id.clone(),
        activation_sender,
        tokio::runtime::Handle::current(),
        primary,
    );

    match listener
        .restore_until(Instant::now() - Duration::from_millis(1))
        .await
    {
        PrimaryListenerRecovery::Failed(error) => {
            assert_eq!(error.public_code(), PublicErrorCode::Timeout);
        }
        _ => panic!("已过期的恢复截止时间必须保留 Timeout 错误分类"),
    }

    let second_instance = match SingleApplication::<String>::new(&instance_id) {
        SingleApplication::SecondaryInstance(secondary) => secondary,
        SingleApplication::PrimaryInstance(_) => {
            panic!("the live listener should own the test instance ID")
        }
    };
    second_instance
        .send_message(encode_message(&ActivationRequest::FocusWindow).expect("encode focus"))
        .await;
    assert_eq!(
        timeout(Duration::from_secs(2), activation_receiver.recv())
            .await
            .expect("message should reach the original queue"),
        Some(ActivationRequest::FocusWindow)
    );

    listener
        .stop_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("old listener and consumer should join");
    assert!(matches!(
        listener
            .restore_until(Instant::now() + Duration::from_secs(2))
            .await,
        PrimaryListenerRecovery::Restored
    ));

    let second_instance = match SingleApplication::<String>::new(&instance_id) {
        SingleApplication::SecondaryInstance(secondary) => secondary,
        SingleApplication::PrimaryInstance(_) => {
            panic!("restored listener should reclaim the same instance ID")
        }
    };
    second_instance
        .send_message(encode_message(&ActivationRequest::FocusWindow).expect("encode focus"))
        .await;
    assert_eq!(
        timeout(Duration::from_secs(2), activation_receiver.recv())
            .await
            .expect("restored listener should use the original queue"),
        Some(ActivationRequest::FocusWindow)
    );

    listener
        .stop_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("restored listener should join cleanly");
}

/// 其他主进程抢占旧 ID 时必须返回 Occupied，不保留“已恢复”或并行 accept-loop。
#[tokio::test]
async fn primary_listener_reports_occupied_instance_without_claiming_recovery() {
    let instance_id = format!(
        "kerminal-host-test-occupied-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    );
    let primary = match SingleApplication::<String>::new(&instance_id) {
        SingleApplication::PrimaryInstance(primary) => primary,
        SingleApplication::SecondaryInstance(_) => {
            panic!("new test instance ID should not already be occupied")
        }
    };
    let (activation_sender, _activation_receiver) = activation_queue();
    let mut listener = PrimaryListener::new(
        instance_id.clone(),
        activation_sender,
        tokio::runtime::Handle::current(),
        primary,
    );
    listener
        .stop_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("old listener and consumer should join");

    let competing_primary = match SingleApplication::<String>::new(&instance_id) {
        SingleApplication::PrimaryInstance(primary) => primary,
        SingleApplication::SecondaryInstance(_) => {
            panic!("old listener should have released the instance ID")
        }
    };
    assert!(matches!(
        listener
            .restore_until(Instant::now() + Duration::from_secs(2))
            .await,
        PrimaryListenerRecovery::Occupied
    ));

    let shutdown: PrimaryInstanceShutdownHandle = competing_primary.shutdown_handle();
    drop(competing_primary);
    shutdown
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("competing test listener should stop");
}
