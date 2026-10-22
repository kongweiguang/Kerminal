// @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    files::FileAccessProvider,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, ContainerStageCleanupRequest, OperationsCommandExecutor,
        OperationsCommandStream,
    },
};
use kerminal_domain::files::{
    ContainerRuntime, FileEndpoint, FileEntryKind, FileRevision, RemotePath, WriteTextRequest,
    WriteTextStrategy,
};
use kerminal_infrastructure::operations::ContainerFileProvider;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
struct RecordedCall {
    target: CommandExecutionTarget,
    invocation: CommandInvocation,
    input: Option<Vec<u8>>,
}

struct FakeExecutor {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    response: Arc<Mutex<Option<AppResult<CommandExecutionResult>>>>,
}

impl FakeExecutor {
    /// 固定返回值并记录结构化 argv，测试容器 adapter 的边界而不启动真实 runtime。
    fn new(response: CommandExecutionResult) -> (Self, Arc<Mutex<Vec<RecordedCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                response: Arc::new(Mutex::new(Some(Ok(response)))),
            },
            calls,
        )
    }

    /// 构造带确定错误结果的命令端口，验证变更调用不会把丢失回执伪装成未执行。
    fn with_error(error: kerminal_application::AppError) -> (Self, Arc<Mutex<Vec<RecordedCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                response: Arc::new(Mutex::new(Some(Err(error)))),
            },
            calls,
        )
    }

    /// 一次测试仅发起一条命令；消费响应能让重复调用错误直接暴露。
    fn take_response(&self) -> AppResult<CommandExecutionResult> {
        self.response
            .lock()
            .expect("test mutex poisoned")
            .take()
            .ok_or_else(|| kerminal_application::AppError::infrastructure("测试响应缺失"))?
    }
}

#[async_trait]
impl OperationsCommandExecutor for FakeExecutor {
    /// 记录普通执行参数并返回夹具响应，不触发本机子进程或 SSH。
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        self.calls
            .lock()
            .expect("test mutex poisoned")
            .push(RecordedCall {
                target,
                invocation,
                input: None,
            });
        self.take_response()
    }

    /// 记录 stdin 负载，确认文件内容不会进入 argv 或固定 shell 程序文本。
    async fn execute_with_input(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        input: Vec<u8>,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        self.calls
            .lock()
            .expect("test mutex poisoned")
            .push(RecordedCall {
                target,
                invocation,
                input: Some(input),
            });
        self.take_response()
    }

    /// 文件 fixture 保持有界整块响应；未配置双向 writer/reader 生命周期时明确拒绝长流。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        Err(kerminal_application::AppError::unsupported(
            "测试文件 executor 未配置流式命令",
        ))
    }

    /// 该控制型夹具不持有真实容器资源，因此拒绝签发 production cleanup capability。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(kerminal_application::AppError::unsupported(
            "测试文件 executor 不签发容器 staging 清理能力",
        ))
    }

    /// 夹具没有后台命令或预留清理资源，quiesce 是确定的空操作。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 该夹具不创建后台命令资源，因此退出清理在测试中显式完成为空操作。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 保留未经容器规范化的领域路径，验证基础设施入口不会依赖某一个上游 parser。
fn container_endpoint(path: &str) -> FileEndpoint {
    FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "kerminal-test".to_owned(),
        path: RemotePath::parse(path).expect("valid fixture path"),
    }
}

/// 包装一条不截断的 runtime 执行结果，减少测试关注点与底层状态重复。
fn result(stdout: Vec<u8>) -> CommandExecutionResult {
    result_with_exit_code(stdout, 0)
}

/// 为运行时错误映射构造确定退出码，避免测试启动 Docker 或 Podman。
fn result_with_exit_code(stdout: Vec<u8>, exit_code: i32) -> CommandExecutionResult {
    CommandExecutionResult {
        stdout,
        stderr: Vec::new(),
        exit_code: Some(exit_code),
        truncated: false,
        input_complete: true,
    }
}

/// 容器适配器入口统一处理 Windows 分隔符、相对路径与内部父级段。
#[tokio::test]
async fn container_path_is_normalized_before_command_and_in_listing() {
    let (executor, calls) = FakeExecutor::new(result(Vec::new()));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let input_path = r"config\.\nested\..\settings.toml";
    let expected_path = RemotePath::parse_container("/config/settings.toml")
        .expect("expected normalized container path");

    let listing = provider
        .list_directory(CancellationToken::new(), container_endpoint(input_path))
        .await
        .expect("normalized path can be listed");

    assert_eq!(
        listing.directory,
        FileEndpoint::Container {
            host_id: None,
            runtime: ContainerRuntime::Docker,
            container_id: "kerminal-test".to_owned(),
            path: expected_path,
        }
    );
    let recorded = calls.lock().expect("test mutex poisoned");
    assert!(
        recorded[0]
            .invocation
            .arguments
            .contains(&"/config/settings.toml".to_owned())
    );
    assert!(
        !recorded[0]
            .invocation
            .arguments
            .contains(&input_path.to_owned())
    );
}

