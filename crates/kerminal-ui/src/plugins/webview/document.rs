//! @author kongweiguang

//! 每个原生实例仅从已冻结资源映射返回字节，不读取磁盘或网络。

use std::{
    borrow::Cow,
    collections::BTreeMap,
    sync::{Arc, mpsc::SyncSender},
};

use super::{MAX_BRIDGE_BYTES, PluginWebViewEvent, PluginWebViewResource, PluginWebViewSource};

const INLINE_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
// SDK业务只走有界IPC。禁止fetch/XHR先于Wry读取HTTP请求体生效，避免最终404之前已分配无界body。
const RESOURCE_CSP: &str = "default-src kplugin: 'self'; connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// 协议使用清单插件身份；Windows 的 HTTP 映射仅是 Wry 的本地协议别名。
pub(super) struct DocumentProtocol {
    origins: [String; 2],
    entrypoint: String,
    resources: BTreeMap<String, PluginWebViewResource>,
    csp: String,
}

impl DocumentProtocol {
    /// 包内入口路径保持原样，因此模块导入、CSS url() 和图片的相对目录语义一致。
    pub(super) fn new(document: PluginWebViewSource) -> Self {
        let (plugin_id, entrypoint, resources, csp) = match document {
            PluginWebViewSource::Inline(document) => (
                "probe.inline".to_owned(),
                "index.html".to_owned(),
                BTreeMap::from([(
                    "index.html".to_owned(),
                    PluginWebViewResource {
                        content_type: "text/html; charset=utf-8",
                        bytes: Arc::from(document.html.into_bytes()),
                    },
                )]),
                INLINE_CSP.to_owned(),
            ),
            PluginWebViewSource::Resources(document) => (
                document.plugin_id,
                document.entrypoint,
                document.resources,
                if document.network_hosts.is_empty() {
                    RESOURCE_CSP.to_owned()
                } else {
                    RESOURCE_CSP.replace(
                        "connect-src 'none'",
                        &format!("connect-src {}", document.network_hosts.join(" ")),
                    )
                },
            ),
        };
        Self {
            origins: [
                format!("kplugin://{plugin_id}"),
                format!("http://kplugin.{plugin_id}"),
            ],
            entrypoint,
            resources,
            csp,
        }
    }

    /// 入口使用清单中的插件身份；非 ASCII 路径按 UTF-8 字节编码后交给浏览器。
    pub(super) fn url(&self) -> String {
        format!(
            "{}/{}",
            self.origins[0],
            encode_resource_path(&self.entrypoint)
        )
    }

    /// 不先用 URL 库规范化；原始点段、反斜杠、编码和 authority 欺骗必须先被拒绝。
    fn resource_path(&self, url: &str) -> Option<String> {
        let path = self
            .origins
            .iter()
            .find_map(|origin| url.strip_prefix(origin)?.strip_prefix('/'))?;
        // 查询不影响冻结内容，允许常见构建器添加版本查询；片段不属于协议资源请求。
        if path.contains('#') {
            return None;
        }
        let path = path.split('?').next()?;
        let decoded = decode_resource_path(path)?;
        super::resources::validate_resource_path(&decoded).ok()?;
        if encode_resource_path(&decoded) != path {
            return None;
        }
        Some(decoded)
    }

    /// 只有顶层固定入口可导航或使用桥接；同源其他 HTML 也不能获得入口身份。
    pub(super) fn permits_document(&self, url: &str) -> bool {
        let url = url.split('#').next().unwrap_or_default();
        !url.contains('?') && self.resource_path(url).as_deref() == Some(self.entrypoint.as_str())
    }

