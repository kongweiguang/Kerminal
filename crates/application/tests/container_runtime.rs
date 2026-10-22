//! 容器运行时列表解析回归；只通过公开用例入口注入命令输出。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerRuntimeService, ContainerStageCleanupCapability, ContainerStageCleanupRequest,
        OperationsCommandExecutor, OperationsCommandStream,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalInputEvent, TerminalPalette, TerminalSearchRequest,
        TerminalSearchResult, TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
};
use kerminal_domain::{
    files::ContainerRuntime,
    identity::SessionId,
    operations::{
        ComposeAction, ComposeProjectTarget, ContainerListQuery, ContainerStatus, ContainerTarget,
    },
    sessions::{SessionInfo, TerminalSize},
};
use tokio_util::sync::CancellationToken;

struct FixedCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    invocations: Mutex<Vec<(CommandExecutionTarget, CommandInvocation)>>,
}

impl FixedCommandOutput {
    /// 命令端口只回放测试给定的 stdout，并记录真实 service 发出的 runtime argv。
    fn new(stdout: impl Into<Vec<u8>>) -> Self {
        Self::with_exit_code(stdout, Some(0))
    }

    /// 只让错误回归显式指定进程退出码，普通成功夹具继续默认返回零。
    fn with_exit_code(stdout: impl Into<Vec<u8>>, exit_code: Option<i32>) -> Self {
        Self::with_result(stdout, Vec::new(), exit_code)
    }

    /// 为命令失败回归固定 stderr，避免启动外部 runtime 或把主机错误文本写入日志。
    /// @author kongweiguang
    fn with_result(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
        exit_code: Option<i32>,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            invocations: Mutex::new(Vec::new()),
        }
    }

    /// 返回列表用例实际发出的结构化调用，确保测试经过 runtime 分流。
    fn invocations(&self) -> Vec<(CommandExecutionTarget, CommandInvocation)> {
        self.invocations
            .lock()
            .expect("command fixture mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl OperationsCommandExecutor for FixedCommandOutput {
    /// 记录本次 argv 并返回固定完整输出，不启动本机或远端命令。
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        self.invocations
            .lock()
            .expect("command fixture mutex should not be poisoned")
            .push((target, invocation));
        Ok(CommandExecutionResult {
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            exit_code: self.exit_code,
            truncated: false,
            input_complete: true,
        })
    }

    /// 列表只调用非交互 execute；意外请求流式命令即暴露测试接线错误。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        Err(AppError::unsupported(
            "container list fixture has no stream",
        ))
    }

    /// 列表不创建 staging 许可；意外进入传输路径时明确失败。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(AppError::unsupported(
            "container list fixture has no staging",
        ))
    }

    /// 固定命令输出没有后台 worker 需要停止。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 固定命令输出没有资源需要异步回收。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

struct UnusedSessions;

#[async_trait]
impl SessionService for UnusedSessions {
    /// 列表摘要测试不创建会话。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unused_session_api()
    }

    /// 列表摘要测试不创建可取消会话。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_session_api()
    }

    /// 列表摘要测试不查询会话目录。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        unused_session_api()
    }

    /// 列表摘要测试不读取运行时会话元数据。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unused_session_api()
    }

    /// 列表摘要测试不写入会话字节。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不注入终端输入事件。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unused_session_api()
    }

    /// 容器列表 fake 不具备可见 UI writer binding，拒绝任何命令行提交。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<kerminal_application::sessions::TerminalCommandContext> {
        unused_session_api()
    }

    /// 列表摘要测试不调整终端网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不读取 GPU 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unused_session_api()
    }

    /// 列表摘要测试不更新终端调色板。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不调整终端 scrollback。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unused_session_api()
    }

    /// 列表摘要测试不读取纯文本快照。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unused_session_api()
    }

    /// 列表摘要测试不滚动终端视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不执行带 UI owner 约束的滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不修改文本选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不提取终端选择文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unused_session_api()
    }

    /// 列表摘要测试不搜索终端 scrollback。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unused_session_api()
    }

    /// 列表摘要测试不重连会话。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_session_api()
    }

    /// 列表摘要测试不关闭单个会话。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不请求带 deadline 的会话回收。
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: SessionDeadline,
    ) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不关闭会话集合。
    async fn close_all(&self) -> AppResult<()> {
        unused_session_api()
    }

    /// 列表摘要测试不订阅会话变化。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unused_session_api()
    }

    /// 列表摘要测试不发送受管 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        unused_session_api()
    }
}

