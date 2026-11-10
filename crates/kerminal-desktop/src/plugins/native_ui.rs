//! 将冻结的原生视图贡献装配到现有窗口宿主。
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::AppResult;
use kerminal_ui::{
    AppServices,
    plugins::{
        content::ContentFactory,
        external::factories::{ExternalContentFactory, ExternalToolFactory, ExternalViewServices},
        external_host_form::ExternalHostFormFactory,
        host_forms::{HostFormFactories, HostFormFactory},
        tools::ToolFactory,
    },
};

use super::assembly::DesktopPlugins;

/// 先构造全部工厂再一次性交给 UI；失败时不能留下半套注册。
pub fn install(
    services: &mut AppServices,
    plugins: &DesktopPlugins,
    locale: &str,
) -> AppResult<()> {
    let views = ExternalViewServices::new(
        plugins.views.clone(),
        services.executor.clone(),
        locale.to_owned(),
    )
    .with_ui_bridge(plugins.ui.clone());
    let mut tools: Vec<Arc<dyn ToolFactory>> = Vec::new();
    let mut contents: Vec<Arc<dyn ContentFactory>> = Vec::new();
    let mut host_forms: Vec<Arc<dyn HostFormFactory>> =
        services.host_form_factories.iter().cloned().collect();
    for plugin in &plugins.startup.enabled {
        for contribution in &plugin.package.manifest.contributes.host_types {
            host_forms.push(Arc::new(ExternalHostFormFactory::new(
                plugin.clone(),
                &contribution.id,
                plugins.vault.clone(),
                plugins.runtime.clone(),
                locale,
            )?));
        }
        for contribution in &plugin.package.manifest.contributes.tools {
            if plugin
                .package
                .manifest
                .views
                .contains_key(&contribution.view)
            {
                tools.push(Arc::new(ExternalToolFactory::new(
                    plugin.clone(),
                    contribution.clone(),
                    views.clone(),
                )?));
            }
        }
        for contribution in &plugin.package.manifest.contributes.tabs {
            if plugin
                .package
                .manifest
                .views
                .contains_key(&contribution.view)
            {
                contents.push(Arc::new(ExternalContentFactory::new(
                    plugin.clone(),
                    contribution.clone(),
                    views.clone(),
                )?));
            }
        }
    }
    let host_forms = Arc::new(HostFormFactories::new(&plugins.host_types, host_forms)?);
    services.host_form_factories = host_forms;
    services.tool_factories.extend(tools);
    services.content_factories.extend(contents);
    services.external_plugin_views = Some(views);
    Ok(())
}
