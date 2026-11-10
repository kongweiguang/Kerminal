//! 主机配置类型及不可变 codec 目录；实现由组合根在数据扫描之前装配。
//!
//! @author kongweiguang

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, RwLock, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_domain::{
    connections::{CredentialRef, RemoteProtocol},
    identity::HostId,
    preferences::ConfigRevision,
    sessions::SessionTarget,
};
use tokio_util::sync::CancellationToken;

use super::{
    management::{HostDraft, HostManagementService, ManagedHost, ManagedLocalHost},
    probe::{
        ConnectionProbeOutcome, ConnectionProbeRequest, ConnectionProbeService, MAX_PROBE_TIMEOUT,
        MIN_PROBE_TIMEOUT,
    },
};
use crate::{AppError, AppResult};

/// codec 只处理非秘密配置；宿主负责文件身份、revision、密钥检查和原子提交。
pub trait HostConfigurationCodec: Any + Send + Sync + 'static {
    /// 从持久化字段生成插件投影；不能借此读取或改写其它主机的状态。
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value>;

    /// 将插件草稿编码成持久化字段，实际写入前仍由宿主复核并调用 decode。
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value>;

    /// 只在能列出已解码配置中的全部 vault 引用时返回 Some；默认未知不能视为无引用。
    ///
    /// 必须覆盖持久配置中的全部引用；若解码投影不足以证明覆盖则返回 None。
    /// 无秘密的类型应显式返回空集合；失败或 None 会阻止宿主清理候选凭据。
    ///
    /// @author kongweiguang
    fn credential_references(
        &self,
        _version: u32,
        _configuration: &toml::Value,
    ) -> AppResult<Option<Vec<CredentialRef>>> {
        Ok(None)
    }
}

/// 保存冻结 schema 和用于校验秘密引用的静态投影，不需要解密即可扫描配置。
pub struct ExternalHostCodec {
    schema: kerminal_plugin_protocol::ConfigurationSchema,
    stored_schema: kerminal_plugin_protocol::ConfigurationSchema,
    plugin_id: String,
}

impl ExternalHostCodec {
    /// 秘密约束在实际调用时用明文复核；静态扫描只接受本插件的 Vault 引用。
    pub fn new(
        schema: kerminal_plugin_protocol::ConfigurationSchema,
        plugin_id: &str,
    ) -> AppResult<Self> {
        kerminal_domain::plugins::PluginId::parse(plugin_id)
            .map_err(|_| AppError::invalid("插件身份无效"))?;
        let mut document = schema.document.clone();
        project_secret_schema(&mut document);
        let bytes =
            serde_json::to_vec(&document).map_err(|_| AppError::invalid("插件主机 schema 无效"))?;
        let stored_schema = kerminal_plugin_protocol::ConfigurationSchema::parse(&bytes)
            .map_err(|_| AppError::invalid("插件主机 schema 无效"))?;
        Ok(Self {
            schema,
            stored_schema,
            plugin_id: plugin_id.to_owned(),
        })
    }
}

/// 替换秘密叶节点的约束以便检查引用，原 schema 仍用于调用期的明文校验。
fn project_secret_schema(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object)
            if object.get("x-secret") == Some(&serde_json::Value::Bool(true)) =>
        {
            *value = serde_json::json!({"type":"string"});
        }
        serde_json::Value::Object(object) => object.values_mut().for_each(project_secret_schema),
        serde_json::Value::Array(items) => items.iter_mut().for_each(project_secret_schema),
        _ => {}
    }
}

impl HostConfigurationCodec for ExternalHostCodec {
    /// 不接受明文或其他插件的秘密引用，普通字段按投影 schema 执行静态校验。
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        if version != 1 || !configuration.is_table() {
            return Err(AppError::unsupported("插件主机配置版本不可用"));
        }
        let value = serde_json::to_value(configuration)
            .map_err(|_| AppError::invalid("插件主机配置无效"))?;
        let paths = crate::plugin_settings::schema::secret_paths(&self.schema, &value)?;
        let prefix = format!("credential:kerminal-plugin:{}:settings:", self.plugin_id);
        for path in paths {
            let reference = value
                .pointer(&path)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::invalid("插件主机秘密引用无效"))?;
            let Some(tail) = reference
                .strip_prefix(&prefix)
                .and_then(|tail| tail.strip_suffix(":v1"))
            else {
                return Err(AppError::invalid("插件主机秘密引用无效"));
            };
            let Some((hash, id)) = tail.split_once(':') else {
                return Err(AppError::invalid("插件主机秘密引用无效"));
            };
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                || uuid::Uuid::parse_str(id).is_err()
            {
                return Err(AppError::invalid("插件主机秘密引用无效"));
            }
        }
        self.stored_schema
            .validate(&value)
            .map_err(|_| AppError::invalid("插件主机配置不符合 schema"))?;
        Ok(configuration.clone())
    }

    /// 保存与读取执行同一校验，不能绕过声明格式写入磁盘。
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }

    /// 列出全部声明的秘密引用，保证其他主机的凭据清理不会误删它们。
    fn credential_references(
        &self,
        version: u32,
        configuration: &toml::Value,
    ) -> AppResult<Option<Vec<CredentialRef>>> {
        self.decode(version, configuration)?;
        let value = serde_json::to_value(configuration)
            .map_err(|_| AppError::invalid("插件主机配置无效"))?;
        let mut references = Vec::new();
        for path in crate::plugin_settings::schema::secret_paths(&self.schema, &value)? {
            let reference = value
                .pointer(&path)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::invalid("插件主机秘密引用无效"))?;
            references.push(
                CredentialRef::new(reference)
                    .map_err(|_| AppError::invalid("插件主机秘密引用无效"))?,
            );
        }
        Ok(Some(references))
    }
}

