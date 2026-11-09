//! 桌面静态清单与实际 codec 的启动前一致性测试。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{App, Window};
use kerminal_application::{
    AppError, AppResult, connections::host_types::HostConfigurationCatalog, plugins::PluginRegistry,
};
use kerminal_infrastructure::configuration::host_configuration_catalog;
use kerminal_ui::plugins::{
    content::{ContentFactory, ContentInstance, ContentKind, ContentSavedState, ContentScope},
    tools::{ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope},
};

use super::{validate_content_factories, validate_tool_factories};
use crate::plugin_catalogs::{builtin_catalogs, validate_host_catalog};

struct FixtureFactory(&'static str);

impl ContentFactory for FixtureFactory {
    /// 返回固定测试身份，避免此检查依赖真实窗口或网络资源。
    ///
    /// @author kongweiguang
    fn content_id(&self) -> &str {
        self.0
    }

    /// 校验测试不创建实例，作用域能力与身份校验无关。
    ///
    /// @author kongweiguang
    fn supports(&self, _kind: ContentKind) -> bool {
        true
    }

    /// 此夹具只验证装配门禁，意外调用时明确报错而不伪造实例。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _scope: &ContentScope,
        _state: Option<&ContentSavedState>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        Err(AppError::unsupported("测试工厂不创建视图"))
    }
}

struct FixtureToolFactory(&'static str);

impl ToolFactory for FixtureToolFactory {
    /// 装配测试只需要稳定身份，不创建窗口或资源。
    ///
    /// @author kongweiguang
    fn tool_id(&self) -> &str {
        self.0
    }

    /// 显示元数据与身份校验正交，此值仅满足真实工厂契约。
    ///
    /// @author kongweiguang
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new("Fixture", "Fixture", gpui_kit::assets::IconName::Puzzle)
    }

    /// 用窗口作用域避免测试依赖活动标签。
    ///
    /// @author kongweiguang
    fn scope(&self) -> ToolScope {
        ToolScope::Window
    }

    /// 若测试意外创建视图则明确失败，避免将空视图视为成功。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        _: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        Err(AppError::unsupported("测试工厂不创建视图"))
    }
}

/// 已声明扩展只接受一次工厂，未知身份及内置工具不能被扩展覆盖。
///
/// @author kongweiguang
#[test]
fn tool_factories_must_match_unique_extension_declarations() {
    use kerminal_application::plugins::{
        PluginDescriptor, PluginId, PluginRegistryBuilder, ToolDescriptor, ToolId,
    };
    let provider = PluginId::new("test.provider").unwrap();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().unwrap();
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Test", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new("test.tool").unwrap(),
        provider,
        "Test",
        None,
    ));
    let plugins = builder.freeze().unwrap();
    let tool: Arc<dyn ToolFactory> = Arc::new(FixtureToolFactory("test.tool"));
    assert!(validate_tool_factories(&plugins, std::slice::from_ref(&tool)).is_ok());
    assert!(validate_tool_factories(&plugins, &[tool.clone(), tool]).is_err());
    assert!(
        validate_tool_factories(&plugins, &[Arc::new(FixtureToolFactory("test.missing"))]).is_err()
    );
    assert!(validate_tool_factories(&plugins, &[Arc::new(FixtureToolFactory("sftp"))]).is_err());
}

/// 生产内置描述必须全部指向实际内置 codec。
///
/// @author kongweiguang
#[test]
fn builtin_catalogs_match_both_directions() {
    let (plugins, hosts) = builtin_catalogs().expect("builtins have matching codecs");
    assert_eq!(plugins.host_types().len(), hosts.iter().count());
}

/// 缺少任何已声明 codec 时拒绝进入扫描，而非运行中才发现不可用。
///
/// @author kongweiguang
#[test]
fn rejects_declared_host_without_codec() {
    let plugins = PluginRegistry::builtin().expect("valid builtins");
    assert!(validate_host_catalog(&plugins, &HostConfigurationCatalog::default()).is_err());
}

/// 未声明的扩展 codec 不能绕过插件身份校验进入配置扫描。
///
/// @author kongweiguang
#[test]
fn rejects_codec_without_declared_host() {
    let plugins = PluginRegistry::empty();
    let hosts = host_configuration_catalog(Vec::new()).expect("valid codecs");
    assert!(validate_host_catalog(&plugins, &hosts).is_err());
}

/// 已声明身份可装配一次；缺失声明或重复工厂都在启动阶段拒绝。
///
/// @author kongweiguang
#[test]
fn content_factories_must_match_unique_declarations() {
    let plugins = PluginRegistry::builtin().expect("valid builtins");
    let factory: Arc<dyn ContentFactory> = Arc::new(FixtureFactory("terminal"));
    assert!(validate_content_factories(&plugins, std::slice::from_ref(&factory)).is_ok());
    assert!(validate_content_factories(&plugins, &[factory.clone(), factory]).is_err());
    let missing: Arc<dyn ContentFactory> = Arc::new(FixtureFactory("test.missing"));
    assert!(validate_content_factories(&plugins, &[missing]).is_err());
}