/// 容器摘要测试不需要 session 能力；任何意外调用都明确失败。
fn unused_session_api<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "container list fixture does not use sessions",
    ))
}

/// 经公开 service 入口执行 Docker 的逐行 JSON 对象解析，并保留列表字段。
#[tokio::test]
async fn docker_ndjson_maps_string_names_labels_and_ports() {
    let stdout = concat!(
        "{\"ID\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",",
        "\"Names\":\"docker-web\",\"Image\":\"nginx:latest\",\"Status\":\"Up 2 minutes\",",
        "\"Ports\":\"0.0.0.0:8080->80/tcp, :::8443->443/tcp\",",
        "\"Labels\":\"com.docker.compose.project=demo,com.docker.compose.service=web,",
        "com.docker.compose.project.working_dir=/srv/demo,",
        "com.docker.compose.config_files=/srv/demo/compose.yml,/srv/demo/override.yml\"}\n",
        "\n",
        "{\"ID\":\"1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",",
        "\"Names\":\"docker-worker\",\"Image\":\"busybox:latest\",\"Status\":\"Exited (0)\",",
        "\"Ports\":\"\",\"Labels\":\"\"}"
    );
    let (service, executor) = service(stdout.as_bytes());

    let rows = service
        .list(query(ContainerRuntime::Docker), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "docker-web");
    assert_eq!(rows[0].image, "nginx:latest");
    assert_eq!(rows[0].status, ContainerStatus::Running);
    assert_eq!(
        rows[0].ports,
        vec![
            "0.0.0.0:8080->80/tcp".to_owned(),
            ":::8443->443/tcp".to_owned(),
        ]
    );
    assert_eq!(
        rows[0].compose.as_ref().unwrap().config_files,
        vec![
            "/srv/demo/compose.yml".to_owned(),
            "/srv/demo/override.yml".to_owned(),
        ]
    );
    assert_eq!(
        rows[0].compose.as_ref().unwrap().working_dir.as_deref(),
        Some("/srv/demo")
    );
    assert_eq!(rows[1].status, ContainerStatus::Exited);
    assert!(rows[1].labels.is_empty());
    assert_list_invocation(&executor, ContainerRuntime::Docker);
}

/// 经公开 service 入口解析 Podman 3.4.4 的数组、名称数组、标签 map 和端口映射数组。
#[tokio::test]
async fn podman_array_maps_null_and_populated_runtime_fields() {
    let stdout = br#"[
      {
        "Id":"2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Names":["stopped-fixture"],
        "Image":"docker.io/library/alpine:latest",
        "Status":"Exited (137) 5 minutes ago",
        "Labels":{"kerminal.acceptance.fixture":"container-list-shape"},
        "Ports":null
      },
      {
        "Id":"3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Names":["podman-web","web-alias"],
        "Image":"docker.io/library/nginx:latest",
        "Status":"Up 6 minutes",
        "Labels":{
          "com.docker.compose.project":"demo",
          "com.docker.compose.service":"web",
          "com.docker.compose.project.working_dir":"/srv/demo",
          "com.docker.compose.config_files":"/srv/demo/compose.yml,/srv/demo/override.yml"
        },
        "Ports":[
          {"hostPort":8080,"containerPort":80,"protocol":"tcp","hostIP":"0.0.0.0"},
          {"hostPort":8443,"containerPort":443,"protocol":"tcp","hostIP":"::"},
          {"hostPort":8081,"containerPort":81,"protocol":"tcp","hostIP":""}
        ]
      },
      {
        "Id":"4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Names":["podman-no-mappings"],
        "Image":"docker.io/library/alpine:latest",
        "Status":"Created",
        "Labels":null,
        "Ports":[]
      }
    ]"#;
    let (service, executor) = service(stdout);

    let rows = service
        .list(query(ContainerRuntime::Podman), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].name, "stopped-fixture");
    assert_eq!(rows[0].status, ContainerStatus::Exited);
    assert!(rows[0].ports.is_empty());
    assert_eq!(
        rows[0].labels,
        [(
            "kerminal.acceptance.fixture".to_owned(),
            "container-list-shape".to_owned()
        )]
    );
    assert_eq!(rows[1].name, "podman-web, web-alias");
    assert_eq!(rows[1].status, ContainerStatus::Running);
    assert_eq!(
        rows[1].ports,
        vec![
            "0.0.0.0:8080->80/tcp".to_owned(),
            "[::]:8443->443/tcp".to_owned(),
            "0.0.0.0:8081->81/tcp".to_owned(),
        ]
    );
    assert_eq!(
        rows[1].compose.as_ref().unwrap().config_files,
        vec![
            "/srv/demo/compose.yml".to_owned(),
            "/srv/demo/override.yml".to_owned(),
        ]
    );
    assert_eq!(
        rows[1].compose.as_ref().unwrap().working_dir.as_deref(),
        Some("/srv/demo")
    );
    assert_list_invocation(&executor, ContainerRuntime::Podman);
    assert_eq!(rows[2].name, "podman-no-mappings");
    assert!(rows[2].labels.is_empty());
    assert!(rows[2].ports.is_empty());
}