/// 主机秘密候选在配置 CAS 前独立创建；未确认结果保留候选供恢复。
#[derive(Clone)]
pub struct ExternalHostSecretService {
    plugin: kerminal_domain::plugins::PluginId,
    schema: kerminal_plugin_protocol::ConfigurationSchema,
    vault: Arc<dyn crate::plugin_settings::PluginSecretRepository>,
}

impl ExternalHostSecretService {
    /// 插件身份和 schema 均来自冻结清单，禁止由表单草稿重新指定。
    pub fn new(
        plugin: kerminal_domain::plugins::PluginId,
        schema: kerminal_plugin_protocol::ConfigurationSchema,
        vault: Arc<dyn crate::plugin_settings::PluginSecretRepository>,
    ) -> Self {
        Self {
            plugin,
            schema,
            vault,
        }
    }

    /// 测试未保存草稿时仅读取已有引用，不创建 Vault 候选或提交主机配置。
    pub async fn preview(
        &self,
        host: &HostId,
        mut values: serde_json::Value,
        previous: Option<&toml::Value>,
        mut edits: BTreeMap<String, crate::plugin_settings::PluginSecretEdit>,
        cancellation: CancellationToken,
    ) -> AppResult<(
        serde_json::Value,
        serde_json::Map<String, serde_json::Value>,
    )> {
        use crate::plugin_settings::{
            PluginSecretEdit,
            schema::{clear_secret_values, remove_pointer, secret_paths, set_pointer},
        };
        let previous = previous
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| AppError::invalid("插件主机配置无效"))?;
        let paths = secret_paths(&self.schema, &values)?;
        if edits.keys().any(|path| !paths.contains(path)) {
            return Err(AppError::invalid("插件主机秘密字段未声明"));
        }
        let mut plaintext = BTreeMap::new();
        for path in &paths {
            let current = previous
                .as_ref()
                .and_then(|value| value.pointer(path))
                .and_then(serde_json::Value::as_str);
            let supplied = values
                .pointer(path)
                .ok_or_else(|| AppError::invalid("插件主机秘密路径无效"))?;
            if !supplied.is_null() && supplied.as_str() != current {
                return Err(AppError::invalid("插件主机秘密只能通过专用输入提交"));
            }
            match edits.remove(path) {
                Some(PluginSecretEdit::Remove) => {
                    remove_pointer(&mut values, path)?;
                }
                Some(PluginSecretEdit::Replace(secret)) => {
                    if secret.is_empty() {
                        return Err(AppError::invalid("插件主机秘密不能为空"));
                    }
                    plaintext.insert(path.clone(), secret);
                }
                None if current.is_none() => {
                    remove_pointer(&mut values, path)?;
                }
                None => {
                    let reference =
                        current.ok_or_else(|| AppError::invalid("插件主机秘密尚未提供"))?;
                    let slot = format!("host/{}/{}", host.as_str(), path);
                    let secret = self
                        .vault
                        .read_setting(&self.plugin, &slot, reference, cancellation.clone())
                        .await?;
                    plaintext.insert(path.clone(), secret);
                }
            }
        }
        let validation = (|| {
            for (path, secret) in &plaintext {
                set_pointer(
                    &mut values,
                    path,
                    serde_json::Value::String(secret.to_string()),
                )?;
            }
            self.schema
                .validate(&values)
                .map_err(|_| AppError::invalid("插件主机配置不符合 schema"))
        })();
        clear_secret_values(&mut values, &paths);
        validation?;
        let secrets = plaintext
            .iter()
            .map(|(path, secret)| (path.clone(), serde_json::Value::String(secret.to_string())))
            .collect();
        Ok((values, secrets))
    }

    /// 明文只接受专用零化输入；原值须属于同一主机字段，候选成功后才返回可保存的引用。
    pub async fn prepare(
        &self,
        host: &HostId,
        mut values: serde_json::Value,
        previous: Option<&toml::Value>,
        mut edits: BTreeMap<String, crate::plugin_settings::PluginSecretEdit>,
        cancellation: CancellationToken,
    ) -> AppResult<toml::Value> {
        use crate::plugin_settings::{
            PluginSecretEdit,
            schema::{clear_secret_values, remove_pointer, secret_paths, set_pointer},
        };
        if !values.is_object() {
            return Err(AppError::invalid("插件主机配置必须是对象"));
        }
        let previous = previous
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| AppError::invalid("插件主机配置无效"))?;
        let paths = secret_paths(&self.schema, &values)?;
        if edits.keys().any(|path| !paths.contains(path)) {
            return Err(AppError::invalid("插件主机秘密字段未声明"));
        }
        let mut stored = values.clone();
        let mut plaintext = BTreeMap::new();
        let mut replacements = std::collections::BTreeSet::new();
        for path in &paths {
            let current = previous
                .as_ref()
                .and_then(|value| value.pointer(path))
                .and_then(serde_json::Value::as_str);
            let supplied = values
                .pointer(path)
                .ok_or_else(|| AppError::invalid("插件主机秘密路径无效"))?;
            if !supplied.is_null() && supplied.as_str() != current {
                return Err(AppError::invalid("插件主机秘密只能通过专用输入提交"));
            }
            match edits.remove(path) {
                Some(PluginSecretEdit::Remove) => {
                    remove_pointer(&mut values, path)?;
                    remove_pointer(&mut stored, path)?;
                }
                Some(PluginSecretEdit::Replace(secret)) => {
                    if secret.is_empty() {
                        return Err(AppError::invalid("插件主机秘密不能为空"));
                    }
                    plaintext.insert(path.clone(), secret);
                    replacements.insert(path.clone());
                }
                None if current.is_none() => {
                    remove_pointer(&mut values, path)?;
                    remove_pointer(&mut stored, path)?;
                }
                None => {
                    let reference =
                        current.ok_or_else(|| AppError::invalid("插件主机秘密尚未提供"))?;
                    let slot = format!("host/{}/{}", host.as_str(), path);
                    let secret = self
                        .vault
                        .read_setting(&self.plugin, &slot, reference, cancellation.clone())
                        .await?;
                    plaintext.insert(path.clone(), secret);
                    set_pointer(
                        &mut stored,
                        path,
                        serde_json::Value::String(reference.to_owned()),
                    )?;
                }
            }
        }
        let validation = (|| {
            for (path, secret) in &plaintext {
                set_pointer(
                    &mut values,
                    path,
                    serde_json::Value::String(secret.to_string()),
                )?;
            }
            self.schema
                .validate(&values)
                .map_err(|_| AppError::invalid("插件主机配置不符合 schema"))
        })();
        clear_secret_values(&mut values, &paths);
        validation?;
        for path in replacements {
            let secret = plaintext
                .remove(&path)
                .ok_or_else(|| AppError::infrastructure("插件主机秘密候选缺失"))?;
            let slot = format!("host/{}/{}", host.as_str(), path);
            let reference = self
                .vault
                .create_setting(&self.plugin, &slot, secret, cancellation.clone())
                .await?;
            set_pointer(&mut stored, &path, serde_json::Value::String(reference))?;
        }
        toml::Value::try_from(stored).map_err(|_| AppError::invalid("插件主机配置无法持久化"))
    }
}

