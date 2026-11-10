//! 内容和工具工厂的组合校验，不创建第二套运行服务。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, sync::Arc};

use kerminal_application::{
    AppError, AppResult, plugins::PluginRegistry, preferences::ToolRailToolId,
};
use kerminal_ui::plugins::{content::ContentFactory, tools::ToolFactory};

/// 工厂身份必须有声明且不能重复，以免不同实现覆盖同一内容。
///
/// @author kongweiguang
pub(crate) fn validate_content_factories(
    plugins: &PluginRegistry,
    factories: &[Arc<dyn ContentFactory>],
) -> AppResult<()> {
    let mut ids = BTreeSet::new();
    for factory in factories {
        let id = factory.content_id();
        if plugins.content_type(id).is_none() {
            return Err(AppError::invalid("内容工厂缺少对应插件声明"));
        }
        if !ids.insert(id) {
            return Err(AppError::conflict("内容工厂身份重复"));
        }
    }
    Ok(())
}

/// 工具必须对应唯一的扩展贡献，不能覆盖由宿主直接管理的内置工具。
///
/// @author kongweiguang
pub(super) fn validate_tool_factories(
    plugins: &PluginRegistry,
    factories: &[Arc<dyn ToolFactory>],
) -> AppResult<()> {
    let mut ids = BTreeSet::new();
    for factory in factories {
        let id = factory.tool_id();
        let tool = ToolRailToolId::from_id(id)
            .map_err(|source| AppError::infrastructure_with_source("工具工厂标识无效", source))?;
        if !matches!(tool, ToolRailToolId::Plugin(_)) || plugins.tool(id).is_none() {
            return Err(AppError::invalid("工具工厂缺少对应扩展声明"));
        }
        if !ids.insert(id) {
            return Err(AppError::conflict("工具工厂身份重复"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/support/builtin_plugins_unit.rs"]
mod tests;
