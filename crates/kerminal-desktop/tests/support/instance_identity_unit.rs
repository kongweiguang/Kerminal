//! 单实例身份必须与隔离配置根一一对应。
//!
//! @author kongweiguang

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use kerminal_application::{AppError, ApplicationTaskExecutor};
use single_application::SingleApplication;
use tokio::time::Instant;

use super::{
    APPLICATION_ID, ApplicationRuntime, activate_existing_instance, normalize_path,
    single_instance_id, spawn_window_state_flush,
};

static NEXT_TEST_INSTANCE: AtomicU64 = AtomicU64::new(1);

struct ThreadUnparker(std::thread::Thread);

impl Wake for ThreadUnparker {
    /// Tokio oneshot 通知外部测试线程恢复轮询，不要求调用线程进入 Tokio runtime。
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    /// 引用式唤醒保留同一测试线程句柄，避免额外的 runtime 或 executor 依赖。
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// 在纯 std future executor 上等待，回归 GPUI 一类调用线程不具备 Tokio context 的边界。
fn block_on_without_tokio<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadUnparker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// 生成独立测试 socket 身份，不会连接或探测正式产品实例。
fn unique_test_instance_id() -> String {
    let sequence = NEXT_TEST_INSTANCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{APPLICATION_ID}.runtime-context-test-{}-{sequence}",
        std::process::id()
    )
}

/// 只持有当前测试独占的目录，避免 Windows 真实目录解析接触正式配置或共享静态 fixture。
struct InstanceConfigRoot {
    absolute: PathBuf,
    #[cfg(windows)]
    relative: PathBuf,
}

impl InstanceConfigRoot {
    /// 在当前测试工作目录下生成独占的相对根，同时保留绝对写法用于验证真实别名。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        let sequence = NEXT_TEST_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let relative = PathBuf::from("target").join(format!(
            "instance-identity-unit-{}-{sequence}",
            std::process::id()
        ));
        let absolute = std::env::current_dir()
            .expect("test working directory should resolve")
            .join(&relative);
        assert!(!absolute.exists(), "test root must be exclusively owned");
        std::fs::create_dir_all(&absolute).expect("owned config fixture should be created");
        Self {
            absolute,
            #[cfg(windows)]
            relative,
        }
    }
}

impl Drop for InstanceConfigRoot {
    /// 仅清理本测试唯一生成的根；失败回归已经 panic 时不再制造 double panic。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.absolute);
    }
}

/// 两种构建模式都必须按目录隔离，不能与旧版固定产品身份或另一测试根互发激活消息。
/// 同时覆盖 Windows 首次目录尚不存在的启动。
///
/// @author kongweiguang
#[test]
fn different_config_roots_have_distinct_instance_ids() {
    let fixture = InstanceConfigRoot::new();
    let first_root = fixture.absolute.join("first");
    let second_root = fixture.absolute.join("second");
    let first =
        single_instance_id(&first_root).expect("first test root should produce an instance id");
    let second =
        single_instance_id(&second_root).expect("second test root should produce an instance id");
    #[cfg(windows)]
    {
        assert!(first_root.is_dir());
        assert!(second_root.is_dir());
    }

    assert_ne!(first, second);
    assert_ne!(first, APPLICATION_ID);
    assert_ne!(second, APPLICATION_ID);
    assert!(first.starts_with(APPLICATION_ID));
    assert!(second.starts_with(APPLICATION_ID));
}

/// 消除点段后保持实例身份稳定，避免相同配置根意外启动第二个主实例。
#[test]
fn path_normalization_collapses_parent_components() {
    assert_eq!(
        normalize_path(Path::new("target/acceptance/first/../shared")),
        normalize_path(Path::new("target/acceptance/shared")),
    );
}

/// Windows 的同目录别名必须投递给同一个真实 named-pipe owner，不能只比较两个计算值。
/// 目录名大小写是否为别名由真实文件系统决定，大小写敏感目录下的独立根仍必须保持独立。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_config_root_aliases_reuse_primary_and_focus_queue() {
    let fixture = InstanceConfigRoot::new();
    let root = fixture.absolute.join("MiXeDConfig");
    assert!(!root.exists());
    let instance_id =
        single_instance_id(&root).expect("first identity should create its config root");
    let canonical_root = std::fs::canonicalize(&root).expect("real config root should resolve");
    let mut aliases = vec![
        (root.clone(), true),
        (fixture.relative.join("MiXeDConfig"), true),
        (canonical_root.clone(), true),
    ];
    let root_text = root.to_str().expect("owned fixture path should be Unicode");
    if root_text.as_bytes().get(1) == Some(&b':') {
        let mut drive_case_alias = root_text.to_owned();
        drive_case_alias.replace_range(..1, &root_text[..1].to_ascii_lowercase());
        aliases.push((PathBuf::from(drive_case_alias), true));
    }
    let directory_case_alias = fixture.absolute.join("mixedconfig");
    let directory_case_is_same_root = match std::fs::canonicalize(&directory_case_alias) {
        Ok(path) => path == canonical_root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("case alias probe failed: {error}"),
    };
    aliases.push((directory_case_alias, directory_case_is_same_root));

    let runtime = ApplicationRuntime::new().expect("application runtime should start");
    let mut primary =
        match runtime.block_on(async { SingleApplication::<String>::new(&instance_id) }) {
            SingleApplication::PrimaryInstance(primary) => primary,
            SingleApplication::SecondaryInstance(_) => {
                panic!("owned root must acquire its first listener")
            }
        };
    for (alias, same_root) in aliases {
        let alias_id = single_instance_id(&alias).expect("config alias identity should resolve");
        if !same_root {
            assert_ne!(
                instance_id, alias_id,
                "distinct real directories must not merge"
            );
            let other_primary =
                match runtime.block_on(async { SingleApplication::<String>::new(&alias_id) }) {
                    SingleApplication::PrimaryInstance(primary) => primary,
                    SingleApplication::SecondaryInstance(_) => {
                        panic!("distinct directory must own a distinct listener")
                    }
                };
            runtime
                .block_on(
                    other_primary
                        .shutdown_handle()
                        .shutdown_until(Instant::now() + Duration::from_secs(2)),
                )
                .expect("distinct case-sensitive listener must join");
            continue;
        }
        assert_eq!(
            instance_id, alias_id,
            "one real directory must share its instance ID"
        );
        let secondary =
            match runtime.block_on(async { SingleApplication::<String>::new(&alias_id) }) {
                SingleApplication::SecondaryInstance(secondary) => secondary,
                SingleApplication::PrimaryInstance(_) => {
                    panic!("same config root must reuse its live primary")
                }
            };
        activate_existing_instance(&runtime, secondary, None)
            .expect("alias activation must reach the live primary");
        let wire = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), primary.recv())
                .await
                .expect("primary should receive bounded alias activation")
                .expect("primary activation channel must remain open")
        });
        assert_eq!(
            super::external_activation::decode_message(&wire).expect("alias message should decode"),
            super::external_activation::ActivationRequest::FocusWindow,
        );
    }
    runtime
        .block_on(
            primary
                .shutdown_handle()
                .shutdown_until(Instant::now() + Duration::from_secs(2)),
        )
        .expect("aliased primary listener must join");
}