/// 运行提供方只弱持有插件服务，避免主机目录与宿主 API 相互保活形成循环。
pub struct ExternalHostProvider {
    plugin: Arc<crate::external_contributions::PreparedPlugin>,
    runtime: Weak<crate::plugin_runtime::PluginRuntimeService>,
    local_type: String,
    title_locale: String,
    secrets: Arc<dyn crate::plugin_settings::PluginSecretRepository>,
}

impl ExternalHostProvider {
    /// 类型和插件来自同一冻结声明，后续不能由请求参数替换提供方。
    pub fn new(
        plugin: Arc<crate::external_contributions::PreparedPlugin>,
        runtime: Weak<crate::plugin_runtime::PluginRuntimeService>,
        local_type: String,
        title_locale: String,
        secrets: Arc<dyn crate::plugin_settings::PluginSecretRepository>,
    ) -> AppResult<Self> {
        if !plugin
            .package
            .manifest
            .contributes
            .host_types
            .iter()
            .any(|host| host.id == local_type)
        {
            return Err(AppError::invalid("插件主机类型未声明"));
        }
        Ok(Self {
            plugin,
            runtime,
            local_type,
            title_locale,
            secrets,
        })
    }

    /// 仅在本次调用中解密已绑定主机和字段的秘密，并对实际值重新校验 schema。
    async fn call(
        &self,
        host: &PluginHost,
        method: &str,
        cancel: CancellationToken,
    ) -> AppResult<serde_json::Value> {
        if host.provider_id != self.plugin.id.as_str()
            || host.type_id != format!("{}/{}", self.plugin.id.as_str(), self.local_type)
        {
            return Err(AppError::invalid("插件主机类型不匹配"));
        }
        let config = host
            .configuration
            .as_ref()
            .ok_or_else(|| AppError::unsupported("插件主机配置不可用"))?;
        let mut config =
            serde_json::to_value(config).map_err(|_| AppError::invalid("插件主机配置无效"))?;
        let host_type = self
            .plugin
            .package
            .manifest
            .contributes
            .host_types
            .iter()
            .find(|entry| entry.id == self.local_type)
            .ok_or_else(|| AppError::invalid("插件主机类型未声明"))?;
        let schema = self
            .plugin
            .package
            .configuration_schemas
            .get(&host_type.schema)
            .ok_or_else(|| AppError::invalid("插件主机 schema 缺失"))?;
        let paths = crate::plugin_settings::schema::secret_paths(schema, &config)?;
        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| AppError::unsupported("插件运行服务不可用"))?;
        let mut plaintext = BTreeMap::new();
        for path in &paths {
            let reference = config
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::invalid("插件主机秘密引用无效"))?;
            let slot = format!("host/{}/{}", host.id.as_str(), path);
            let secret = self
                .secrets
                .read_setting(&self.plugin.id, &slot, reference, cancel.clone())
                .await?;
            plaintext.insert(path.clone(), secret);
        }
        for (path, secret) in &plaintext {
            if let Some(target) = config.pointer_mut(path) {
                *target = serde_json::Value::String(secret.to_string());
            } else {
                crate::plugin_settings::schema::clear_secret_values(&mut config, &paths);
                return Err(AppError::invalid("插件主机秘密路径无效"));
            }
        }
        let validation = schema
            .validate(&config)
            .map_err(|_| AppError::invalid("插件主机配置不符合 schema"));
        crate::plugin_settings::schema::clear_secret_values(&mut config, &paths);
        validation?;
        let secrets: serde_json::Map<String, serde_json::Value> = plaintext
            .iter()
            .map(|(path, secret)| (path.clone(), serde_json::Value::String(secret.to_string())))
            .collect();
        // 探测可能涉及真实网络，按协议允许 60 秒；打开操作使用较短超时。
        let timeout = if method == "host.test" {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(10)
        };
        runtime
            .call_plugin(
                self.plugin.id.clone(),
                method.into(),
                serde_json::json!({"host_type":self.local_type,"config":config,"secrets":secrets}),
                Instant::now() + timeout,
                cancel,
            )
            .await
    }

    /// 标题在冻结资源中按当前语言和英文回退解析，不信任插件运行时提供的标题。
    fn tab_title(&self, title: &str) -> AppResult<String> {
        let Some(key) = title
            .strip_prefix('%')
            .and_then(|text| text.strip_suffix('%'))
        else {
            return Ok(title.to_owned());
        };
        self.plugin
            .localizations
            .get(&self.title_locale)
            .and_then(|messages| messages.get(key))
            .or_else(|| {
                self.plugin
                    .localizations
                    .get("en")
                    .and_then(|messages| messages.get(key))
            })
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .ok_or_else(|| AppError::invalid("插件标签标题缺少翻译"))
    }
}

