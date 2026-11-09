//! 真实 Vault、Sidecar RPC 与环回 HTTP 的隔离验收，不读取用户配置。
//! @author kongweiguang

#![cfg(windows)]

use kerminal_application::{
    ApplicationTaskExecutor, connections::host_types::ExternalHostSecretService,
    plugin_ports::PluginProcessPort, plugin_settings::PluginSecretEdit,
};
use kerminal_domain::{
    identity::HostId,
    plugins::{PluginId, PluginVersion},
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore,
    plugins::{process::PluginProcessSpec, supervisor::ProcessSupervisor},
    vault::VaultStore,
};
use kerminal_plugin_protocol::{ConfigurationSchema, HostOperationParams, HostTestResult};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
    net::TcpListener,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// 按已有 Windows 进程验收方式定位 pwsh，并避免打开用户桌面窗口。
fn powershell() -> PathBuf {
    let output = Command::new("where.exe")
        .creation_flags(0x08000000)
        .arg("pwsh.exe")
        .output()
        .expect("locate pwsh");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("path")
            .lines()
            .next()
            .expect("pwsh"),
    )
}

/// 使用真实 JSON-RPC 和系统 HTTP 客户端，只向测试传入的环回地址发请求。
fn spec(root: &Path) -> PluginProcessSpec {
    let script = r#"
$ErrorActionPreference = 'Stop'
# 隔离环境没有继承 SystemRoot；Windows HTTP 系统服务仍需要系统目录。
$env:SystemRoot = [Environment]::GetFolderPath('Windows')
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $request = $line | ConvertFrom-Json
  if ($request.method -eq 'initialize') {
    $result = @{protocol=1;plugin=@{id='acme.hostcheck';version='1.0.0'}}
  } elseif ($request.method -eq 'host.test') {
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.UseProxy = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    try {
      $client.DefaultRequestHeaders.Add('Authorization', 'Bearer ' + $request.params.secrets.'/token')
      $response = $client.GetAsync([string]$request.params.config.url).GetAwaiter().GetResult()
      $result = @{ok=$response.IsSuccessStatusCode}
    } finally { $client.Dispose(); $handler.Dispose() }
  } elseif ($request.method -eq 'shutdown') {
    [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{}} | ConvertTo-Json -Compress -Depth 8))
    break
  } else { continue }
  [Console]::Out.WriteLine((@{jsonrpc='2.0';id=$request.id;result=$result} | ConvertTo-Json -Compress -Depth 8))
}
"#;
    PluginProcessSpec {
        id: PluginId::parse("acme.hostcheck").unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        executable: powershell(),
        arguments: vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(script),
        ],
        data_directory: root.join("plugin-data"),
        log_directory: root.join("plugin-logs"),
        locale: "zh-CN".into(),
        theme: "dark".into(),
        host_api: "1.0".into(),
    }
}

/// 真实加密凭据解密后只随本次 host.test 发送；本地服务确认收到并返回成功。
#[tokio::test]
async fn isolated_host_test_uses_real_vault_sidecar_and_loopback_http() {
    let root = tempfile::tempdir().expect("isolated root");
    let config = ConfigurationStore::initialize(root.path()).expect("isolated store");
    let vault = Arc::new(VaultStore::open(config.storage_coordinator()));
    let plugin = PluginId::parse("acme.hostcheck").unwrap();
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"url":{"type":"string"},"token":{"type":"string","x-secret":true}},"required":["url","token"]}"#).unwrap();
    let service = ExternalHostSecretService::new(plugin.clone(), schema, vault);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/health", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, peer) = listener.accept().unwrap();
        assert!(peer.ip().is_loopback());
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0 && request.len() < 8192);
            request.extend_from_slice(&buffer[..count]);
        }
        let text = String::from_utf8(request).unwrap().to_ascii_lowercase();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        text.contains("authorization: bearer fixture-host-secret")
    });
    let host = HostId::new();
    let mut edits = BTreeMap::new();
    edits.insert(
        "/token".into(),
        PluginSecretEdit::Replace(Zeroizing::new("fixture-host-secret".into())),
    );
    let saved = service
        .prepare(
            &host,
            json!({"url":endpoint.clone(),"token":null}),
            None,
            edits,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!saved.to_string().contains("fixture-host-secret"));
    let vault_text = std::fs::read_to_string(root.path().join("secrets/vault.toml")).unwrap();
    assert!(!vault_text.contains("fixture-host-secret"));
    let (values, secrets) = service
        .preview(
            &host,
            json!({"url":endpoint,"token":null}),
            Some(&saved),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(values["token"].is_null());
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let supervisor = ProcessSupervisor::new(vec![spec(root.path())], executor).unwrap();
    let generation = supervisor
        .activate(&plugin, Instant::now() + Duration::from_secs(15))
        .await
        .unwrap();
    let params = HostOperationParams {
        host_type: "http-endpoint".into(),
        config: values,
        secrets: Value::Object(secrets),
    };
    let result = supervisor
        .call(
            &plugin,
            generation,
            "host.test",
            serde_json::to_value(params).unwrap(),
            Instant::now() + Duration::from_secs(15),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(serde_json::from_value::<HostTestResult>(result).unwrap().ok);
    assert!(
        server.join().unwrap(),
        "fixture server did not receive the expected credential"
    );
    supervisor
        .shutdown_until(Instant::now() + Duration::from_secs(6))
        .await
        .expect("sidecar shutdown");
}
