//! 单实例取得所有权后冻结插件版本；开发目录不进入正式安装状态。
//! @author kongweiguang

use std::{collections::BTreeMap, path::Path, sync::Arc};

use kerminal_application::{
    AppError, AppResult, external_contributions::PreparedPlugin,
    plugins::PluginId as RegistryPluginId,
};
use kerminal_domain::plugins::{PluginBootPhase, PluginId, PluginSource, PluginVersion};
use kerminal_infrastructure::{
    plugins::{
        boot::{BootPackageValidator, BootSnapshot, ProtocolBootValidator},
        store::PluginStore,
    },
    storage::StorageCoordinator,
};
use kerminal_plugin_protocol::Platform;

use super::arguments::PluginStartupOptions;

// 宿主 API 版本权威值在 application 宿主侧（`kerminal_application::plugins::HOST_API_VERSION`），
// 这里重导出旧名 `HOST_API` 是为了保持 composition、dev_reload 与 assembly 等既有调用点
// 不感知常量迁移；三个维度（宿主能力契约、SDK crate 版本、JSON-RPC PROTOCOL_VERSION）
// 互不推导，禁止把 SDK 或 RPC 版本当宿主契约一起升级。
//
// @author kongweiguang
pub use kerminal_application::plugins::HOST_API_VERSION as HOST_API;

/// 同时保留权威启动记录和内存资源，关闭窗口不会意外丢失启动标记所有权。
pub struct StartupPlugins {
    pub store: PluginStore,
    pub snapshot: BootSnapshot<PreparedPlugin>,
    pub enabled: Vec<Arc<PreparedPlugin>>,
    pub options: PluginStartupOptions,
}

impl StartupPlugins {
    /// 持久状态以领域 `PluginId` 保存，目录冻结用应用层贡献身份；两层类型刻意分离，
    /// 是因为前者受存储格式约束、后者受静态贡献校验约束。这里逐项转换而不静默丢弃：
    /// 状态文件里出现目录无法表达的键代表已停用单元不会再被尊重，宁可让启动失败。
    ///
    /// @author kongweiguang
    pub(crate) fn builtin_states(&self) -> AppResult<BTreeMap<RegistryPluginId, bool>> {
        self.snapshot
            .store
            .state
            .builtins
            .iter()
            .map(|(id, enabled)| {
                RegistryPluginId::new(id.as_str())
                    .map(|id| (id, *enabled))
                    .map_err(|source| {
                        AppError::infrastructure_with_source("内置插件启停身份无效", source)
                    })
            })
            .collect()
    }

    /// 声明先于配置扫描冻结；当前阶段的主机 codec 由同一启动目录装配，不能动态替换。
    pub fn catalogs(&self, locale: &str) -> AppResult<crate::plugin_catalogs::PluginCatalogs> {
        // 开发插件覆盖相同身份的已安装版本；禁用包只为静态扫描提供声明。
        let mut plugins = self
            .snapshot
            .plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), Arc::new(plugin.manifest.clone())))
            .collect::<BTreeMap<_, _>>();
        plugins.extend(
            self.enabled
                .iter()
                .map(|plugin| (plugin.id.clone(), plugin.clone())),
        );
        // 冻结失败保留错误而不退回全启用：把用户停用的单元静默启用等于越权。
        let builtin_states = self.builtin_states()?;
        let (registry, hosts) = crate::plugin_catalogs::catalogs_for_plugins(
            &plugins.into_values().collect::<Vec<_>>(),
            &self.enabled,
            locale,
            &builtin_states,
        )?;
        self.store
            .mark_boot_phase(&self.snapshot.boot_id, PluginBootPhase::RegistryFrozen)
            .map_err(|source| {
                AppError::infrastructure_with_source("保存插件冻结标记失败", source)
            })?;
        Ok((registry, hosts))
    }

    /// 调用前必须取得主实例身份；实际安全模式也冻结开发入口，防止崩溃恢复又加载同一开发插件。
    /// 失败保留状态文件，不能退回空目录继续写入。
    pub fn load(root: &Path, mut options: PluginStartupOptions) -> AppResult<Self> {
        let store = PluginStore::new(StorageCoordinator::new(root))
            .map_err(|source| AppError::infrastructure_with_source("初始化插件存储失败", source))?;
        let validator = ProtocolBootValidator {
            host_api: HOST_API.into(),
            platform: current_platform()?,
        };
        let snapshot = store
            .begin_startup(&validator, options.safe_mode)
            .map_err(|source| AppError::infrastructure_with_source("插件启动事务失败", source))?;
        if snapshot.safe_mode {
            options.safe_mode = true;
            options.dev_plugin = None;
        }
        let mut enabled = snapshot
            .enabled_plugins()
            .map(|plugin| (plugin.id.clone(), Arc::new(plugin.manifest.clone())))
            .collect::<BTreeMap<_, _>>();
        if let Some(directory) = &options.dev_plugin {
            let bytes =
                std::fs::read(directory.join("kerminal-plugin.json")).map_err(|source| {
                    AppError::infrastructure_with_source("读取开发插件清单失败", source)
                })?;
            // 开发目录由用户显式选择，允许调试官方命名空间；它不会获得市场来源或写入 state。
            let manifest = kerminal_plugin_protocol::parse_manifest(
                &bytes,
                kerminal_plugin_protocol::ManifestPolicy::Official,
            )
            .map_err(|source| AppError::infrastructure_with_source("开发插件清单无效", source))?;
            let id = PluginId::parse(manifest.id).map_err(|source| {
                AppError::infrastructure_with_source("开发插件身份无效", source)
            })?;
            let version = PluginVersion::parse(&manifest.version).map_err(|source| {
                AppError::infrastructure_with_source("开发插件版本无效", source)
            })?;
            let prepared = validator
                .validate(directory, &id, &version, PluginSource::Official)
                .map_err(|_| AppError::invalid("开发插件包、平台或 API 不兼容"))?;
            enabled.insert(id, Arc::new(prepared));
        }
        Ok(Self {
            store,
            snapshot,
            enabled: enabled.into_values().collect(),
            options,
        })
    }
}

/// 平台身份与包协议共用枚举，未知构建目标明确拒绝而不选择相近平台资产。
pub fn current_platform() -> AppResult<Platform> {
    Platform::current().ok_or_else(|| AppError::unsupported("当前平台不支持独立插件"))
}

#[cfg(test)]
#[path = "../../tests/support/plugin_startup_unit.rs"]
mod tests;
