// @author kongweiguang

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rquickjs::{Ctx, Exception, TypedArray};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha384, Sha512};
use url::Url;

const MAX_BINARY_BYTES: usize = 4 * 1024 * 1024;

/// 在不重新进入 JS 的短暂借用期间读取视图，原生编码避免大文件耗尽执行时间。
pub(crate) fn encode_binary<'js>(
    ctx: Ctx<'js>,
    value: TypedArray<'js, u8>,
) -> rquickjs::Result<String> {
    // 安全性：借用期间没有任何 JS 调用，编码完成后才释放该引用。
    let data = unsafe { value.as_bytes() }
        .ok_or_else(|| Exception::throw_message(&ctx, "detached binary data"))?;
    if data.len() > MAX_BINARY_BYTES {
        return Err(Exception::throw_message(&ctx, "binary data exceeds 4 MiB"));
    }
    Ok(STANDARD.encode(data))
}

/// 创建由 QuickJS 分配的独立数组，让解码结果继续受运行时堆上限约束。
pub(crate) fn decode_binary<'js>(
    ctx: Ctx<'js>,
    text: String,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    if text.len() > MAX_BINARY_BYTES.div_ceil(3) * 4 {
        return Err(Exception::throw_message(&ctx, "binary data exceeds 4 MiB"));
    }
    let bytes = STANDARD
        .decode(text)
        .map_err(|_| Exception::throw_message(&ctx, "invalid base64 data"))?;
    if bytes.len() > MAX_BINARY_BYTES {
        return Err(Exception::throw_message(&ctx, "binary data exceeds 4 MiB"));
    }
    TypedArray::new_copy(ctx, bytes)
}

/// URL 和摘要使用锁定的原生实现，脚本侧不复制安全敏感的解析算法。
pub(crate) fn call(text: &str) -> Result<String, String> {
    if text.len() > 6 * 1024 * 1024 {
        return Err("web operation exceeds size limit".into());
    }
    let request: Value = serde_json::from_str(text).map_err(|_| "invalid web arguments")?;
    let result = match string(&request, "op")? {
        "uuid" => json!(uuid::Uuid::new_v4().to_string()),
        "digest" => {
            let bytes = bytes(&request)?;
            let digest = match string(&request, "algorithm")?.to_ascii_uppercase().as_str() {
                "SHA-256" => Sha256::digest(&bytes).to_vec(),
                "SHA-384" => Sha384::digest(&bytes).to_vec(),
                "SHA-512" => Sha512::digest(&bytes).to_vec(),
                _ => return Err("unsupported digest algorithm".into()),
            };
            json!(STANDARD.encode(digest))
        }
        "decodeUtf8" => {
            let bytes = bytes(&request)?;
            if request
                .get("fatal")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                json!(String::from_utf8(bytes).map_err(|_| "invalid UTF-8")?)
            } else {
                json!(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
        "url.parse" => {
            let input = string(&request, "input")?;
            let url = match request.get("base").and_then(Value::as_str) {
                Some(base) => Url::parse(base).and_then(|url| url.join(input)),
                None => Url::parse(input),
            }
            .map_err(|error| format!("invalid URL: {error}"))?;
            parts(&url)
        }
        "url.set" => {
            let mut url = Url::parse(string(&request, "href")?).map_err(|_| "invalid URL")?;
            let field = string(&request, "field")?;
            let value = string(&request, "value")?;
            match field {
                "href" => url = Url::parse(value).map_err(|_| "invalid URL")?,
                "protocol" => url
                    .set_scheme(value.trim_end_matches(':'))
                    .map_err(|_| "invalid URL scheme")?,
                "username" => url
                    .set_username(value)
                    .map_err(|_| "invalid URL username")?,
                "password" => url
                    .set_password(if value.is_empty() { None } else { Some(value) })
                    .map_err(|_| "invalid URL password")?,
                "hostname" => url
                    .set_host(Some(value))
                    .map_err(|_| "invalid URL hostname")?,
                "port" => {
                    let port = if value.is_empty() {
                        None
                    } else {
                        Some(value.parse::<u16>().map_err(|_| "invalid URL port")?)
                    };
                    url.set_port(port).map_err(|_| "invalid URL port")?;
                }
                "host" => {
                    let host =
                        Url::parse(&format!("http://{value}/")).map_err(|_| "invalid URL host")?;
                    if !host.username().is_empty() || host.password().is_some() {
                        return Err("invalid URL host".into());
                    }
                    url.set_host(host.host_str())
                        .map_err(|_| "invalid URL host")?;
                    url.set_port(host.port()).map_err(|_| "invalid URL port")?;
                }
                "pathname" => url.set_path(value),
                "search" => url.set_query(if value.is_empty() {
                    None
                } else {
                    Some(value.strip_prefix('?').unwrap_or(value))
                }),
                "hash" => url.set_fragment(if value.is_empty() {
                    None
                } else {
                    Some(value.strip_prefix('#').unwrap_or(value))
                }),
                _ => return Err("unknown URL property".into()),
            }
            parts(&url)
        }
        "query.parse" => {
            let query = string(&request, "query")?;
            json!(
                url::form_urlencoded::parse(query.strip_prefix('?').unwrap_or(query).as_bytes())
                    .into_owned()
                    .collect::<Vec<_>>()
            )
        }
        "query.format" => {
            let pairs = request
                .get("pairs")
                .and_then(Value::as_array)
                .ok_or("invalid URL parameters")?;
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for pair in pairs {
                let pair = pair
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or("invalid URL parameter")?;
                serializer.append_pair(
                    pair[0].as_str().ok_or("invalid URL parameter")?,
                    pair[1].as_str().ok_or("invalid URL parameter")?,
                );
            }
            json!(serializer.finish())
        }
        _ => return Err("unknown web operation".into()),
    };
    serde_json::to_string(&result).map_err(|error| error.to_string())
}

/// 二进制数据按统一上限解码，避免摘要或字符串转换绕过内存预算。
fn bytes(request: &Value) -> Result<Vec<u8>, String> {
    let bytes = STANDARD
        .decode(string(request, "data")?)
        .map_err(|_| "invalid binary data")?;
    if bytes.len() > MAX_BINARY_BYTES {
        return Err("binary data exceeds 4 MiB".into());
    }
    Ok(bytes)
}

/// 全部 URL 属性来自同一已解析对象，避免 getter 与规范化结果不一致。
fn parts(url: &Url) -> Value {
    let hostname = url.host_str().unwrap_or("");
    let host = match url.port() {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname.to_owned(),
    };
    json!({"href":url.as_str(),"origin":url.origin().ascii_serialization(),"protocol":format!("{}:",url.scheme()),
        "username":url.username(),"password":url.password().unwrap_or(""),"host":host,"hostname":hostname,
        "port":url.port().map(|port| port.to_string()).unwrap_or_default(),"pathname":url.path(),
        "search":url.query().filter(|query| !query.is_empty()).map(|query| format!("?{query}")).unwrap_or_default(),
        "hash":url.fragment().filter(|fragment| !fragment.is_empty()).map(|fragment| format!("#{fragment}")).unwrap_or_default()})
}

/// 原生边界拒绝缺失或非字符串参数，保持 URL 输入校验可预测。
fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {name}"))
}
