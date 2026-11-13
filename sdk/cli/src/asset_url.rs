//! `asset-url` 子命令的纯计算实现：批量校验资产 URL 与重定向 Location 原文。
//!
//! 与安装器网络边界共用协议层同一入口，市场登记侧可离线调用；本模块不执行任何
//! 网络请求，不读取凭据，也不在输出中回显被拒输入的 query 或 userinfo。成功条目
//! 携带解析器字段供登记侧复用，避免市场再用 Python 近似解析已规范化 URL。
//!
//! @author kongweiguang

use std::io::{self, Read};

use serde::Deserialize;
use serde_json::json;

use kerminal_plugin_protocol::{AssetUrlRejection, validate_asset_url, validate_redirect_target};

/// 批量校验 stdin 上的 JSON 请求并输出 JSON 结果；输入无效应整体失败，不留部分成功。
///
/// 请求契约：`{"urls": ["https://...", ...]}`，或带基准的 `{"base": "...", "locations": [...]}`
/// 用于重定向 Location 检查；两类字段可以在同一请求中出现。显式提供的 `base`
/// 无论是否存在 `locations` 都先按共享契约校验，避免坏基准被静默忽略。
/// 每个条目返回 `{"index", "ok"}`：失败附 `error` 固定分类文案；成功附
/// `normalized`（序列化 URL，可能包含调用方自己的 query，不能视为无敏感信息）、
/// `scheme`（解析器小写协议）、`host`（解析器 `host_str()` 原样，IPv6 带方括号）、
/// `port`（`port_or_known_default()` 的有效端口整数，显式或默认值）。解析器把含
/// 无效条目的请求按非零状态拒绝，避免批量脚本把失败混成成功。
pub fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    // serde_json 的 unknown field / type 错误会回显字段名原文；这里整体丢弃错误细节，
    // 只保留固定分类文案，防止调用方把带凭据的 URL 误写成 JSON 键后泄露到 stderr。
    let request: AssetUrlRequest =
        serde_json::from_str(&input).map_err(|_| "asset-url 请求 JSON 无效".to_string())?;

    // 基准在必填字段检查之前统一校验：只要显式给出 base，无论是否带 locations，
    // 坏基准都必须得到明确失败，而不是被"缺少 urls/locations"这类更早的错误掩盖。
    let base = match request.base.as_deref() {
        Some(base) => Some(
            validate_asset_url(base)
                .map_err(|reason| format!("base 不是合法资产地址（{}）", reason.describe()))?,
        ),
        None => None,
    };

    if request.urls.is_none() && request.locations.is_none() {
        return Err("asset-url 请求必须包含 urls 或 locations".into());
    }

    let mut results = Vec::new();
    let mut rejected = false;
    if let Some(urls) = &request.urls {
        for (index, value) in urls.iter().enumerate() {
            let outcome = classify(validate_asset_url(value), |u| {
                (
                    u.scheme().to_owned(),
                    u.host_str().map(str::to_owned),
                    u.port_or_known_default(),
                )
            });
            rejected |= outcome.is_err();
            results.push(entry(index, outcome)?);
        }
    }
    if let Some(locations) = &request.locations {
        let current = base.as_ref().ok_or("locations 检查必须提供 base")?;
        for (index, value) in locations.iter().enumerate() {
            let outcome = classify(validate_redirect_target(current, value), |u| {
                (
                    u.scheme().to_owned(),
                    u.host_str().map(str::to_owned),
                    u.port_or_known_default(),
                )
            });
            rejected |= outcome.is_err();
            results.push(entry(index, outcome)?);
        }
    }

    let payload = json!({ "results": results, "allAccepted": !rejected });
    println!("{payload}");
    if rejected {
        // 非零状态让批量调用方明确感知存在拒绝项；stdout 仍保留逐条结果供排查。
        return Err("asset-url 存在被拒绝的条目".into());
    }
    Ok(())
}

/// 成功条目的解析器字段快照；全部来自协议返回的同一解析对象，不做二次解析。
struct ParsedFields {
    normalized: String,
    scheme: String,
    host: Option<String>,
    port: Option<u16>,
}

/// 把校验产物收敛成稳定字段；闭包在调用点按具体类型提取，CLI 不直接依赖 url
/// crate，避免在清单中复制宿主的锁定版本。失败只返回固定分类文案，不回显输入。
///
/// `T: Display` 约束支撑 `normalized` 的序列化输出；提取闭包与序列化共用同一
/// 解析对象引用，不产生二次解析。
fn classify<T: std::fmt::Display>(
    result: Result<T, AssetUrlRejection>,
    extract: impl Fn(&T) -> (String, Option<String>, Option<u16>),
) -> Result<ParsedFields, &'static str> {
    match result {
        Ok(url) => {
            let (scheme, host, port) = extract(&url);
            Ok(ParsedFields {
                normalized: url.to_string(),
                scheme,
                host,
                port,
            })
        }
        Err(reason) => Err(reason.describe()),
    }
}

/// 组装单条结果；错误字段只在失败时出现，成功条目携带规范化 URL 与解析器字段。
///
/// 不接收输入原文：条目级输出永不回显被拒 URL，拒绝原因只来自固定分类文案。
/// 主机与端口的缺失在契约上不可能发生（校验入口已保证），但仍失败关闭：
/// 让整个命令以固定文案退出，而不是编造字段或 unwrap。
fn entry(
    index: usize,
    outcome: Result<ParsedFields, &'static str>,
) -> Result<serde_json::Value, String> {
    match outcome {
        Ok(fields) => {
            let host = fields
                .host
                .ok_or_else(|| "asset-url 解析结果缺少主机".to_string())?;
            let port = fields
                .port
                .ok_or_else(|| "asset-url 解析结果缺少端口".to_string())?;
            Ok(json!({
                "index": index,
                "ok": true,
                "normalized": fields.normalized,
                "scheme": fields.scheme,
                "host": host,
                "port": port,
            }))
        }
        Err(reason) => Ok(json!({ "index": index, "ok": false, "error": reason })),
    }
}

/// stdin 请求的骨架；未知字段拒绝，避免调用方把误拼字段当成已生效。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetUrlRequest {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    urls: Option<Vec<String>>,
    #[serde(default)]
    locations: Option<Vec<String>>,
}
