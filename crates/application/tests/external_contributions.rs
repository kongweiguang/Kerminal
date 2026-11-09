//! 校验外部声明映射和注册前的一致性，不启动插件进程或桌面。
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use kerminal_application::{
    external_contributions::{
        ExternalContributionError, PreparedPlugin, register_external_plugins,
    },
    plugins::{
        PluginDescriptor, PluginId as RegistryPluginId, PluginRegistryBuilder, PluginRegistryError,
    },
};
use kerminal_domain::plugins::{PluginId, PluginVersion};
use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
use serde_json::json;

/// 从真实协议解析清单，单独构造资源快照以聚焦注册行为。
fn prepared_plugin() -> PreparedPlugin {
    let document = json!({
        "schema":1,"id":"acme.notes","name":{"en":"Notes","zh-CN":"笔记"},
        "description":{"en":"Notes"},"version":"1.2.0","api":"^1.0",
        "publisher":"acme","runtime":{"kind":"none"},
        "l10n":"l10n",
        "views":{"main":{"kind":"native","file":"views/main.json"}},
        "contributes":{
            "tools":[{"id":"panel","title":"%panel%","view":"main","scope":"window"}],
            "tabs":[{"id":"editor","title":"Editor","view":"main","state_version":1}],
            "host_types":[{"id":"server","title":"Server","schema":"schema.json"}],
            "settings":{"schema":"schema.json"},
            "commands":[{"id":"open","title":"%command%","keybinding":"Ctrl+Alt+N"}]
        }
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&document).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    PreparedPlugin {
        id: PluginId::parse("acme.notes").unwrap(),
        version: PluginVersion::parse("1.2.0").unwrap(),
        directory: PathBuf::from("unused"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::from([
            (
                "en".into(),
                BTreeMap::from([
                    ("panel".into(), "Panel".into()),
                    ("command".into(), "Open".into()),
                ]),
            ),
            (
                "zh-CN".into(),
                BTreeMap::from([
                    ("panel".into(), "面板".into()),
                    ("command".into(), " ".into()),
                ]),
            ),
        ]),
        resources: BTreeMap::new(),
    }
}

#[test]
/// 贡献身份包含插件命名空间，空白或缺失的本地翻译使用英文，设置和命令仍可取得。
fn registers_namespaced_declarations_and_localized_names() {
    let plugin = prepared_plugin();
    let mut builder = PluginRegistryBuilder::new();
    let declarations =
        register_external_plugins(&mut builder, [&plugin], "1.4.0", "zh-CN").unwrap();
    assert_eq!(
        declarations[0].settings.as_ref().unwrap().schema,
        "schema.json"
    );
    assert_eq!(declarations[0].commands[0].id, "acme.notes/open");
    assert_eq!(declarations[0].commands[0].title, "Open");
    let catalog = builder.freeze().unwrap();
    assert_eq!(catalog.provider("acme.notes").unwrap().name, "笔记");
    assert_eq!(catalog.tool("acme.notes/panel").unwrap().name, "面板");
    assert!(catalog.content_type("acme.notes/editor").is_some());
    assert_eq!(
        catalog
            .host_type("acme.notes/server")
            .unwrap()
            .configuration_type,
        "acme.notes/server"
    );
    let mut fallback_builder = PluginRegistryBuilder::new();
    let fallback =
        register_external_plugins(&mut fallback_builder, [&plugin], "1.4.0", "fr-FR").unwrap();
    assert_eq!(fallback[0].tools[0].name, "Panel");
}

#[test]
/// 同批次重复身份或不兼容 API 不得留下前半批已经注册的贡献。
fn rejects_duplicate_or_incompatible_batch_without_partial_mutation() {
    let plugin = prepared_plugin();
    let mut builder = PluginRegistryBuilder::new();
    assert!(matches!(
        register_external_plugins(&mut builder, [&plugin, &plugin], "1.0.0", "en"),
        Err(ExternalContributionError::DuplicatePlugin(_))
    ));
    assert!(builder.freeze().unwrap().providers().is_empty());
    let mut builder = PluginRegistryBuilder::new();
    assert!(matches!(
        register_external_plugins(&mut builder, [&plugin], "2.0.0", "en"),
        Err(ExternalContributionError::Incompatible(_))
    ));
    assert!(builder.freeze().unwrap().providers().is_empty());
}

#[test]
/// 注册器冻结时仍统一检查内置及外部来源的冲突，避免按注册顺序覆盖。
fn freeze_rejects_collision_with_existing_provider() {
    let plugin = prepared_plugin();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(PluginDescriptor::new(
        RegistryPluginId::new("acme.notes").unwrap(),
        "other",
        true,
    ));
    register_external_plugins(&mut builder, [&plugin], "1.0.0", "en").unwrap();
    assert!(matches!(
        builder.freeze(),
        Err(PluginRegistryError::DuplicateId(_))
    ));
}
