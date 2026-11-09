//! 声明式清单、平台与权限；安装、启动和开发工具共用同一入口。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{ProtocolError, invalid, parse_json, validate_extensions, validate_package_path};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MANIFEST_FILE_NAME: &str = "kerminal-plugin.json";

/// 命名空间信任由调用方根据经过验证的来源决定，而不是相信清单自报。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ManifestPolicy {
    #[default]
    ThirdParty,
    Official,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginManifest {
    pub schema: u32,
    pub id: String,
    pub name: BTreeMap<String, String>,
    pub description: BTreeMap<String, String>,
    pub version: String,
    pub api: String,
    pub publisher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub runtime: PluginRuntime,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub contributes: Contributions,
    #[serde(default)]
    pub views: BTreeMap<String, ViewDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l10n: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// 兼容宿主使用外部插件这一领域名称，同时只维护一种清单模型。
pub type ExternalPluginManifest = PluginManifest;

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
pub enum Platform {
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
    #[serde(rename = "windows-aarch64")]
    WindowsAarch64,
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    #[serde(rename = "macos-x86_64")]
    MacosX86_64,
    #[serde(rename = "macos-aarch64")]
    MacosAarch64,
}

impl Platform {
    /// 不支持的平台返回空值，不能回退到其他架构的可执行文件。
    pub fn current() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", "x86_64") => Some(Self::WindowsX86_64),
            ("windows", "aarch64") => Some(Self::WindowsAarch64),
            ("linux", "x86_64") => Some(Self::LinuxX86_64),
            ("linux", "aarch64") => Some(Self::LinuxAarch64),
            ("macos", "x86_64") => Some(Self::MacosX86_64),
            ("macos", "aarch64") => Some(Self::MacosAarch64),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Permission {
    UiNotify,
    UiDialog,
    UiFilePicker,
    ClipboardRead,
    ClipboardWrite,
    OpenExternal,
    Network(String),
    HostsRead,
    TerminalWrite,
    EventsSubscribe,
    Secrets,
    FullAccess,
}

impl Permission {
    /// 通配符仅匹配真正的子域，不能扩大到根域或相似后缀。
    pub fn allows_network_host(&self, host: &str) -> bool {
        let Self::Network(pattern) = self else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let pattern = pattern.to_ascii_lowercase();
        if let Some(suffix) = pattern.strip_prefix("*.") {
            host.strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
        } else {
            host == pattern
        }
    }
}

impl fmt::Display for Permission {
    /// 权限始终编码为契约中的短字符串，避免 Rust 枚举表示泄漏到 JSON。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::UiNotify => "ui.notify",
            Self::UiDialog => "ui.dialog",
            Self::UiFilePicker => "ui.file-picker",
            Self::ClipboardRead => "clipboard.read",
            Self::ClipboardWrite => "clipboard.write",
            Self::OpenExternal => "open-external",
            Self::HostsRead => "hosts.read",
            Self::TerminalWrite => "terminal.write",
            Self::EventsSubscribe => "events.subscribe",
            Self::Secrets => "secrets",
            Self::FullAccess => "full-access",
            Self::Network(host) => return write!(formatter, "network:{host}"),
        };
        formatter.write_str(value)
    }
}

impl FromStr for Permission {
    type Err = ProtocolError;

    /// 不接受全域通配、端口或 URL，确保网络授权只作用于明确主机。
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let permission = match value {
            "ui.notify" => Self::UiNotify,
            "ui.dialog" => Self::UiDialog,
            "ui.file-picker" => Self::UiFilePicker,
            "clipboard.read" => Self::ClipboardRead,
            "clipboard.write" => Self::ClipboardWrite,
            "open-external" => Self::OpenExternal,
            "hosts.read" => Self::HostsRead,
            "terminal.write" => Self::TerminalWrite,
            "events.subscribe" => Self::EventsSubscribe,
            "secrets" => Self::Secrets,
            "full-access" => Self::FullAccess,
            _ if value.starts_with("network:") => {
                let host = &value["network:".len()..];
                if !valid_network_pattern(host) {
                    return Err(invalid("permissions", "无效的网络主机白名单"));
                }
                Self::Network(host.to_ascii_lowercase())
            }
            _ => return Err(invalid("permissions", "未知权限")),
        };
        Ok(permission)
    }
}

