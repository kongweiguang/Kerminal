// @author kongweiguang

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kerminal_plugin_protocol::Permission;
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue,
    LOCATION, PROXY_AUTHORIZATION,
};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use url::Url;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;

/// 禁止自动重定向和环境代理，确保每次网络目标都经过显式授权检查。
pub(crate) fn client() -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())
}

/// 每一跳在连接前检查主机权限，跨来源跳转移除敏感请求头。
pub(crate) async fn fetch(
    client: Client,
    permissions: Vec<Permission>,
    request: Value,
) -> Result<Value, String> {
    let mut url =
        Url::parse(string(&request, "url")?).map_err(|error| format!("invalid URL: {error}"))?;
    let mut method = Method::from_bytes(string(&request, "method")?.as_bytes())
        .map_err(|_| "invalid HTTP method")?;
    if [Method::CONNECT, Method::TRACE].contains(&method) || method.as_str() == "TRACK" {
        return Err("forbidden HTTP method".into());
    }
    let mut headers = parse_headers(request.get("headers"))?;
    let mut body = match request.get("body").and_then(Value::as_str) {
        Some(encoded) => Some(
            STANDARD
                .decode(encoded)
                .map_err(|_| "invalid request body")?,
        ),
        None => None,
    };
    if body
        .as_ref()
        .is_some_and(|data| data.len() > MAX_BODY_BYTES)
    {
        return Err("request body exceeds 4 MiB".into());
    }
    if body.is_some() && (method == Method::GET || method == Method::HEAD) {
        return Err("GET and HEAD cannot have a body".into());
    }
    let redirect = request
        .get("redirect")
        .and_then(Value::as_str)
        .unwrap_or("follow");
    if !["follow", "manual", "error"].contains(&redirect) {
        return Err("invalid redirect mode".into());
    }
    let mut redirected = false;
    for hop in 0..=MAX_REDIRECTS {
        check_url(&url, &permissions)?;
        url.set_fragment(None);
        let mut builder = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        if let Some(data) = &body {
            builder = builder.body(data.clone());
        }
        let mut response = builder
            .send()
            .await
            .map_err(|error| format!("network request failed: {error}"))?;
        let status = response.status();
        if is_redirect(status) && response.headers().contains_key(LOCATION) {
            if redirect == "error" {
                return Err("redirect is not allowed".into());
            }
            if redirect == "follow" {
                if hop == MAX_REDIRECTS {
                    return Err("too many redirects".into());
                }
                let location = response.headers()[LOCATION]
                    .to_str()
                    .map_err(|_| "invalid redirect location")?;
                let next = url
                    .join(location)
                    .map_err(|_| "invalid redirect location")?;
                // 即使不发送请求也先校验新地址，禁止重定向绕过域名白名单。
                check_url(&next, &permissions)?;
                if url.origin() != next.origin() {
                    headers.remove(AUTHORIZATION);
                    headers.remove(PROXY_AUTHORIZATION);
                    headers.remove(COOKIE);
                }
                if (status == StatusCode::SEE_OTHER
                    && method != Method::GET
                    && method != Method::HEAD)
                    || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
                        && method == Method::POST)
                {
                    method = Method::GET;
                    body = None;
                    headers.remove(CONTENT_TYPE);
                    headers.remove(CONTENT_LENGTH);
                }
                url = next;
                redirected = true;
                continue;
            }
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err("response body exceeds 4 MiB".into());
        }
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("response read failed: {error}"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err("response body exceeds 4 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(
            json!({"status":status.as_u16(),"statusText":status.canonical_reason().unwrap_or(""),
            "url":url.as_str(),"redirected":redirected,"headers":response_headers,"body":STANDARD.encode(bytes)}),
        );
    }
    Err("too many redirects".into())
}

/// 只允许 HTTP(S) 且逐一匹配声明的 DNS 主机，不接受 URL 内嵌凭据。
fn check_url(url: &Url, permissions: &[Permission]) -> Result<(), String> {
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("unsupported or credentialed network URL".into());
    }
    let host = url.host_str().ok_or("network URL has no host")?;
    if !permissions
        .iter()
        .any(|permission| permission.allows_network_host(host))
    {
        return Err("network permission denied".into());
    }
    Ok(())
}

/// 将脚本提供的头部逐项校验，禁止伪造连接层字段。
fn parse_headers(value: Option<&Value>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let Some(items) = value.and_then(Value::as_array) else {
        return Ok(headers);
    };
    if items.len() > 128 {
        return Err("too many request headers".into());
    }
    for pair in items {
        let pair = pair
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or("invalid request header")?;
        let name =
            HeaderName::from_bytes(pair[0].as_str().ok_or("invalid header name")?.as_bytes())
                .map_err(|_| "invalid header name")?;
        if [
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "proxy-authorization",
        ]
        .contains(&name.as_str())
        {
            return Err("forbidden request header".into());
        }
        let value = HeaderValue::from_str(pair[1].as_str().ok_or("invalid header value")?)
            .map_err(|_| "invalid header value")?;
        headers.append(name, value);
    }
    Ok(headers)
}

/// 只有有定义的跳转状态会产生下一次连接。
fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// 网络参数必须保持显式字符串类型，不能由宽松转换改变目标。
fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {name}"))
}