/// 规范路径的前缀转换保持普通磁盘及 UNC 的旧 hash 表示，避免本次修复无谓迁移全部隔离实例名。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_instance_path_normalization_preserves_ordinary_prefixes() {
    assert_eq!(
        normalize_path(Path::new(r"\\?\C:\owned\config")),
        Path::new(r"C:\owned\config"),
    );
    assert_eq!(
        normalize_path(Path::new(r"\\?\UNC\server\share\owned\config")),
        Path::new(r"\\server\share\owned\config"),
    );
}

/// 不可用的根必须显式拒绝派生实例名，不能启动另一个字符串 scope 或改写原有文件。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_unavailable_config_root_identity_fails_without_fallback() {
    let fixture = InstanceConfigRoot::new();
    let occupied = fixture.absolute.join("occupied");
    let witness = b"KERMINAL_INSTANCE_ROOT_FILE_WITNESS";
    std::fs::write(&occupied, witness).expect("owned occupied-path witness should be written");
    let error =
        single_instance_id(&occupied).expect_err("file path cannot become a config directory");
    assert!(error.to_string().contains("无法为单实例准备配置目录"));
    assert!(occupied.is_file());
    assert_eq!(
        std::fs::read(&occupied)
            .expect("witness remains readable")
            .as_slice(),
        witness.as_slice()
    );
}

/// 从普通测试线程调用真实二次实例发送路径，确保 Tokio timeout 在 block_on runtime 内创建。
#[test]
fn secondary_instance_send_works_without_caller_tokio_context() {
    let runtime = ApplicationRuntime::new().expect("application runtime should start");
    let instance_id = unique_test_instance_id();
    let mut primary = match runtime
        .block_on(async { SingleApplication::<String>::new(&instance_id) })
    {
        SingleApplication::PrimaryInstance(primary) => primary,
        SingleApplication::SecondaryInstance(_) => panic!("isolated test identity must be free"),
    };
    let secondary = match runtime.block_on(async { SingleApplication::<String>::new(&instance_id) })
    {
        SingleApplication::SecondaryInstance(secondary) => secondary,
        SingleApplication::PrimaryInstance(_) => panic!("existing test listener must be reused"),
    };

    activate_existing_instance(&runtime, secondary, None)
        .expect("secondary send must work from a non-Tokio thread");
    let wire = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), primary.recv())
            .await
            .expect("primary should receive the bounded message")
            .expect("primary channel remains open")
    });
    assert_eq!(
        super::external_activation::decode_message(&wire).expect("message should decode"),
        super::external_activation::ActivationRequest::FocusWindow
    );

    runtime
        .block_on(
            primary
                .shutdown_handle()
                .shutdown_until(Instant::now() + Duration::from_secs(2)),
        )
        .expect("test listener must be joined");
}

/// 证明 GPUI 外部 future executor 可以 await 共享 Tokio 执行器的 oneshot 回执。
#[test]
fn application_task_executor_accepts_non_tokio_waiter() {
    let runtime = ApplicationRuntime::new().expect("application runtime should start");
    let executor = ApplicationTaskExecutor::new(runtime.handle());
    let result = block_on_without_tokio(executor.run(|_| async { Ok::<_, AppError>(42_u8) }));

    assert_eq!(result.expect("background task should finish"), 42);
    runtime
        .block_on(executor.shutdown())
        .expect("tracked task must be joined before runtime shutdown");
}

/// 验证 UI 风格 executor 等待窗口 flush 回执时，Tokio deadline 由共享 runtime 内部驱动。
#[test]
fn window_state_flush_bridge_accepts_non_tokio_waiter() {
    let runtime = ApplicationRuntime::new().expect("application runtime should start");
    let receipt = spawn_window_state_flush(
        runtime.handle(),
        async { Ok::<(), AppError>(()) },
        Instant::now() + Duration::from_secs(2),
    );

    assert_eq!(
        block_on_without_tokio(receipt).expect("runtime task should return a receipt"),
        None
    );
}