/// 验证非普通文本路径只作为 argv 传递，目录项类型和顺序来自远端 metadata。
#[tokio::test]
async fn directory_listing_preserves_types_and_keeps_path_as_argv() {
    let mut output = Vec::new();
    for field in [
        b"file with spaces".as_slice(),
        b"f",
        b"12",
        b"1700000000.0000000000",
        b"644",
        b"folder",
        b"d",
        b"0",
        b"1700000001.0000000000",
        b"755",
        b"link",
        b"l",
        b"4",
        b"1700000002.0000000000",
        b"777",
    ] {
        output.extend_from_slice(field);
        output.push(0);
    }
    let (executor, calls) = FakeExecutor::new(result(output));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let path = "/tmp/$(do-not-run)";

    let listing = provider
        .list_directory(CancellationToken::new(), container_endpoint(path))
        .await
        .expect("well-formed listing");

    assert_eq!(listing.entries.len(), 3);
    assert_eq!(listing.entries[0].name, "folder");
    assert_eq!(listing.entries[0].kind, FileEntryKind::Directory);
    assert_eq!(listing.entries[1].kind, FileEntryKind::File);
    assert_eq!(listing.entries[2].kind, FileEntryKind::Symlink);
    let recorded = calls.lock().expect("test mutex poisoned");
    let command = &recorded[0].invocation.arguments;
    assert!(command.iter().any(|argument| argument == path));
    assert!(!command[5].contains(path));
    assert!(matches!(recorded[0].target, CommandExecutionTarget::Local));
}

/// 容器 Unix 目录按点文件语义给 UI 提供隐藏元数据，不依赖权限文本或 raw 行。
///
/// @author kongweiguang
#[tokio::test]
async fn directory_listing_marks_dotfiles_hidden() {
    let mut output = Vec::new();
    for field in [
        b".secret".as_slice(),
        b"f",
        b"6",
        b"1700000000.0000000000",
        b"600",
    ] {
        output.extend_from_slice(field);
        output.push(0);
    }
    let (executor, _) = FakeExecutor::new(result(output));
    let provider = ContainerFileProvider::new(Arc::new(executor));

    let listing = provider
        .list_directory(CancellationToken::new(), container_endpoint("/tmp"))
        .await
        .expect("dotfile listing succeeds");

    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, ".secret");
    assert!(listing.entries[0].is_hidden);
}

/// 验证领域路径中的父级段在任何 CLI 调用之前被拒绝。
#[tokio::test]
async fn parent_traversal_is_rejected_before_runtime_execution() {
    let (executor, calls) = FakeExecutor::new(result(Vec::new()));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let error = provider
        .list_directory(CancellationToken::new(), container_endpoint("/../etc"))
        .await
        .expect_err("parent traversal must fail closed");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert!(calls.lock().expect("test mutex poisoned").is_empty());
}

/// 写入命令已交给容器运行时后若取消回执，调用方必须按未知结果回读而不能盲目重试。
#[tokio::test]
async fn container_write_maps_lost_cancel_receipt_to_outcome_unknown() {
    let (executor, calls) = FakeExecutor::with_error(kerminal_application::AppError::cancelled(
        "测试取消期间回执丢失",
    ));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let request = WriteTextRequest {
        endpoint: container_endpoint("/tmp/kerminal-write-test.txt"),
        content: "test payload".to_owned(),
        strategy: WriteTextStrategy::ExplicitOverwrite,
    };

    let error = provider
        .write_text_file(CancellationToken::new(), request)
        .await
        .expect_err("lost write receipt must not look like a confirmed cancellation");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let recorded = calls.lock().expect("test mutex poisoned");
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].input.is_some());
}

/// 未确认完整 stdin 的写命令须保留退出回执供回读，且错误展示不得泄露文件字节。
///
/// @author kongweiguang
#[tokio::test]
async fn container_write_with_unconfirmed_input_is_unknown_and_redacted() {
    let mut response = result_with_exit_code(b"private-stdout".to_vec(), 0);
    response.stderr = b"private-stderr".to_vec();
    response.input_complete = false;
    let (executor, calls) = FakeExecutor::new(response);
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let error = provider
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: container_endpoint("/tmp/kerminal-write-test.txt"),
                content: "private-input".to_owned(),
                strategy: WriteTextStrategy::ExplicitOverwrite,
            },
        )
        .await
        .expect_err("unconfirmed input must require an authoritative reread");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let visible_error = format!("{error} {error:?}");
    for secret in ["private-input", "private-stdout", "private-stderr"] {
        assert!(!visible_error.contains(secret));
    }
    let recorded = calls.lock().expect("test mutex poisoned");
    assert_eq!(recorded.len(), 1, "未知结果不能自动重放写命令");
    assert_eq!(
        recorded[0].input.as_deref(),
        Some(b"private-input".as_slice())
    );
}

