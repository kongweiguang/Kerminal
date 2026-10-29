//! 使用生产组合根和一次性 WSL fixture 验收所有公开 MCP 工具，禁止读取用户数据根。
//!
//! @author kongweiguang

mod core;
mod terminals;

use std::{collections::BTreeSet, io::Write, path::PathBuf, sync::Arc, time::Duration};

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientInfo, InitializeRequestParams},
    service::RunningService,
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use tokio::{
    runtime::Handle,
    sync::Mutex,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

use super::DesktopComposition;

pub(super) struct Harness {
    pub client: RunningService<RoleClient, InitializeRequestParams>,
    pub fixture: Value,
    pub root: PathBuf,
    verified: Mutex<BTreeSet<String>>,
}

/// 必须显式 opt-in 并校验隔离 fixture，避免普通 cargo test 连接真实主机或启动业务 CLI。
/// 只在全部固定目录工具均有真实效果且组合根关停成功后写入回执。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires KERMINAL_MCP_ACCEPTANCE_MANIFEST and an isolated fixture"]
async fn all_retained_tools_work_over_live_mcp() {
    let manifest = std::env::var_os("KERMINAL_MCP_ACCEPTANCE_MANIFEST")
        .expect("explicit one-time fixture manifest is required");
    let output = PathBuf::from(
        std::env::var_os("KERMINAL_MCP_ACCEPTANCE_RECEIPT")
            .expect("explicit evidence output is required"),
    );
    assert!(!output.exists(), "acceptance receipt must be a new file");
    let fixture: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert_eq!(fixture["state"], "ready");
    let run_id = text(&fixture, "runId");
    assert!(run_id.len() == 12 && run_id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        fixture["wslRoot"],
        format!("/run/kerminal-right-rail-{run_id}")
    );
    assert_eq!(
        fixture["remoteSftpDirectory"],
        format!("/run/kerminal-right-rail-{run_id}/home/sftp"),
        "remote test paths must stay within the validated fixture root"
    );

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    // 隔离根同样遵守 vault 主密钥忽略规则。
    std::fs::write(root.join(".gitignore"), "secrets/vault-key.toml\n").unwrap();
    std::fs::create_dir(root.join("hosts")).unwrap();
    let source = PathBuf::from(text(&fixture, "gpuiConfigRoot"));
    std::fs::copy(
        source.join("hosts/right-rail-fixture.toml"),
        root.join("hosts/right-rail-fixture.toml"),
    )
    .unwrap();
    std::fs::copy(source.join("known_hosts"), root.join("known_hosts")).unwrap();

    let composition = Arc::new(
        DesktopComposition::build(Handle::current(), &root)
            .await
            .expect("production composition initializes"),
    );
    let endpoint = composition
        .mcp_runtime
        .start(CancellationToken::new())
        .await
        .expect("production MCP runtime starts")
        .endpoint
        .unwrap();
    let transport = StreamableHttpClientTransport::from_uri(endpoint.clone());
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("real RMCP HTTP initialization succeeds");
    let harness = Arc::new(Harness {
        client,
        fixture,
        root: root.clone(),
        verified: Mutex::new(BTreeSet::new()),
    });
    let shared = composition.clone();
    let scenario = harness.clone();
    // 独立任务捕获断言 panic，父任务仍持有生产资源 owner 并执行统一 deadline 清理。
    let outcome = tokio::spawn(async move {
        let harness = scenario;
        let tools = harness.client.peer().list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 20);
        core::run(&harness, &shared).await;
        terminals::run(&harness, &shared, &endpoint).await;
        let mut expected: BTreeSet<String> = tools.into_iter().map(|t| t.name.into_owned()).collect();
        expected.insert("kerminal.agent.target_context".into());
        assert_eq!(expected.len(), 21, "advertised tool names must be distinct");
        assert_eq!(*harness.verified.lock().await, expected, "every advertised tool requires a successful real invocation and scenario assertions");
        json!({"schemaVersion": 1, "fixtureRunId": run_id,
            "transport": "RMCP Streamable HTTP", "services": "DesktopComposition production adapters",
            "environment": {"clientOs": std::env::consts::OS, "sshServer": "WSL Ubuntu-22.04", "containerRuntime": harness.fixture["containerRuntime"]},
            "tools": expected, "passed": true})
    }).await;
    let cleanup_harness = harness.clone();
    let fixture_cleanup = tokio::spawn(async move { core::cleanup(&cleanup_harness).await }).await;
    let harness = Arc::try_unwrap(harness)
        .ok()
        .expect("scenario releases HTTP client");
    let client_cleanup = harness.client.cancel().await;
    let mut composition = Arc::try_unwrap(composition)
        .ok()
        .expect("scenario releases shared composition");
    let cleanup = composition
        .shutdown_until(Instant::now() + Duration::from_secs(30))
        .await;
    assert!(
        cleanup.is_ok(),
        "production resources must finish shutdown: {cleanup:?}"
    );
    assert!(
        client_cleanup.is_ok(),
        "HTTP client must close: {client_cleanup:?}"
    );
    assert!(
        fixture_cleanup.is_ok(),
        "fixture resources must close: {fixture_cleanup:?}"
    );
    assert!(outcome.is_ok(), "live MCP scenario failed: {outcome:?}");
    let mut evidence = outcome.unwrap();
    evidence["shutdownVerified"] = json!(true);
    evidence["completedAtUnixSeconds"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    drop(composition);
    temporary.close().unwrap();
    // create_new 避免并发执行或重试覆盖上一轮成功回执。
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .expect("acceptance receipt must be created without replacing existing evidence")
        .write_all(&serde_json::to_vec_pretty(&evidence).unwrap())
        .unwrap();
}

impl Harness {
    /// 仅记录真实 HTTP 成功回执；场景还必须断言业务效果，参数和凭据不写日志或证据。
    async fn envelope(&self, name: &'static str, args: Value) -> Value {
        let result = timeout(
            Duration::from_secs(65),
            self.client.peer().call_tool(
                CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
            ),
        )
        .await
        .expect("MCP request has a bounded deadline")
        .expect("MCP request reaches live server");
        let value = result
            .structured_content
            .expect("production structured envelope");
        assert_ne!(result.is_error, Some(true), "{name} failed: {value}");
        if self.verified.lock().await.insert(name.into()) {
            println!("live MCP OK: {name}");
        }
        value
    }

    /// 统一剥离成功 envelope，让场景只断言与该工具有关的业务数据。
    async fn call(&self, name: &'static str, args: Value) -> Value {
        self.envelope(name, args).await["data"].clone()
    }

    /// 错误场景必须收到明确失败并验证错误族，不能把错误返回计入成功覆盖集合。
    async fn error(&self, name: &'static str, args: Value) -> Value {
        let result = self
            .client
            .peer()
            .call_tool(
                CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            result.is_error,
            Some(true),
            "{name} must reject invalid state"
        );
        result.structured_content.unwrap()
    }

    /// 经同一个 MCP endpoint 执行夹具命令并检查远端退出码，禁止以受理成功代替命令成功。
    async fn ssh(&self, command: &str) -> String {
        let result = self
            .call(
                "ssh.command",
                json!({"hostId": "right-rail-fixture", "command": command, "timeoutSeconds": 30}),
            )
            .await;
        let output = &result["output"];
        assert_eq!(
            output["exitCode"], 0,
            "fixture command exit status: {result}"
        );
        text(output, "stdout")
    }

    /// 只信任队列权威终态，并保留失败/取消任务供相应恢复场景检查。
    async fn wait_transfer(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(50);
        loop {
            let result = self
                .call("sftp.transfer.list", json!({"transferId": id}))
                .await;
            let transfer = &result["transfers"][0];
            let status = text(transfer, "status");
            if [
                "succeeded",
                "unchanged",
                "failed",
                "canceled",
                "skipped",
                "outcomeUnknown",
            ]
            .contains(&status.as_str())
            {
                return transfer.clone();
            }
            assert!(
                Instant::now() < deadline,
                "transfer did not reach terminal state: {transfer}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// 强制必有字段具有契约类型，避免 Null 自动转空文本掩盖响应适配缺陷。
fn text(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing string {key}: {value}"))
        .to_owned()
}
