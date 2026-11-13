//! `asset-url` 子命令的批量 JSON 契约回归，复用协议契约测试的黄金样本。
//!
//! 样本列表与 `sdk/protocol/tests/asset_url_contract.rs` 保持逐字一致；调整时必须
//! 同步两侧。本测试通过真实二进制验证 stdin/stdout 行为：不联网、不读凭据，
//! 含拒绝条目的批量请求以非零状态失败且逐条可区分。
//!
//! @author kongweiguang

use std::process::{Command, Stdio};

use serde_json::Value;

/// 必须被接受的输入：与协议契约测试的 ACCEPTED 逐字一致。
const ACCEPTED: &[&str] = &[
    "https://example.com/asset",
    "http://example.com/asset?version=1&mirror=cn",
    "https://example.com/@user/asset",
    "https://example.com/a?x=@y&mirror=cn",
    "https://example.com/路径@名/asset",
    "https://example.com/a b/asset",
    "http://127.0.0.1:8080/asset",
    "http://10.0.0.5/asset",
    "https://[::1]:8443/asset?mirror=cn",
    "https://[2001:db8::1]:8443/asset",
    "https://example.com:8443/asset",
    "HTTPS://EXAMPLE.COM/asset",
    "https://例子.测试/asset",
    "https://xn--2ba.tv/asset",
    "https://xn--fsq.tv/path",
    "http://lan-host.local/asset",
];

/// 必须被拒绝的输入：与协议契约测试的 REJECTED 逐字一致（期望原因在断言中体现）。
const REJECTED: &[&str] = &[
    "ftp://example.com/asset",
    "data:text/plain,asset",
    "https://user:pass@example.com/asset",
    "https://user@example.com/asset",
    "http://@emptyuser.com/asset",
    "http://:@emptypass.com/asset",
    "http://user:@host.example/asset",
    "http://:pass@host.example/asset",
    "https://@/asset",
    "https://example.com/asset#fragment",
    "",
    "https:///missing",
    "https:////example.com/asset",
    "https:/example.com/asset",
    "http:example.com/asset",
    "http:",
    "https://example.com/a\tb",
    " https://example.com/asset",
    "https://example.com/asset ",
    "https://exa\u{7f}mple.com/asset",
    "https://example.com/a\\b",
    "https://example.com\\asset",
    "http://example.com:0/asset",
    "https://example.com:0/asset",
];

/// 在临时目录中运行 CLI，注入 stdin 并捕获 stdout/stderr。
fn run_cli(stdin: &str) -> (std::process::Output, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin"))
        .arg("asset-url")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI should start");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("CLI should finish");
    // 先取 stdout 再交出 output，避免返回元组时把已被移动的结构体再借用一次。
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (output, stdout)
}

#[test]
/// 全部接受的批量请求返回零状态，且逐条给出规范化 URL。
fn accepted_batch_succeeds_with_normalized_urls() {
    let request = serde_json::json!({ "urls": ACCEPTED });
    let (output, stdout) = run_cli(&request.to_string());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_str(&stdout).expect("stdout JSON");
    assert_eq!(payload["allAccepted"], true);
    let results = payload["results"].as_array().expect("results array");
    assert_eq!(results.len(), ACCEPTED.len());
    for (index, (entry, raw)) in results.iter().zip(ACCEPTED).enumerate() {
        assert_eq!(entry["index"], index);
        assert_eq!(entry["ok"], true, "{raw}");
        assert!(entry["normalized"].is_string(), "{raw}");
        // 成功条目必须携带解析器字段，市场侧据此免于二次解析。
        assert!(entry["scheme"].is_string(), "{raw}");
        assert!(entry["host"].is_string(), "{raw}");
        assert!(entry["port"].is_u64(), "{raw}");
    }
    // IDN 原文按共享契约规范化为 A-label，登记侧可直接比对 xn--2ba 等宿主已证实标签。
    let normalized: Vec<&str> = results
        .iter()
        .filter_map(|entry| entry["normalized"].as_str())
        .collect();
    assert!(
        normalized.iter().any(|url| url.contains("xn--2ba.tv")),
        "{normalized:?}"
    );
}