/// CompareRevision 的前置读取失败必须直接返回，不能退化为创建或显式覆盖。
#[tokio::test]
async fn compare_revision_missing_file_never_falls_back_to_write() {
    let (executor, calls) = FakeExecutor::with_error(kerminal_application::AppError::not_found(
        "测试目标文件不存在",
    ));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let error = provider
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: container_endpoint("/tmp/missing-config.txt"),
                content: "replacement".to_owned(),
                strategy: WriteTextStrategy::CompareRevision(FileRevision {
                    size: 4,
                    modified: Some("1700000000".to_owned()),
                    permissions: Some(0o644),
                    content_digest: Some("a".repeat(64)),
                }),
            },
        )
        .await
        .expect_err("missing comparison target must fail closed");

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::NotFound
    );
    let recorded = calls.lock().expect("test mutex poisoned");
    assert_eq!(recorded.len(), 1, "only the revision read is allowed");
    assert!(recorded[0].input.is_none());
    assert!(recorded[0].invocation.arguments[5].contains("head -c"));
    assert!(!recorded[0].invocation.arguments[5].contains("mv -fT"));
}

/// 目录创建命令保留递归与并发幂等规则，非目录目标由 adapter 映射为冲突。
#[tokio::test]
async fn create_directory_is_recursive_idempotent_and_type_safe() {
    let (executor, calls) = FakeExecutor::new(result(Vec::new()));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    provider
        .create_directory(CancellationToken::new(), container_endpoint("/tmp/a/b"))
        .await
        .expect("existing directory or successful mkdir is idempotent");
    {
        let recorded = calls.lock().expect("test mutex poisoned");
        let script = &recorded[0].invocation.arguments[5];
        assert!(script.contains("mkdir -p --"));
        assert!(script.contains("[ -d \"$p\" ] && [ ! -L \"$p\" ]"));
        assert!(script.contains("exit 42"));
    }

    let (executor, _) = FakeExecutor::new(result_with_exit_code(Vec::new(), 42));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let error = provider
        .create_directory(CancellationToken::new(), container_endpoint("/tmp/a/b"))
        .await
        .expect_err("an existing file is a typed conflict");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// 删除脚本将已经缺失的同类目标视为幂等完成，变更回执仍通过同一命令执行结果确认。
#[tokio::test]
async fn delete_missing_file_is_idempotent() {
    let (executor, calls) = FakeExecutor::new(result(Vec::new()));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    provider
        .delete(
            CancellationToken::new(),
            container_endpoint("/tmp/already-removed.txt"),
            kerminal_domain::files::DeleteTargetKind::File,
        )
        .await
        .expect("a missing file is already in the requested state");

    let recorded = calls.lock().expect("test mutex poisoned");
    let script = &recorded[0].invocation.arguments[5];
    assert!(script.contains("if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then exit 0; fi"));
    assert!(script.contains("rm -f --"));
}

/// 验证文件内容经 stdin 传输，且创建结果返回可用于后续 CAS 的 SHA-256。
#[tokio::test]
async fn create_new_sends_content_on_stdin_and_returns_digest_revision() {
    let mut response = b"4\0".to_vec();
    response.extend_from_slice(b"1700000000\0");
    response.extend_from_slice(b"644\0");
    let (executor, calls) = FakeExecutor::new(result(response));
    let provider = ContainerFileProvider::new(Arc::new(executor));
    let content = "data".to_owned();
    let result = provider
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: container_endpoint("/tmp/config.txt"),
                content: content.clone(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await
        .expect("create completes");

    assert_eq!(result.bytes_written, 4);
    assert_eq!(
        result.revision.content_digest.as_deref().map(str::len),
        Some(64)
    );
    let recorded = calls.lock().expect("test mutex poisoned");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].input.as_deref(), Some(content.as_bytes()));
    assert!(
        !recorded[0]
            .invocation
            .arguments
            .iter()
            .any(|argument| argument == &content)
    );
    assert!(recorded[0].invocation.environment.is_empty());
    assert!(
        recorded[0]
            .invocation
            .arguments
            .contains(&"--interactive".to_owned())
    );
    assert!(recorded[0].invocation.arguments[5].contains("$p"));
}