#[async_trait]
impl HostTypeProvider for ExternalHostProvider {
    /// 探测成功由插件明确给出，不把 RPC 成功等同连接成功。
    async fn test(&self, host: &PluginHost, cancel: CancellationToken) -> AppResult<()> {
        let value = self.call(host, "host.test", cancel).await?;
        let result: kerminal_plugin_protocol::HostTestResult =
            serde_json::from_value(value).map_err(|_| AppError::invalid("插件主机测试结果无效"))?;
        if result.ok {
            Ok(())
        } else {
            Err(AppError::infrastructure("插件主机测试失败"))
        }
    }

    /// 插件只能打开自己声明的标签，并由宿主冻结其标题和状态版本。
    async fn open(
        &self,
        host: &PluginHost,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        let value = self.call(host, "host.open", cancel).await?;
        let result: kerminal_plugin_protocol::HostOpenResult =
            serde_json::from_value(value).map_err(|_| AppError::invalid("插件主机打开结果无效"))?;
        let tab = self
            .plugin
            .package
            .manifest
            .contributes
            .tabs
            .iter()
            .find(|tab| tab.id == result.tab)
            .ok_or_else(|| AppError::invalid("插件主机返回未声明的标签"))?;
        let size = serde_json::to_vec(&result.state)
            .map_err(|_| AppError::invalid("插件标签状态无效"))?
            .len();
        if size > kerminal_plugin_protocol::MAX_VIEW_STATE_BYTES {
            return Err(AppError::invalid("插件标签状态超过上限"));
        }
        Ok(PluginHostOpenIntent::Content {
            provider_id: self.plugin.id.as_str().to_owned(),
            content_type_id: format!("{}/{}", self.plugin.id.as_str(), tab.id),
            title: self.tab_title(&tab.title)?,
            state_version: tab.state_version,
            state: result.state,
        })
    }
}