impl Serialize for Permission {
    /// 字符串表示与其他语言 SDK 的权限声明保持一致。
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Permission {
    /// 在反序列化边界即拒绝未知权限，防止授权列表被静默缩减。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Permission {
    /// Schema 同时描述固定权限与受约束的网络权限字符串。
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Permission".into()
    }
    /// 由类型提供 schema，编辑器不需要维护另一份权限清单。
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        let label = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?";
        let host = format!(r"{label}(?:\.{label})*");
        schemars::json_schema!({"type":"string", "anyOf":[
            {"enum":["ui.notify","ui.dialog","ui.file-picker","clipboard.read","clipboard.write","open-external","hosts.read","terminal.write","events.subscribe","secrets","full-access"]},
            {"pattern":format!(r"^network:{host}$"), "maxLength":261,
             "not":{"pattern":r"[^A-Za-z0-9:.-]"}},
            {"pattern":format!(r"^network:\*\.{host}$"), "maxLength":263,
             "not":{"pattern":r"[^A-Za-z0-9:.*-]"}}
        ]})
    }
}

/// DNS 标签遵循常见主机写法，不允许把通配符、路径或凭据混入权限。
fn valid_network_pattern(pattern: &str) -> bool {
    let host = pattern.strip_prefix("*.").unwrap_or(pattern);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    #[default]
    OnDemand,
    Startup,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PluginRuntime {
    None {
        #[serde(flatten)]
        extensions: BTreeMap<String, Value>,
    },
    Script {
        main: String,
        #[serde(default)]
        activation: Activation,
        #[serde(flatten)]
        extensions: BTreeMap<String, Value>,
    },
    Sidecar {
        main: BTreeMap<Platform, String>,
        #[serde(default)]
        activation: Activation,
        #[serde(flatten)]
        extensions: BTreeMap<String, Value>,
    },
}

impl PluginRuntime {
    /// Sidecar 的权限由执行形态决定，不能伪装成受限脚本。
    pub fn is_sidecar(&self) -> bool {
        matches!(self, Self::Sidecar { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ViewDefinition {
    Native {
        file: String,
        #[serde(flatten)]
        extensions: BTreeMap<String, Value>,
    },
    Webview {
        entry: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        csp: Option<String>,
        #[serde(flatten)]
        extensions: BTreeMap<String, Value>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Contributions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<TabContribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<SettingsContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_types: Vec<HostTypeContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandContribution>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ToolScope {
    Window,
    Tab,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolContribution {
    pub id: String,
    pub title: String,
    pub view: String,
    pub scope: ToolScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TabContribution {
    pub id: String,
    pub title: String,
    pub view: String,
    pub state_version: u32,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SettingsContribution {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HostTypeContribution {
    pub id: String,
    pub title: String,
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandContribution {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybinding: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// 宿主、安装器和开发工具共享此入口，不能各自放宽同一清单的规则。
pub fn parse_manifest(
    bytes: &[u8],
    policy: ManifestPolicy,
) -> Result<PluginManifest, ProtocolError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("manifest", "清单超过大小上限"));
    }
    let value = parse_json(bytes, "manifest")?;
    let manifest: PluginManifest =
        serde_json::from_value(value).map_err(|source| ProtocolError::Json {
            location: "manifest".into(),
            source,
        })?;
    manifest.validate(policy)?;
    Ok(manifest)
}

impl PluginManifest {
    /// Sidecar 的完全访问由运行形态隐含，声明列表无需重复写入。
    pub fn effective_permissions(&self) -> BTreeSet<Permission> {
        let mut permissions = self.permissions.iter().cloned().collect::<BTreeSet<_>>();
        if self.runtime.is_sidecar() {
            permissions.insert(Permission::FullAccess);
        }
        permissions
    }

    /// npm 风格范围由 node-semver 计算，避免把 Cargo 的版本范围当成 npm 语义。
    pub fn supports_host_api(&self, host_api: &str) -> Result<bool, ProtocolError> {
        let range = node_semver::Range::parse(&self.api)
            .map_err(|_| invalid("manifest.api", "无效的 npm 版本范围"))?;
        let version = node_semver::Version::parse(host_api)
            .map_err(|_| invalid("host_api", "无效的宿主 API 版本"))?;
        Ok(range.satisfies(&version))
    }

    /// 安装前检查身份、引用与运行形态，文件内容再由同一个包校验入口检查。
    pub fn validate(&self, policy: ManifestPolicy) -> Result<(), ProtocolError> {
        validate_extensions(&self.extensions, "manifest")?;
        if self.schema != MANIFEST_SCHEMA_VERSION {
            return Err(invalid("manifest.schema", "不支持的清单版本"));
        }
        let (publisher, name) = self
            .id
            .split_once('.')
            .ok_or_else(|| invalid("manifest.id", "插件 ID 必须为 publisher.name"))?;
        if !valid_id_segment(publisher, 39)
            || !valid_id_segment(name, 63)
            || self.publisher != publisher
        {
            return Err(invalid("manifest.id", "插件 ID 或发布者无效"));
        }
        if publisher == "kerminal" && policy != ManifestPolicy::Official {
            return Err(invalid("manifest.id", "官方命名空间只允许可信官方插件使用"));
        }
        semver::Version::parse(&self.version)
            .map_err(|_| invalid("manifest.version", "无效的 SemVer 版本"))?;
        if self.api.len() > 256 || node_semver::Range::parse(&self.api).is_err() {
            return Err(invalid("manifest.api", "无效的 npm 版本范围"));
        }
        validate_localized(&self.name, "manifest.name")?;
        validate_localized(&self.description, "manifest.description")?;
        if self
            .license
            .as_ref()
            .is_some_and(|value| !valid_text(value, 256))
            || self
                .repository
                .as_ref()
                .is_some_and(|value| !value.starts_with("https://") || !valid_text(value, 2048))
            || self.categories.iter().any(|value| !valid_local_id(value))
        {
            return Err(invalid("manifest", "许可、仓库或分类无效"));
        }
        if let Some(icon) = &self.icon {
            validate_package_path(icon)?;
        }
        if let Some(l10n) = &self.l10n {
            validate_package_path(l10n)?;
        }

        let mut permissions = BTreeSet::new();
        for permission in &self.permissions {
            if !permissions.insert(permission) {
                return Err(invalid("manifest.permissions", "权限重复"));
            }
            if *permission == Permission::FullAccess && !self.runtime.is_sidecar() {
                return Err(invalid(
                    "manifest.permissions",
                    "只有 Sidecar 可以拥有完全访问",
                ));
            }
        }
        match &self.runtime {
            PluginRuntime::None { extensions } => validate_extensions(extensions, "runtime")?,
            PluginRuntime::Script {
                main,
                activation,
                extensions,
            } => {
                validate_extensions(extensions, "runtime")?;
                validate_package_path(main)?;
                if *activation == Activation::Startup
                    && !permissions.contains(&Permission::EventsSubscribe)
                {
                    return Err(invalid(
                        "runtime.activation",
                        "脚本启动激活需要 events.subscribe",
                    ));
                }
            }
            PluginRuntime::Sidecar {
                main, extensions, ..
            } => {
                validate_extensions(extensions, "runtime")?;
                if main.is_empty() || main.contains_key(&Platform::Any) {
                    return Err(invalid("runtime.main", "Sidecar 必须声明具体平台"));
                }
                for path in main.values() {
                    validate_package_path(path)?;
                }
            }
        }
        self.validate_contributions()
    }

    /// 不同贡献类型共享插件内的 ID 空间，避免注册顺序决定最终路由。
    fn validate_contributions(&self) -> Result<(), ProtocolError> {
        let contributions = &self.contributes;
        validate_extensions(&contributions.extensions, "contributes")?;
        let mut ids = BTreeSet::new();
        for tool in &contributions.tools {
            validate_contribution(&tool.id, &tool.title, &tool.extensions, &mut ids)?;
            self.require_view(&tool.view, false)?;
            if let Some(icon) = &tool.icon {
                validate_package_path(icon)?;
            }
        }
        for tab in &contributions.tabs {
            validate_contribution(&tab.id, &tab.title, &tab.extensions, &mut ids)?;
            self.require_view(&tab.view, false)?;
            if tab.state_version == 0 {
                return Err(invalid("contributes.tabs", "状态版本必须大于零"));
            }
        }
        if let Some(settings) = &contributions.settings {
            validate_extensions(&settings.extensions, "contributes.settings")?;
            validate_package_path(&settings.schema)?;
            if let Some(view) = &settings.view {
                self.require_view(view, true)?;
            }
        }
        for host in &contributions.host_types {
            validate_contribution(&host.id, &host.title, &host.extensions, &mut ids)?;
            validate_package_path(&host.schema)?;
            if let Some(view) = &host.view {
                self.require_view(view, true)?;
            }
        }
        for command in &contributions.commands {
            validate_contribution(&command.id, &command.title, &command.extensions, &mut ids)?;
            if command
                .keybinding
                .as_ref()
                .is_some_and(|value| !valid_text(value, 128))
            {
                return Err(invalid("contributes.commands", "快捷键无效"));
            }
        }
        for (id, view) in &self.views {
            if !valid_local_id(id) {
                return Err(invalid("views", "视图 ID 无效"));
            }
            match view {
                ViewDefinition::Native { file, extensions } => {
                    validate_extensions(extensions, "views")?;
                    validate_package_path(file)?;
                }
                ViewDefinition::Webview {
                    entry,
                    csp,
                    extensions,
                } => {
                    validate_extensions(extensions, "views")?;
                    validate_package_path(entry)?;
                    if csp.as_ref().is_some_and(|value| !valid_text(value, 2048)) {
                        return Err(invalid("views.csp", "CSP 无效"));
                    }
                }
            }
        }
        Ok(())
    }

    /// 事务式设置和主机表单只接受原生视图。
    fn require_view(&self, view: &str, native_only: bool) -> Result<(), ProtocolError> {
        match self.views.get(view) {
            Some(ViewDefinition::Native { .. }) => Ok(()),
            Some(ViewDefinition::Webview { .. }) if !native_only => Ok(()),
            _ => Err(invalid(
                "contributes.view",
                "视图不存在或不允许使用 WebView",
            )),
        }
    }
}

/// 身份规则与设计文档中的小写 ASCII 正则保持一致。
fn valid_id_segment(value: &str, max: usize) -> bool {
    (2..=max).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase_or_digit()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase_or_digit() || byte == b'-')
}

trait LowercaseOrDigit {
    fn is_ascii_lowercase_or_digit(&self) -> bool;
}
impl LowercaseOrDigit for u8 {
    /// 身份不受区域设置影响，数字和小写字母才具有稳定含义。
    fn is_ascii_lowercase_or_digit(&self) -> bool {
        self.is_ascii_lowercase() || self.is_ascii_digit()
    }
}

/// 局部身份与注册器共用首尾规则，避免路径式或无法注册的名称进入运行时。
pub(crate) fn valid_local_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// 保留国际化文本，但拒绝空文案和控制字符。
pub(crate) fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

/// 英文是兜底语言，不能只为当前开发环境提供翻译。
fn validate_localized(
    values: &BTreeMap<String, String>,
    location: &str,
) -> Result<(), ProtocolError> {
    if !values.contains_key("en")
        || values.len() > 128
        || values
            .iter()
            .any(|(locale, value)| !valid_text(locale, 64) || !valid_text(value, 4096))
    {
        return Err(invalid(location, "本地化字典必须包含非空 en 文案"));
    }
    Ok(())
}

/// 集中检查所有贡献的 ID，防止不同类型之间发生碰撞。
fn validate_contribution<'a>(
    id: &'a str,
    title: &str,
    extensions: &BTreeMap<String, Value>,
    ids: &mut BTreeSet<&'a str>,
) -> Result<(), ProtocolError> {
    validate_extensions(extensions, "contributes")?;
    if id.len() > 63 || !valid_local_id(id) || !valid_text(title, 512) || !ids.insert(id) {
        return Err(invalid("contributes", "贡献 ID 重复或文案无效"));
    }
    Ok(())
}