#[test]
/// 成功条目的 scheme/host/port 来自同一解析对象：正常域名、IDN、IPv6、显式与默认端口。
///
/// host 是解析器 `host_str()` 原样（IPv6 保留方括号），port 是有效端口
/// （显式优先，否则该协议的默认值），scheme 一律小写。
fn parsed_fields_cover_domains_idn_ipv6_and_ports() {
    let request = serde_json::json!({
        "urls": [
            "https://example.com/asset",
            "http://example.com:8443/asset",
            "https://例子.测试/asset",
            "https://[2001:db8::1]:8443/asset",
            "http://10.0.0.5/asset",
        ]
    });
    let (output, stdout) = run_cli(&request.to_string());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_str(&stdout).expect("stdout JSON");
    let results = payload["results"].as_array().expect("results array");

    // 正常域名 + 默认端口；scheme 由解析器小写化。
    assert_eq!(results[0]["scheme"], "https");
    assert_eq!(results[0]["host"], "example.com");
    assert_eq!(results[0]["port"], 443);
    // 显式端口优先于协议默认值。
    assert_eq!(results[1]["scheme"], "http");
    assert_eq!(results[1]["port"], 8443);
    // Unicode IDN 的 host 是解析器产出的 A-label 原样文本。
    assert_eq!(results[2]["host"], "xn--fsqu00a.xn--0zwm56d");
    assert_eq!(results[2]["port"], 443);
    // IPv6 的 host 保留方括号约定；port 是显式值。
    assert_eq!(results[3]["host"], "[2001:db8::1]");
    assert_eq!(results[3]["port"], 8443);
    assert_eq!(results[4]["scheme"], "http");
    assert_eq!(results[4]["host"], "10.0.0.5");
    assert_eq!(results[4]["port"], 80);
}

#[test]
/// 含拒绝条目的批量请求以非零状态失败，stdout 仍逐条给出可区分的错误分类。
fn rejected_batch_fails_with_distinguishable_reasons() {
    let request = serde_json::json!({ "urls": REJECTED });
    let (output, stdout) = run_cli(&request.to_string());
    assert!(!output.status.success(), "batch with rejections must fail");
    let payload: Value = serde_json::from_str(&stdout).expect("stdout JSON still emitted");
    assert_eq!(payload["allAccepted"], false);
    let results = payload["results"].as_array().expect("results array");
    assert_eq!(results.len(), REJECTED.len());
    for (entry, raw) in results.iter().zip(REJECTED) {
        assert_eq!(entry["ok"], false, "{raw}");
        let reason = entry["error"].as_str().expect("error text");
        assert!(!reason.is_empty(), "{raw}");
        // 错误文案是固定规则名，不能包含输入原文（前缀拼进错误也算泄露）；
        // 空样本的 contains("") 恒真没有判别力，只在非空输入上检查。
        if !raw.is_empty() {
            assert!(!reason.contains(*raw), "error must not contain the input");
        }
    }
    // 凭据类与片段类分别映射到不同固定文案，调用方可区分失败类别。
    let userinfo = results[2]["error"].as_str().unwrap();
    let fragment = results[9]["error"].as_str().unwrap();
    let zero_port = results[22]["error"].as_str().unwrap();
    assert_ne!(userinfo, fragment);
    assert_ne!(userinfo, zero_port);
    assert_ne!(fragment, zero_port);
}

#[test]
/// 带敏感 query 的被拒 URL 不得把令牌带入 stdout/stderr 任一输出。
///
/// 断言失败日志本身也只输出布尔结果，不打印输出内容，避免测试自身泄露标记秘密。
fn rejected_url_with_secret_query_does_not_leak_marker() {
    let marker = "leaked-secret-marker-3f9d";
    let secret_url = format!("https://user:pass@example.com/asset?token={marker}");
    let request = serde_json::json!({ "urls": [secret_url] });
    let (output, stdout) = run_cli(&request.to_string());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stdout.contains(marker), "stdout leaked the marker");
    assert!(!stderr.contains(marker), "stderr leaked the marker");
}