/// 一个稳定类型只由一个提供方拥有，避免提供方缺失时被其他实现接管。
#[derive(Clone)]
#[non_exhaustive]
pub struct HostConfigurationType {
    pub provider_id: String,
    pub type_id: String,
    pub codec: Arc<dyn HostConfigurationCodec>,
    provider: Option<Arc<dyn HostTypeProvider>>,
}

impl HostConfigurationType {
    /// 提前限制标识语法，使配置可跨平台保存并避免空白或控制字符产生歧义。
    pub fn new(
        provider_id: impl Into<String>,
        type_id: impl Into<String>,
        codec: Arc<dyn HostConfigurationCodec>,
    ) -> AppResult<Self> {
        let provider_id = provider_id.into();
        let type_id = type_id.into();
        if !valid_host_type_id(&provider_id) || !valid_host_type_id(&type_id) {
            return Err(AppError::invalid("主机类型或提供方标识无效"));
        }
        Ok(Self {
            provider_id,
            type_id,
            codec,
            provider: None,
        })
    }

    /// 无需运行时服务的实现可随 codec 一起登记；其他实现可在服务装配后绑定。
    pub fn with_provider(mut self, provider: Arc<dyn HostTypeProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// 返回随 codec 预先登记的实现，后期绑定通过目录查询。
    pub fn provider(&self) -> Option<&Arc<dyn HostTypeProvider>> {
        self.provider.as_ref()
    }
}

/// 配置目录保持不变，运行提供方可在服务装配后按类型绑定一次。
#[derive(Clone, Default)]
pub struct HostConfigurationCatalog {
    types: BTreeMap<String, HostConfigurationType>,
    runtime_providers: Arc<RwLock<BTreeMap<String, Arc<dyn HostTypeProvider>>>>,
}

impl HostConfigurationCatalog {
    /// 重复类型一律拒绝，防止不同插件或内置实现发生顺序相关的覆盖。
    pub fn new(types: impl IntoIterator<Item = HostConfigurationType>) -> AppResult<Self> {
        let mut catalog = BTreeMap::new();
        for host_type in types {
            if !valid_host_type_id(&host_type.provider_id)
                || !valid_host_type_id(&host_type.type_id)
            {
                return Err(AppError::invalid("主机类型或提供方标识无效"));
            }
            if catalog
                .insert(host_type.type_id.clone(), host_type)
                .is_some()
            {
                return Err(AppError::conflict("主机类型重复注册"));
            }
        }
        Ok(Self {
            types: catalog,
            runtime_providers: Arc::default(),
        })
    }

    /// 同时匹配提供方和类型，防止已安装的同名类型读取不属于自己的配置。
    pub fn get(&self, provider_id: &str, type_id: &str) -> Option<&HostConfigurationType> {
        self.types
            .get(type_id)
            .filter(|entry| entry.provider_id == provider_id)
    }

    /// 为主机编辑入口提供冻结顺序，不暴露可变注册表。
    pub fn iter(&self) -> impl Iterator<Item = &HostConfigurationType> {
        self.types.values()
    }

    /// 只允许已冻结类型绑定一次运行实现，防止运行期替换其所有者或 codec。
    pub fn bind_runtime_provider(
        &self,
        provider_id: &str,
        type_id: &str,
        provider: Arc<dyn HostTypeProvider>,
    ) -> AppResult<()> {
        let entry = self
            .get(provider_id, type_id)
            .ok_or_else(|| AppError::not_found("主机类型未注册"))?;
        if entry.provider().is_some() {
            return Err(AppError::conflict("主机运行提供方重复注册"));
        }
        let mut providers = self
            .runtime_providers
            .write()
            .map_err(|_| AppError::infrastructure("主机运行目录不可用"))?;
        if providers.contains_key(type_id) {
            return Err(AppError::conflict("主机运行提供方重复注册"));
        }
        providers.insert(type_id.to_owned(), provider);
        Ok(())
    }

    /// 同时核对提供方身份并取得共享实现，锁不会跨越异步调用。
    pub fn runtime_provider(
        &self,
        provider_id: &str,
        type_id: &str,
    ) -> AppResult<Option<Arc<dyn HostTypeProvider>>> {
        let Some(entry) = self.get(provider_id, type_id) else {
            return Ok(None);
        };
        if let Some(provider) = entry.provider() {
            return Ok(Some(provider.clone()));
        }
        let providers = self
            .runtime_providers
            .read()
            .map_err(|_| AppError::infrastructure("主机运行目录不可用"))?;
        Ok(providers.get(type_id).cloned())
    }
}

/// 主机配置身份与其它贡献共享命名空间规则，避免清单通过后被配置目录拒绝。
/// 旧内置身份仍按原值保存，外部身份允许唯一的插件分隔符与完整长度。
pub fn valid_host_type_id(value: &str) -> bool {
    crate::plugins::validate_id(value).is_ok()
}

/// 缺失提供方时仍返回目录元数据，调用方必须禁止打开或编辑不可用主机。
#[derive(Clone)]
pub struct PluginHost {
    pub id: HostId,
    pub provider_id: String,
    pub type_id: String,
    pub name: String,
    pub group_id: Option<String>,
    pub sort_order: i64,
    pub configuration_version: u32,
    pub configuration: Option<toml::Value>,
    /// codec 与执行能力分别安装，避免可编辑的配置被误认为可运行。
    pub runtime_available: bool,
    pub revision: ConfigRevision,
}

impl PluginHost {
    /// 只有成功由已注册 codec 解码的配置才允许交给插件执行。
    pub fn is_available(&self) -> bool {
        self.configuration.is_some()
    }