    /// 方法和来源同时校验，缺失资源返回空 404；CSP/nosniff 对失败响应同样生效。
    pub(super) fn response(
        &self,
        request: &wry::http::Request<Vec<u8>>,
    ) -> wry::http::Response<Cow<'static, [u8]>> {
        let uri = request.uri().to_string();
        let same_origin = request.headers().get("Origin").is_none_or(|origin| {
            origin
                .to_str()
                .is_ok_and(|origin| self.origins.iter().any(|allowed| allowed == origin))
        });
        let resource = (request.method() == wry::http::Method::GET && same_origin)
            .then(|| {
                self.resource_path(&uri)
                    .and_then(|path| self.resources.get(&path))
            })
            .flatten();
        let mut response = wry::http::Response::new(Cow::Owned(
            resource.map_or_else(Vec::new, |resource| resource.bytes.to_vec()),
        ));
        *response.status_mut() = if resource.is_some() {
            wry::http::StatusCode::OK
        } else {
            wry::http::StatusCode::NOT_FOUND
        };
        response.headers_mut().insert(
            "Content-Type",
            wry::http::HeaderValue::from_static(
                resource.map_or("text/plain; charset=utf-8", |resource| {
                    resource.content_type
                }),
            ),
        );
        response.headers_mut().insert(
            "Content-Security-Policy",
            wry::http::HeaderValue::from_str(&self.csp)
                .unwrap_or_else(|_| wry::http::HeaderValue::from_static(RESOURCE_CSP)),
        );
        response.headers_mut().insert(
            "X-Content-Type-Options",
            wry::http::HeaderValue::from_static("nosniff"),
        );
        response.headers_mut().insert(
            "Cache-Control",
            wry::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}

/// 导航、加载完成与验收消息共用入口校验；正式权限路由由后续插件运行时提供。
pub(super) fn builder<'a>(
    document: PluginWebViewSource,
    sender: SyncSender<PluginWebViewEvent>,
) -> wry::WebViewBuilder<'a> {
    let initialization_script = document.initialization_script().map(str::to_owned);
    let message_id = document.instance_id().to_owned();
    let loaded_id = message_id.clone();
    let navigation_id = message_id.clone();
    let new_window_id = message_id.clone();
    let navigation_sender = sender.clone();
    let new_window_sender = sender.clone();
    let protocol = Arc::new(DocumentProtocol::new(document));
    let navigation = protocol.clone();
    let ipc_protocol = protocol.clone();
    let loaded_protocol = protocol.clone();
    let loaded_sender = sender.clone();
    let builder = wry::WebViewBuilder::new();
    let builder = if let Some(script) = initialization_script {
        builder.with_initialization_script(script)
    } else {
        builder
    };
    builder
        .with_url(protocol.url())
        .with_visible(false)
        .with_focused(false)
        .with_incognito(true)
        .with_devtools(false)
        .with_clipboard(false)
        .with_general_autofill_enabled(false)
        .with_permission_handler(|_| wry::PermissionResponse::Deny)
        .with_new_window_req_handler(move |url, _| {
            enqueue_external_navigation(&new_window_sender, &new_window_id, &url);
            wry::NewWindowResponse::Deny
        })
        .with_download_started_handler(|_, _| false)
        .with_navigation_handler(move |url| {
            if navigation.permits_document(&url) {
                true
            } else {
                enqueue_external_navigation(&navigation_sender, &navigation_id, &url);
                false
            }
        })
        .with_custom_protocol("kplugin".into(), move |_, request| {
            protocol.response(&request)
        })
        .with_ipc_handler(move |request| {
            if request.body().len() <= MAX_BRIDGE_BYTES
                && ipc_protocol.permits_document(&request.uri().to_string())
            {
                let _ = sender.try_send(PluginWebViewEvent::Message {
                    instance_id: message_id.clone(),
                    body: request.into_body(),
                });
            }
        })
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, wry::PageLoadEvent::Finished)
                && loaded_protocol.permits_document(&url)
            {
                let _ = loaded_sender.try_send(PluginWebViewEvent::Loaded {
                    instance_id: loaded_id.clone(),
                });
            }
        })
}

/// 队列仅传递有界 HTTPS 候选；是否授予打开权限仍由实例绑定的宿主路由决定。
pub(super) fn enqueue_external_navigation(
    sender: &SyncSender<PluginWebViewEvent>,
    instance_id: &str,
    url: &str,
) {
    if url.len() <= 4096 && url.starts_with("https://") && !url.chars().any(char::is_control) {
        let _ = sender.try_send(PluginWebViewEvent::ExternalNavigation {
            instance_id: instance_id.to_owned(),
            url: url.to_owned(),
        });
    }
}

/// 保留目录分隔符，其余非安全字节逐字节编码，避免 UTF-8 名称被误认为查询或片段。
fn encode_resource_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            result.push(char::from(byte));
        } else {
            result.push('%');
            result.push(char::from(HEX[usize::from(byte >> 4)]));
            result.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    result
}

/// 只解码一次且拒绝编码的分隔符，路径仍须通过冻结资源集合的约束。
fn decode_resource_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = if bytes[index] == b'%' {
            let high = char::from(*bytes.get(index + 1)?).to_digit(16)?;
            let low = char::from(*bytes.get(index + 2)?).to_digit(16)?;
            index += 3;
            let byte = ((high << 4) | low) as u8;
            if matches!(byte, b'/' | b'\\' | 0) {
                return None;
            }
            byte
        } else {
            let byte = bytes[index];
            index += 1;
            byte
        };
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}