/// 两种 runtime 的合法空结果分别为空输出和 JSON 空数组。
#[tokio::test]
async fn empty_runtime_outputs_return_an_empty_list() {
    let (docker, docker_executor) = service(b" \n\t");
    let (podman, podman_executor) = service(b"[]\n");

    assert!(
        docker
            .list(query(ContainerRuntime::Docker), CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        podman
            .list(query(ContainerRuntime::Podman), CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_list_invocation(&docker_executor, ContainerRuntime::Docker);
    assert_list_invocation(&podman_executor, ContainerRuntime::Podman);
}

/// 任一记录或字段损坏都会拒绝整批结果，不能只返回前面的成功行或空默认值。
#[tokio::test]
async fn malformed_rows_and_podman_field_shapes_are_rejected() {
    let (docker, _) = service(
        br#"{"ID":"4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","Names":"ok","Image":"busybox","Status":"Up 1 second","Ports":"","Labels":""}
{"ID":"bad"}"#,
    );
    let (podman_names_error, _) = service(
        br#"[{"Id":"5123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","Names":"not-an-array","Image":"busybox","Status":"Up 1 second","Labels":{},"Ports":[]}]"#,
    );
    let (podman_ports_error, _) = service(
        br#"[{"Id":"6123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","Names":["bad-ports"],"Image":"busybox","Status":"Up 1 second","Labels":null,"Ports":{"containerPort":80}}]"#,
    );

    let docker_error = docker
        .list(query(ContainerRuntime::Docker), CancellationToken::new())
        .await
        .unwrap_err();
    let podman_names_error = podman_names_error
        .list(query(ContainerRuntime::Podman), CancellationToken::new())
        .await
        .unwrap_err();
    let podman_ports_error = podman_ports_error
        .list(query(ContainerRuntime::Podman), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        docker_error.public_code(),
        kerminal_application::PublicErrorCode::Infrastructure
    );
    assert_eq!(
        podman_names_error.public_code(),
        kerminal_application::PublicErrorCode::Infrastructure
    );
    assert_eq!(
        podman_ports_error.public_code(),
        kerminal_application::PublicErrorCode::Infrastructure
    );
}

/// 非零退出码优先于空输出解释，避免命令失败伪装成没有容器。
#[tokio::test]
async fn nonzero_runtime_exit_rejects_empty_or_whitespace_output() {
    for (runtime, stdout) in [
        (ContainerRuntime::Docker, &b""[..]),
        (ContainerRuntime::Docker, &b" \n\t"[..]),
        (ContainerRuntime::Podman, &b""[..]),
        (ContainerRuntime::Podman, &b" \n\t"[..]),
    ] {
        let (service, _) = service_with_exit_code(stdout, Some(17));
        let error = service
            .list(query(runtime), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::RemoteExit
        );
    }
}

/// 有效 JSON 也不能掩盖 runtime 的非零退出码，确保退出码检查发生在解析之前。
#[tokio::test]
async fn nonzero_runtime_exit_precedes_valid_json_parsing() {
    let docker_json = br#"{"ID":"7123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","Names":"docker-web","Image":"nginx:latest","Status":"Up 1 minute","Ports":"","Labels":""}"#;
    let podman_json = br#"[{"Id":"8123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","Names":["podman-web"],"Image":"nginx:latest","Status":"Up 1 minute","Ports":null,"Labels":{}}]"#;

    for (runtime, stdout) in [
        (ContainerRuntime::Docker, &docker_json[..]),
        (ContainerRuntime::Podman, &podman_json[..]),
    ] {
        let (service, _) = service_with_exit_code(stdout, Some(17));
        let error = service
            .list(query(runtime), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::RemoteExit
        );
    }
}

/// 只将 Podman 的精确 cgroup v2 stats 限制映射为保留 RemoteExit 类别的安全提示。
/// @author kongweiguang
#[tokio::test]
async fn podman_cgroup_v2_stats_failure_has_specific_safe_message() {
    let stderr = b"Container stats resource only available for cgroup v2\r\n";
    let (service, executor) = service_with_result(b"", stderr, Some(125));
    let error = service
        .stats(
            container_target(ContainerRuntime::Podman),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::RemoteExit
    );
    assert_eq!(
        error.public_message(),
        "Podman 容器统计不可用：该环境需要 cgroup v2"
    );
    let calls = executor.invocations();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1.arguments,
        vec![
            "stats".to_owned(),
            "--no-stream".to_owned(),
            "--format".to_owned(),
            "{{json .}}".to_owned(),
            "fixture-container".to_owned(),
        ]
    );
}

/// Podman CLI 可在固定诊断前加一次 `Error: `；只剥离该前缀仍精确得到安全提示。
/// @author kongweiguang
#[tokio::test]
async fn podman_cli_error_prefix_keeps_specific_stats_message() {
    let stderr = b"Error: Container stats resource only available for cgroup v2\r\n";
    let (service, _) = service_with_result(b"", stderr, Some(125));
    let error = service
        .stats(
            container_target(ContainerRuntime::Podman),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::RemoteExit
    );
    assert_eq!(
        error.public_message(),
        "Podman 容器统计不可用：该环境需要 cgroup v2"
    );
}

/// 相同 stderr 只在 Podman exit 125 且完整匹配时分类，其余失败维持通用退出结果。
/// @author kongweiguang
#[tokio::test]
async fn stats_capability_classification_rejects_other_runtime_codes_and_text() {
    let cases = [
        (
            ContainerRuntime::Docker,
            Some(125),
            &b"Container stats resource only available for cgroup v2"[..],
        ),
        (
            ContainerRuntime::Docker,
            Some(125),
            &b"Error: Container stats resource only available for cgroup v2"[..],
        ),
        (
            ContainerRuntime::Podman,
            Some(124),
            &b"Container stats resource only available for cgroup v2"[..],
        ),
        (
            ContainerRuntime::Podman,
            Some(125),
            &b"Container stats resource only available for cgroup v2\nextra diagnostic"[..],
        ),
        (
            ContainerRuntime::Podman,
            Some(125),
            &b"Error: Error: Container stats resource only available for cgroup v2"[..],
        ),
        (
            ContainerRuntime::Podman,
            Some(125),
            &b"Error: Container stats resource only available for cgroup v2\nextra diagnostic"[..],
        ),
        (
            ContainerRuntime::Podman,
            Some(125),
            &b"permission denied"[..],
        ),
    ];

    for (runtime, exit_code, stderr) in cases {
        let (service, _) = service_with_result(b"", stderr, exit_code);
        let error = service
            .stats(container_target(runtime), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::RemoteExit
        );
        assert_eq!(error.public_message(), "远端命令已退出");
    }
}

/// 构造本机 runtime 列表请求，避免测试依赖用户配置或外部服务。
fn query(runtime: ContainerRuntime) -> ContainerListQuery {
    ContainerListQuery {
        host_id: None,
        runtime,
        include_stopped: true,
    }
}

/// Stop 与 Down 发出不同命令；拆除保留精确项目目录供再次 Up，不添加删除数据卷标志。
#[tokio::test]
async fn compose_stop_and_down_are_distinct_and_removed_project_remains_startable() {
    let (service, executor) = service(b"");
    let target = ComposeProjectTarget {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        project: "fixture".to_owned(),
        working_dir: "/srv/fixture".to_owned(),
        config_files: vec!["compose.yml".to_owned(), "compose.override.yml".to_owned()],
    };
    for action in [ComposeAction::Stop, ComposeAction::Down, ComposeAction::Up] {
        service
            .compose_action(target.clone(), action, CancellationToken::new())
            .await
            .unwrap();
    }
    let calls = executor.invocations();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1.arguments.last().unwrap(), "stop");
    assert_eq!(calls[1].1.arguments.last().unwrap(), "down");
    assert_eq!(
        &calls[2].1.arguments[calls[2].1.arguments.len() - 2..],
        ["up", "--detach"]
    );
    for (_, invocation) in &calls {
        assert_eq!(
            invocation.working_directory.as_deref(),
            Some("/srv/fixture")
        );
        assert_eq!(
            &invocation.arguments[..7],
            [
                "compose",
                "--project-name",
                "fixture",
                "--file",
                "compose.yml",
                "--file",
                "compose.override.yml"
            ],
        );
        assert!(
            !invocation
                .arguments
                .iter()
                .any(|argument| argument == "--volumes" || argument == "-v")
        );
    }
    assert_eq!(
        service
            .known_compose_projects(&None, ContainerRuntime::Docker)
            .unwrap(),
        vec![target]
    );
    assert!(
        service
            .known_compose_projects(&None, ContainerRuntime::Podman)
            .unwrap()
            .is_empty()
    );
}

/// 同名但配置顺序或目录不同的项目必须各自保留，防止再次启动指向另一份部署。
#[tokio::test]
async fn compose_catalog_keeps_complete_target_identity() {
    let (service, _) = service(b"");
    let first = ComposeProjectTarget {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        project: "same-name".to_owned(),
        working_dir: "/srv/first".to_owned(),
        config_files: vec!["a.yml".to_owned(), "b.yml".to_owned()],
    };
    let mut second = first.clone();
    second.working_dir = "/srv/second".to_owned();
    let mut third = first.clone();
    third.config_files.reverse();
    for target in [&first, &second, &third] {
        service
            .compose_action(
                target.clone(),
                ComposeAction::Down,
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        service
            .known_compose_projects(&None, ContainerRuntime::Docker)
            .unwrap(),
        vec![first, second, third]
    );
}

/// 构造本机 runtime stats 目标，确保错误分类经过真实 stats 用例入口。
/// @author kongweiguang
fn container_target(runtime: ContainerRuntime) -> ContainerTarget {
    ContainerTarget {
        host_id: None,
        runtime,
        container_id: "fixture-container".to_owned(),
    }
}

/// 注入固定输出和不应被调用的会话端口，测试真实 ContainerRuntimeService 编排。
fn service(stdout: &[u8]) -> (ContainerRuntimeService, Arc<FixedCommandOutput>) {
    let executor = Arc::new(FixedCommandOutput::new(stdout.to_vec()));
    (
        ContainerRuntimeService::new(executor.clone(), Arc::new(UnusedSessions)),
        executor,
    )
}

/// 为退出码回归注入真实 executor 结果，成功场景仍通过上方默认构造器返回零。
fn service_with_exit_code(
    stdout: &[u8],
    exit_code: Option<i32>,
) -> (ContainerRuntimeService, Arc<FixedCommandOutput>) {
    let executor = Arc::new(FixedCommandOutput::with_exit_code(
        stdout.to_vec(),
        exit_code,
    ));
    (
        ContainerRuntimeService::new(executor.clone(), Arc::new(UnusedSessions)),
        executor,
    )
}

/// 注入 stdout、stderr 与退出码三元组，专供 stats 错误分类回归使用。
/// @author kongweiguang
fn service_with_result(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
) -> (ContainerRuntimeService, Arc<FixedCommandOutput>) {
    let executor = Arc::new(FixedCommandOutput::with_result(
        stdout.to_vec(),
        stderr.to_vec(),
        exit_code,
    ));
    (
        ContainerRuntimeService::new(executor.clone(), Arc::new(UnusedSessions)),
        executor,
    )
}

/// 验证公开用例按选定 runtime 发出完整列表 argv，且测试不启动外部命令。
fn assert_list_invocation(executor: &FixedCommandOutput, runtime: ContainerRuntime) {
    let calls = executor.invocations();
    assert_eq!(calls.len(), 1);
    let (target, invocation) = &calls[0];
    assert_eq!(target, &CommandExecutionTarget::Local);
    assert_eq!(
        invocation.program.as_str(),
        match runtime {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    );
    assert_eq!(
        invocation.arguments,
        vec![
            "ps".to_owned(),
            "--no-trunc".to_owned(),
            "--all".to_owned(),
            "--format".to_owned(),
            "{{json .}}".to_owned(),
        ]
    );
}