    /// 运行需要有效投影和实际提供方；仅有 codec 仍可保留或编辑配置。
    pub fn is_runnable(&self) -> bool {
        self.is_available() && self.runtime_available
    }
}

/// 将表单 JSON 转换为配置文档；界面层不依赖持久化格式实现。
pub fn host_configuration_from_json(value: serde_json::Value) -> AppResult<toml::Value> {
    toml::Value::try_from(value).map_err(|_| AppError::invalid("主机配置无法转换"))
}

/// 草稿仅携带非秘密字段；密钥仍须由现有 vault 能力单独管理。
#[derive(Clone)]
pub struct PluginHostDraft {
    pub id: Option<HostId>,
    pub provider_id: String,
    pub type_id: String,
    pub name: String,
    pub group_id: Option<String>,
    pub sort_order: i64,
    pub configuration_version: u32,
    pub configuration: toml::Value,
}

/// 内置主机保持原有类型和秘密引用，不能先转成插件的通用 TOML。
/// 两种完整配置均间接持有，避免它们的字段尺寸扩大提供方分派及异步状态。
#[derive(Clone, Debug)]
pub enum BuiltinHost {
    Remote(Box<ManagedHost>),
    Local(Box<ManagedLocalHost>),
}

impl BuiltinHost {
    /// 操作和回读始终使用原始主机身份，避免提供方重定向其他主机。
    pub fn id(&self) -> &HostId {
        match self {
            Self::Remote(host) => &host.id,
            Self::Local(host) => &host.id,
        }
    }

    /// 协议 token 与已有配置格式一致，不能从显示名称推导。
    pub fn configuration_type(&self) -> &'static str {
        match self {
            Self::Local(_) => "local",
            Self::Remote(host) => match host.protocol {
                RemoteProtocol::Ssh => "ssh",
                RemoteProtocol::Sftp => "sftp",
                RemoteProtocol::Telnet => "telnet",
                RemoteProtocol::Serial => "serial",
                RemoteProtocol::Rdp => "rdp",
            },
        }
    }

    /// 只用持久化 revision 判断异步结果是否仍属于原配置。
    pub fn revision(&self) -> &ConfigRevision {
        match self {
            Self::Remote(host) => &host.revision,
            Self::Local(host) => &host.revision,
        }
    }
}

/// 内置打开仅传身份和协议，具体信任、凭据与进程生命周期交回已有用例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinHostOpenIntent {
    pub host_id: HostId,
    pub configuration_type: String,
}

/// 提供方只返回可由既有工作区、终端或 RDP 服务执行的受控目标。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PluginHostOpenIntent {
    Builtin(BuiltinHostOpenIntent),
    Terminal(SessionTarget),
    Content {
        provider_id: String,
        content_type_id: String,
        title: String,
        state_version: u32,
        state: serde_json::Value,
    },
    Rdp {
        host_id: HostId,
    },
}

/// 内置探测保留信任挑战和协议分层结果，扩展探测只承诺提供方返回成功。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostTestOutcome {
    Plugin,
    Builtin(ConnectionProbeOutcome),
}

/// 插件运行实现负责探测和目标转换，资源创建与回收仍归已有服务所有。
#[async_trait]
pub trait HostTypeProvider: Send + Sync + 'static {
    /// 取消信号传到实际探测实现，避免只丢弃已开始的网络结果。
    async fn test(&self, host: &PluginHost, cancel: CancellationToken) -> AppResult<()>;

    /// 返回经过类型约束的打开意图，调用方再交给相应会话 owner。
    async fn open(
        &self,
        host: &PluginHost,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent>;

    /// 内置草稿测试有独立的凭据和信任结果，不把缺省实现伪装成探测成功。
    async fn test_builtin(
        &self,
        _host: &BuiltinHost,
        _cancel: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        Err(AppError::unsupported("内置主机测试由连接表单执行"))
    }

    /// 草稿探测保留一次性凭据和强类型结果，不要求先保存配置。
    async fn probe_builtin_draft(
        &self,
        _request: ConnectionProbeRequest,
        _cancel: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        Err(AppError::unsupported("内置主机草稿测试不可用"))
    }

    /// 内置协议默认不交给插件目标转换，提供方须显式支持其强类型投影。
    async fn open_builtin(
        &self,
        _host: &BuiltinHost,
        _cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        Err(AppError::unsupported("内置主机运行提供方不支持此操作"))
    }
}

