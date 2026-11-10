//! @author kongweiguang

//! UI 仅接收受验字节的不可变投影；包签名、授权和启动代次仍由组合根负责。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kerminal_application::{AppError, AppResult};

use super::PluginWebViewDocument;

const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESOURCES: usize = 1024;

/// 内容类型由宿主投影，构造文档时再与路径白名单核对；页面不能改写共享字节。
#[derive(Clone, Debug)]
pub struct PluginWebViewResource {
    pub content_type: &'static str,
    pub bytes: Arc<[u8]>,
}

/// 私有字段保证通过校验后不会被替换入口或插入未校验资源，不提供文件系统回退。
pub struct PluginWebViewResourceDocument {
    pub(super) plugin_id: String,
    pub(super) instance_id: String,
    pub(super) entrypoint: String,
    pub(super) resources: BTreeMap<String, PluginWebViewResource>,
    pub(super) network_hosts: Vec<String>,
    pub(super) initialization_script: Option<String>,
}

impl PluginWebViewResourceDocument {
    /// 从启动校验后的包快照构造，网页资源和网络权限均不得回退读取磁盘。
    pub fn from_prepared(
        prepared: &kerminal_application::external_contributions::PreparedPlugin,
        view_id: &str,
        instance_id: String,
    ) -> AppResult<Self> {
        use kerminal_plugin_protocol::ViewDefinition;

        let Some(ViewDefinition::Webview { entry, csp, .. }) =
            prepared.package.manifest.views.get(view_id)
        else {
            return Err(AppError::invalid("插件贡献不是网页视图"));
        };
        let resources = prepared.resources.iter().filter_map(|(path, bytes)| {
            content_type(path).map(|content_type| {
                (
                    path.clone(),
                    PluginWebViewResource {
                        content_type,
                        bytes: bytes.clone(),
                    },
                )
            })
        });
        let mut document = Self::new(
            prepared.id.as_str().to_owned(),
            instance_id,
            entry.clone(),
            resources,
        )?;
        if let Some(csp) = csp.as_deref().filter(|csp| *csp != "default") {
            for origin in csp.split_whitespace() {
                let Some(host) = origin.strip_prefix("https://") else {
                    return Err(AppError::invalid("插件网页网络地址无效"));
                };
                if host.is_empty()
                    || !host
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
                    || !prepared
                        .package
                        .manifest
                        .permissions
                        .iter()
                        .any(|permission| permission.allows_network_host(host))
                {
                    return Err(AppError::invalid("插件网页网络地址未获授权"));
                }
                document.network_hosts.push(origin.to_owned());
            }
        }
        Ok(document)
    }

    /// 宿主生成的桥接只在页面入口初始化，插件资源不能替换实例身份。
    pub(crate) fn with_initialization_script(mut self, script: String) -> Self {
        self.initialization_script = Some(script);
        self
    }

    /// 消费冻结集合而非路径；重复、大小写别名和 URL 歧义在创建原生页面前统一拒绝。
    pub fn new(
        plugin_id: String,
        instance_id: String,
        entrypoint: String,
        resources: impl IntoIterator<Item = (String, PluginWebViewResource)>,
    ) -> AppResult<Self> {
        validate_plugin_id(&plugin_id)?;
        validate_identity(&instance_id)?;
        validate_resource_path(&entrypoint)?;
        let mut frozen = BTreeMap::new();
        let mut folded = BTreeSet::new();
        let mut total = 0usize;
        for (path, resource) in resources {
            validate_resource_path(&path)?;
            total = total
                .checked_add(resource.bytes.len())
                .ok_or_else(|| AppError::invalid("插件网页资源总量超限"))?;
            if resource.bytes.len() > MAX_RESOURCE_BYTES
                || total > MAX_TOTAL_BYTES
                || frozen.len() >= MAX_RESOURCES
                || !folded.insert(path.to_lowercase())
                || content_type(&path) != Some(resource.content_type)
            {
                return Err(AppError::invalid("插件网页资源类型、大小或路径集合无效"));
            }
            frozen.insert(path, resource);
        }
        if frozen
            .get(&entrypoint)
            .is_none_or(|resource| resource.content_type != "text/html; charset=utf-8")
        {
            return Err(AppError::invalid("插件网页入口不是可用 HTML 资源"));
        }
        Ok(Self {
            plugin_id,
            instance_id,
            entrypoint,
            resources: frozen,
            network_hosts: Vec::new(),
            initialization_script: None,
        })
    }
}