#[test]
/// locations 检查要求合法 base，并把相对与绝对 Location 用同一契约判定。
///
/// 重定向目标的成功条目输出与初始 URL 相同的解析器字段，相对 Location
/// 继承基准的 scheme/host，端口取基准的默认值。
fn locations_checks_use_base_and_share_contract() {
    let request = serde_json::json!({
        "base": "https://public.example:8443/asset",
        "locations": ["/next", "//cdn.example/asset", "//@host/path", "#fragment"]
    });
    let (output, stdout) = run_cli(&request.to_string());
    assert!(!output.status.success(), "batch with rejections must fail");
    let payload: Value = serde_json::from_str(&stdout).expect("stdout JSON");
    let results = payload["results"].as_array().expect("results array");
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[1]["ok"], true);
    assert_eq!(results[2]["ok"], false);
    assert_eq!(results[3]["ok"], false);
    // 相对 Location 继承基准 origin：scheme/host 与基准一致，端口取基准显式值。
    assert_eq!(results[0]["scheme"], "https");
    assert_eq!(results[0]["host"], "public.example");
    assert_eq!(results[0]["port"], 8443);
    assert_eq!(results[0]["normalized"], "https://public.example:8443/next");
    // 绝对重定向目标按自身 authority 输出字段。
    assert_eq!(results[1]["host"], "cdn.example");
    assert_eq!(results[1]["port"], 443);
}

#[test]
/// 非法 base、无效 JSON 与缺少字段都以明确错误失败，不能把无输入当成全部通过。
///
/// 显式 base 的校验先于必填字段检查：即使没有任何 urls/locations，
/// 坏基准也必须按共享契约给出明确失败，不被更早的结构错误掩盖。
fn invalid_requests_fail_explicitly() {
    for (request, marker) in [
        (
            r#"{"urls": [], "base": "http://@bad.example/"}"#,
            "base 不是合法",
        ),
        // 无任何条目字段的请求同样先校验 base，再报缺少 urls/locations。
        (r#"{"base": "http://example.com:0/"}"#, "base 不是合法"),
        (r#"{"locations": ["/next"]}"#, "base"),
        (r#"{"unknown": 1}"#, "请求 JSON 无效"),
        (r#"not json"#, "请求 JSON 无效"),
    ] {
        let (output, _) = run_cli(request);
        assert!(!output.status.success(), "{request}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(marker), "{request}: {stderr}");
    }
}

#[test]
/// 缺少 urls 与 locations 的请求独立失败，与 base 错误可区分。
fn missing_entry_fields_fail_explicitly() {
    let (output, _) = run_cli(r#"{}"#);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("必须包含 urls 或 locations"), "{stderr}");
    assert!(!stderr.contains("base 不是合法"), "{stderr}");
}

#[test]
/// 合法的无 locations base 不产生任何条目也不失败；请求契约允许只核对基准本身。
fn valid_base_without_locations_succeeds() {
    let (output, stdout) = run_cli(r#"{"urls": [], "base": "https://mirror.example:8443/a"}"#);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_str(&stdout).expect("stdout JSON");
    assert_eq!(payload["allAccepted"], true);
    assert_eq!(payload["results"].as_array().expect("results").len(), 0);
}

#[test]
/// JSON 解析错误只输出固定文案：未知字段名原文（可能是误写成键的带凭据 URL）绝不回显。
fn parse_errors_do_not_echo_sensitive_keys() {
    // 调用方把带令牌的 URL 误写成 JSON 键时，serde_json 的 unknown field 错误
    // 本会携带该键原文；这里断言 stderr 与 stdout 都不包含它。
    let secret_key = r#"{"https://example.com/asset?token=topsecret": 1}"#;
    let (output, stdout) = run_cli(secret_key);
    assert!(!output.status.success());
    let rendered = format!("{} {}", String::from_utf8_lossy(&output.stderr), stdout);
    assert!(!rendered.contains("topsecret"), "{rendered}");
    assert!(!rendered.contains("token="), "{rendered}");
    assert!(rendered.contains("请求 JSON 无效"), "{rendered}");
}

#[test]
/// asset-url 拒绝任何参数，保持纯计算入口不携带路径、凭据或副作用。
fn asset_url_rejects_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin"))
        .args(["asset-url", "--out", "x"])
        .output()
        .expect("CLI should start");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("不接受参数"), "{stderr}");
}