/// 只把已登记的内置主机送回既有连接 owner，不创建第二套协议运行时。
pub struct BuiltinHostProvider {
    probe: Arc<ConnectionProbeService>,
}

impl BuiltinHostProvider {
    /// 依赖原始探测服务，避免通过贡献目录再次分派形成递归。
    pub fn new(probe: Arc<ConnectionProbeService>) -> Self {
        Self {
            probe: Arc::new(probe.direct()),
        }
    }
}

#[async_trait]
impl HostTypeProvider for BuiltinHostProvider {
    /// 内置实现不能接管扩展主机，避免配置身份被跨提供方解释。
    async fn test(&self, _host: &PluginHost, _cancel: CancellationToken) -> AppResult<()> {
        Err(AppError::unsupported("内置提供方不能测试插件主机"))
    }

    /// 内置实现不能为扩展主机伪造既有协议意图。
    async fn open(
        &self,
        _host: &PluginHost,
        _cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        Err(AppError::unsupported("内置提供方不能打开插件主机"))
    }

    /// 已保存远程主机完整保留显示状态并复用探测后端；本地主机没有远程探测语义。
    ///
    /// @author kongweiguang
    async fn test_builtin(
        &self,
        host: &BuiltinHost,
        cancel: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        let BuiltinHost::Remote(host) = host else {
            return Err(AppError::unsupported("本地主机不支持连接探测"));
        };
        let draft = HostDraft {
            id: Some(host.id.clone()),
            group_id: host.group_id.clone(),
            name: host.name.clone(),
            note: host.note.clone(),
            protocol: host.protocol,
            endpoint: host.endpoint.clone(),
            authentication: host.authentication.clone(),
            authentication_mode: host.authentication_mode,
            rdp_password: host.rdp_password.clone(),
            route: host.route.clone(),
            serial: host.serial.clone(),
            tags: host.tags.clone(),
            sort_order: host.sort_order,
            connect_timeout_ms: host.connect_timeout_ms,
            keepalive_interval_ms: host.keepalive_interval_ms,
            terminal_options: host.terminal_options.clone(),
            sftp_options: host.sftp_options.clone(),
            rdp_display: host.rdp_display.clone(),
        };
        self.probe_builtin_draft(
            ConnectionProbeRequest {
                draft,
                credential_changes: Vec::new(),
                timeout: Duration::from_millis(host.connect_timeout_ms)
                    .clamp(MIN_PROBE_TIMEOUT, MAX_PROBE_TIMEOUT),
            },
            cancel,
        )
        .await
    }

    /// 将未经持久化的完整草稿交给原始用例，取消和结果原样传递。
    async fn probe_builtin_draft(
        &self,
        request: ConnectionProbeRequest,
        cancel: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        self.probe.probe(request, cancel).await
    }

    /// 只传递配置身份，后续 owner 负责读取凭据、尽力记录主机密钥和实际打开。
    async fn open_builtin(
        &self,
        host: &BuiltinHost,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        ensure_host_operation_not_cancelled(&cancel)?;
        Ok(PluginHostOpenIntent::Builtin(BuiltinHostOpenIntent {
            host_id: host.id().clone(),
            configuration_type: host.configuration_type().to_owned(),
        }))
    }
}

enum ResolvedHost {
    Plugin(PluginHost),
    Builtin(BuiltinHost),
}

impl ResolvedHost {
    /// 提供方结果必须绑定本次读取的身份，不能借意图打开其他记录。
    fn id(&self) -> &HostId {
        match self {
            Self::Plugin(host) => &host.id,
            Self::Builtin(host) => host.id(),
        }
    }
}

/// 打开和探测均先回读当前配置，阻断缺失提供方或无运行实现的记录。
#[derive(Clone)]
pub struct PluginHostService {
    hosts: HostManagementService,
    catalog: Arc<HostConfigurationCatalog>,
}

impl PluginHostService {
    /// 目录必须与配置仓储使用同一份冻结实例，调用方负责在组合根共享它。
    pub fn new(hosts: HostManagementService, catalog: Arc<HostConfigurationCatalog>) -> Self {
        Self { hosts, catalog }
    }

    /// 测试前后核对配置并传递取消，旧配置的迟到结果不能报告为当前主机成功。
    pub async fn test(&self, id: &HostId, cancel: CancellationToken) -> AppResult<HostTestOutcome> {
        ensure_host_operation_not_cancelled(&cancel)?;
        let (host, provider) = self.resolve(id).await?;
        ensure_host_operation_not_cancelled(&cancel)?;
        let result = match &host {
            ResolvedHost::Plugin(host) => provider
                .test(host, cancel.clone())
                .await
                .map(|()| HostTestOutcome::Plugin),
            ResolvedHost::Builtin(host) => provider
                .test_builtin(host, cancel.clone())
                .await
                .map(HostTestOutcome::Builtin),
        };
        ensure_host_operation_not_cancelled(&cancel)?;
        self.ensure_current(&host, &cancel).await?;
        result
    }