/// 兼容既有内联验收结构；调用者必须显式选择冻结资源文档，不能混用两种 CSP。
pub enum PluginWebViewSource {
    Inline(PluginWebViewDocument),
    Resources(PluginWebViewResourceDocument),
}

impl From<PluginWebViewDocument> for PluginWebViewSource {
    /// 原内联字面量继续可传给 create，不要求验收夹具添加生产包字段。
    fn from(document: PluginWebViewDocument) -> Self {
        Self::Inline(document)
    }
}

impl From<PluginWebViewResourceDocument> for PluginWebViewSource {
    /// 已校验文档按所有权传递，原生协议闭包随后独占其资源索引。
    fn from(document: PluginWebViewResourceDocument) -> Self {
        Self::Resources(document)
    }
}

impl PluginWebViewSource {
    /// 内联验收文档沿用原有行为，正式资源文档可携带宿主生成的桥接脚本。
    pub(super) fn initialization_script(&self) -> Option<&str> {
        match self {
            Self::Inline(_) => None,
            Self::Resources(document) => document.initialization_script.as_deref(),
        }
    }

    /// 事件身份取自同一输入，不允许资源入口另行指定桥接身份。
    pub(super) fn instance_id(&self) -> &str {
        match self {
            Self::Inline(document) => &document.instance_id,
            Self::Resources(document) => &document.instance_id,
        }
    }

    /// 冻结文档已经受私有构造约束；内联兼容输入仍须补齐原有身份和大小门禁。
    pub(super) fn validate(&self) -> AppResult<()> {
        validate_identity(self.instance_id())?;
        if matches!(self, Self::Inline(document) if document.html.len() > MAX_RESOURCE_BYTES) {
            return Err(AppError::invalid("插件 WebView 内联文档大小无效"));
        }
        Ok(())
    }
}

/// 实例标识只供宿主事件关联，不参与 URL 拼接，避免业务身份变成协议主机名。
fn validate_identity(instance_id: &str) -> AppResult<()> {
    if instance_id.trim().is_empty() || instance_id.len() > 256 {
        return Err(AppError::invalid("插件 WebView 文档身份无效"));
    }
    Ok(())
}

/// 资源路径需兼容所有受支持的平台，UTF-8 文件名则由协议层单独进行 URL 编码。
pub(super) fn validate_resource_path(path: &str) -> AppResult<()> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', ':', '\0']) {
        return Err(AppError::invalid("插件网页资源路径无效"));
    }
    for segment in path.split('/') {
        let stem = segment
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with(['.', ' '])
            || segment.chars().any(|character| {
                character.is_control() || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
            })
            || reserved
        {
            return Err(AppError::invalid("插件网页资源路径无效"));
        }
    }
    Ok(())
}

/// 协议主机名复用领域层的清单身份门禁，避免安装与资源加载对同一插件产生不同判断。
fn validate_plugin_id(plugin_id: &str) -> AppResult<()> {
    kerminal_domain::plugins::PluginId::parse(plugin_id)
        .map(|_| ())
        .map_err(|_| AppError::invalid("插件网页身份无效"))
}

/// 与宿主受验投影使用相同有限 MIME 集，不以嗅探或通用二进制回退扩大可执行资源面。
fn content_type(path: &str) -> Option<&'static str> {
    match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "html" | "htm" => Some("text/html; charset=utf-8"),
        "js" | "mjs" => Some("text/javascript; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        "json" => Some("application/json"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "svg" => Some("image/svg+xml"),
        "woff" => Some("font/woff"),
        "woff2" => Some("font/woff2"),
        "ttf" => Some("font/ttf"),
        "otf" => Some("font/otf"),
        _ => None,
    }
}