    /// 返回类型化意图而不直接操作 UI，调用方负责迟到结果和生命周期门禁。
    pub async fn open(
        &self,
        id: &HostId,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        ensure_host_operation_not_cancelled(&cancel)?;
        let (host, provider) = self.resolve(id).await?;
        ensure_host_operation_not_cancelled(&cancel)?;
        let result = match &host {
            ResolvedHost::Plugin(host) => provider.open(host, cancel.clone()).await,
            ResolvedHost::Builtin(host) => provider.open_builtin(host, cancel.clone()).await,
        };
        ensure_host_operation_not_cancelled(&cancel)?;
        self.ensure_current(&host, &cancel).await?;
        let intent = result?;
        match (&host, &intent) {
            (ResolvedHost::Builtin(host), PluginHostOpenIntent::Builtin(intent))
                if intent.host_id == *host.id()
                    && intent.configuration_type == host.configuration_type() => {}
            (ResolvedHost::Builtin(_), _) | (_, PluginHostOpenIntent::Builtin(_)) => {
                return Err(AppError::invalid("内置主机返回的身份或类型无效"));
            }
            (
                ResolvedHost::Plugin(host),
                PluginHostOpenIntent::Content {
                    provider_id,
                    state_version,
                    ..
                },
            ) if provider_id != &host.provider_id || *state_version == 0 => {
                return Err(AppError::invalid("插件主机返回的内容提供方或版本无效"));
            }
            (_, PluginHostOpenIntent::Rdp { host_id }) if host_id != host.id() => {
                return Err(AppError::invalid("主机返回的 RDP 身份无效"));
            }
            _ => {}
        }
        Ok(intent)
    }

    /// 运行结果必须对应同一份持久配置；回读失败或记录变化都不能当作成功。
    async fn ensure_current(
        &self,
        host: &ResolvedHost,
        cancel: &CancellationToken,
    ) -> AppResult<()> {
        let unchanged = match host {
            ResolvedHost::Plugin(host) => {
                let current = self.hosts.load_plugin_host(&host.id).await;
                ensure_host_operation_not_cancelled(cancel)?;
                current?.is_some_and(|current| {
                    current.revision == host.revision
                        && current.provider_id == host.provider_id
                        && current.type_id == host.type_id
                        && current.configuration_version == host.configuration_version
                        && current.is_runnable()
                })
            }
            ResolvedHost::Builtin(BuiltinHost::Remote(host)) => {
                let current = self.hosts.load_host(&host.id).await;
                ensure_host_operation_not_cancelled(cancel)?;
                current?.is_some_and(|current| {
                    current.revision == host.revision && current.protocol == host.protocol
                })
            }
            ResolvedHost::Builtin(BuiltinHost::Local(host)) => {
                let current = self.hosts.load_local_host(&host.id).await;
                ensure_host_operation_not_cancelled(cancel)?;
                current?.is_some_and(|current| current.revision == host.revision)
            }
        };
        if unchanged {
            Ok(())
        } else {
            Err(AppError::conflict("主机配置已变化，请重新读取"))
        }
    }

    /// 内置与扩展记录分别读取强类型投影，再按冻结身份寻找运行实现。
    async fn resolve(&self, id: &HostId) -> AppResult<(ResolvedHost, Arc<dyn HostTypeProvider>)> {
        if let Some(host) = self.hosts.load_plugin_host(id).await? {
            let provider = self
                .catalog
                .runtime_provider(&host.provider_id, &host.type_id)?
                .filter(|_| host.is_available())
                .ok_or_else(|| AppError::unsupported("插件主机提供方不可用"))?;
            return Ok((ResolvedHost::Plugin(host), provider));
        }
        let host = if let Some(host) = self.hosts.load_host(id).await? {
            BuiltinHost::Remote(Box::new(host))
        } else if let Some(host) = self.hosts.load_local_host(id).await? {
            BuiltinHost::Local(Box::new(host))
        } else {
            return Err(AppError::not_found("主机不存在"));
        };
        let provider = self
            .catalog
            .runtime_provider("kerminal", host.configuration_type())?
            .ok_or_else(|| AppError::unsupported("内置主机运行提供方不可用"))?;
        Ok((ResolvedHost::Builtin(host), provider))
    }
}

/// 在只读回查和运行提供方边界重复确认取消，避免已取消操作进入下一阶段。
fn ensure_host_operation_not_cancelled(cancel: &CancellationToken) -> AppResult<()> {
    if cancel.is_cancelled() {
        Err(AppError::cancelled("插件主机操作已取消"))
    } else {
        Ok(())
    }
}
