//! 覆盖独立插件契约边界，不启动桌面或真实插件进程。
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_plugin_protocol::*;
use serde_json::{Value, json};

const VALID: &[u8] = include_bytes!("../../../tests/fixtures/plugins/manifests/valid.json");
const UNKNOWN: &[u8] =
    include_bytes!("../../../tests/fixtures/plugins/manifests/unknown-field.json");
const RESERVED: &[u8] =
    include_bytes!("../../../tests/fixtures/plugins/manifests/reserved-namespace.json");
const TRAVERSAL: &[u8] =
    include_bytes!("../../../tests/fixtures/plugins/manifests/path-traversal.json");
const DEEP_VIEW: &[u8] =
    include_bytes!("../../../tests/fixtures/plugins/manifests/view-too-deep.json");

/// 每项变更都从真实样本出发，避免无效样本因其他字段提前失败。
fn valid_manifest() -> Value {
    serde_json::from_slice(VALID).unwrap()
}

/// 统一从字节调用公开入口，覆盖与安装器相同的解析路径。
fn parse(value: &Value) -> Result<PluginManifest, ProtocolError> {
    parse_manifest(
        &serde_json::to_vec(value).unwrap(),
        ManifestPolicy::ThirdParty,
    )
}

/// 模拟包内普通文件，不依赖宿主配置目录。
fn write_file(root: &Path, path: &str, bytes: &[u8]) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, bytes).unwrap();
}

#[test]
/// 保留命名空间由已验证来源决定，普通本地包不能自称官方。
fn manifest_fixtures_enforce_identity_and_unknown_fields() {
    assert!(parse_manifest(VALID, ManifestPolicy::ThirdParty).is_ok());
    assert!(parse_manifest(UNKNOWN, ManifestPolicy::ThirdParty).is_err());
    assert!(parse_manifest(RESERVED, ManifestPolicy::ThirdParty).is_err());
    assert!(parse_manifest(RESERVED, ManifestPolicy::Official).is_ok());
    assert!(parse_manifest(TRAVERSAL, ManifestPolicy::ThirdParty).is_err());
    let mut value = valid_manifest();
    value["x-acme"] = json!({"kept": true});
    value["runtime"]["x-note"] = json!(1);
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.extensions["x-acme"], json!({"kept":true}));
    value["runtime"]["surprise"] = json!(1);
    assert!(parse(&value).is_err());
}

#[test]
/// 身份、英文回退、SemVer 与 npm 范围各自在边界拒绝无效值。
fn manifest_fields_validate_independently() {
    for (field, invalid_value) in [
        ("schema", json!(2)),
        ("id", json!("a.notes")),
        ("id", json!("Acme.notes")),
        ("id", json!("acme.notes.extra")),
        ("publisher", json!("another")),
        ("name", json!({"zh-CN":"笔记"})),
        ("description", json!({"en":""})),
        ("version", json!("1.2")),
        ("version", json!("01.2.3")),
        ("api", json!("not a range")),
        ("categories", json!(["bad/category"])),
    ] {
        let mut value = valid_manifest();
        value[field] = invalid_value;
        assert!(parse(&value).is_err(), "field {field}");
    }
    let mut value = valid_manifest();
    value["api"] = json!("^1.0 || >=2.1 <3");
    let parsed = parse(&value).unwrap();
    assert!(parsed.supports_host_api("1.9.0").unwrap());
    assert!(parsed.supports_host_api("2.2.0").unwrap());
    assert!(!parsed.supports_host_api("2.0.0").unwrap());
    assert!(!parsed.supports_host_api("3.0.0").unwrap());
}

#[test]
/// 旧权限字符串仍可解析，但不再决定运行形态或授予任何网络能力。
fn legacy_permission_strings_parse_without_grants() {
    let mut value = valid_manifest();
    value["runtime"] = json!({"kind":"script","main":"dist/main.js","activation":"startup"});
    assert!(parse(&value).is_err());
    value["runtime"] = json!({"kind":"none"});
    value["permissions"] = json!([
        "events.subscribe",
        "network:*.example.com",
        "network:*",
        "unknown",
        "unknown"
    ]);
    let parsed = parse(&value).expect("descriptive permissions");
    assert_eq!(
        parsed.permissions,
        [
            "events.subscribe",
            "network:*.example.com",
            "network:*",
            "unknown",
            "unknown"
        ]
    );
    // 完全访问不再限定 Sidecar：没有运行时也可以声明，且不产生任何授权。
    value["permissions"] = json!(["full-access"]);
    assert!(parse(&value).is_ok());
    value["runtime"] =
        json!({"kind":"sidecar","main":{"windows-x86_64":"bin/app.exe"},"activation":"startup"});
    value["permissions"] = json!([]);
    assert!(parse(&value).is_ok());
    value["runtime"]["main"] = json!({"any":"bin/app"});
    assert!(parse(&value).is_err());
}

#[test]
/// 协议 v1 只承认 none 与 sidecar；旧 script 清单必须在解析边界拒绝，不能悄悄按无进程插件运行。
fn script_runtime_is_rejected() {
    let mut value = valid_manifest();
    value["runtime"] = json!({"kind":"script","main":"dist/main.js"});
    assert!(parse(&value).is_err());
    let schema = schema::manifest_json_schema();
    assert!(!jsonschema::validator_for(&schema).unwrap().is_valid(&value));
}

#[test]
/// 旧清单中的 network 与 csp 可以继续解析，但不会授予联网或改变视图语义。
fn legacy_permissions_and_csp_parse_without_effect() {
    let mut value = valid_manifest();
    value["permissions"] = json!(["ui.notify", "network:example.com"]);
    value["views"] = json!({
        "page": {"kind":"webview","entry":"web/index.html","csp":"https://example.com"}
    });
    value["contributes"] = json!({
        "commands": [{"id":"refresh","title":"Refresh","keybinding":"ctrl+r"}],
        "settings": {"schema":"schemas/settings.json"},
        "tools": [{"id":"panel","title":"Panel","view":"page","scope":"tab"}]
    });
    let parsed = parse(&value).expect("legacy manifest");
    assert_eq!(parsed.permissions, ["ui.notify", "network:example.com"]);
    assert!(matches!(
        parsed.views.get("page"),
        Some(ViewDefinition::Webview { .. })
    ));
}

#[test]
/// 重复键不能让不同实现选择不同的 ID 或权限。
fn duplicate_json_keys_are_rejected() {
    let duplicate = br#"{"schema":1,"id":"acme.notes","id":"evil.notes","name":{"en":"Notes"},"description":{"en":"Notes"},"version":"1.2.0","api":"^1.0","publisher":"acme","runtime":{"kind":"none"}}"#;
    assert!(parse_manifest(duplicate, ManifestPolicy::ThirdParty).is_err());
}

#[test]
/// 包内路径允许 UTF-8 文件名，但不能越界或形成 Windows 设备别名。
fn package_paths_are_portable() {
    for valid in ["views/面板.json", "assets/icon.svg", "bin/console.exe"] {
        assert!(validate_package_path(valid).is_ok(), "{valid}");
    }
    for bad in [
        "",
        "/absolute",
        "../secret",
        "a/../b",
        "a//b",
        "a\\b",
        "C:/test",
        "bin/CON.txt",
        "aux",
        "COM¹.txt",
        "folder/file.",
        "folder/file ",
    ] {
        assert!(validate_package_path(bad).is_err(), "{bad}");
    }
}

#[test]
/// 贡献共享局部命名空间，设置视图和主机表单不能使用 WebView。
fn contribution_references_and_ids_are_checked() {
    let mut value = valid_manifest();
    value["views"] = json!({"panel":{"kind":"native","file":"views/panel.json"}, "web":{"kind":"webview","entry":"web/index.html"}});
    value["contributes"]["tools"] =
        json!([{"id":"panel","title":"Panel","view":"panel","scope":"tab"}]);
    value["contributes"]["tabs"] =
        json!([{"id":"panel","title":"Tab","view":"web","state_version":1}]);
    assert!(parse(&value).is_err());
    value["contributes"]["tabs"][0]["id"] = json!("editor");
    assert!(parse(&value).is_ok());
    value["contributes"]["settings"] = json!({"schema":"schemas/settings.json","view":"web"});
    assert!(parse(&value).is_err());
    value["contributes"]["settings"]["view"] = json!("panel");
    assert!(parse(&value).is_ok());
    value["contributes"]["tabs"][0]["id"] = json!("a".repeat(64));
    assert!(parse(&value).is_err());
}

#[test]
/// 示例中的工具栏、只读绑定、菜单与空态可按原文通过解析。
fn documented_native_view_is_accepted() {
    let view = json!({"schema":1,"data":{"query":"","items":[],"busy":false},"root":{"type":"stack","gap":"md","children":[
        {"type":"toolbar","children":[{"type":"input","bind":"query","placeholder":"%search%","on_submit":"search"},
        {"type":"button","icon":"refresh","action":"refresh","busy":"$busy"}]},
        {"type":"list","items":"$items","key":"id","title":"title","subtitle":"path","on_activate":"open",
        "menu":[{"label":"%delete%","action":"delete","danger":true}],"empty":{"icon":"file","title":"%empty%"}}
    ]}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok());
}

#[test]
/// 所有 v1 组件分别接受一条有效声明，避免遗漏某个 UI 类别。
fn all_native_node_kinds_parse() {
    let nodes = vec![
        json!({"type":"stack","children":[]}),
        json!({"type":"row","children":[]}),
        json!({"type":"section","title":"Group","children":[]}),
        json!({"type":"scroll","child":{"type":"text","text":"x"}}),
        json!({"type":"split","children":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}),
        json!({"type":"tabs","tabs":[{"id":"one","title":"One","content":{"type":"text","text":"x"}}]}),
        json!({"type":"text","text":"x","tone":"muted"}),
        json!({"type":"markdown","text":"x"}),
        json!({"type":"badge","text":"x"}),
        json!({"type":"icon","icon":"file"}),
        json!({"type":"code","code":"x"}),
        json!({"type":"progress","value":1}),
        json!({"type":"qrcode","matrix":"$qr","label":"%scan%"}),
        json!({"type":"empty","title":"None"}),
        json!({"type":"notice","text":"x"}),
        json!({"type":"input","bind":"form.name"}),
        json!({"type":"textarea","bind":"form.name"}),
        json!({"type":"number","bind":"form.number"}),
        json!({"type":"select","bind":"form.option","options":[{"label":"A","value":"a"}]}),
        json!({"type":"switch","bind":"form.flag"}),
        json!({"type":"checkbox","bind":"form.flag"}),
        json!({"type":"path","bind":"form.path"}),
        json!({"type":"path","bind":"form.directory","mode":"directory"}),
        json!({"type":"secret","bind":"form.secret"}),
        json!({"type":"button","label":"Run","action":"run","variant":"primary"}),
        json!({"type":"toolbar","children":[]}),
        json!({"type":"menu","items":[{"label":"Run","action":"run"}]}),
        json!({"type":"list","items":"$rows","key":"id","title":"name"}),
        json!({"type":"table","items":"$rows","key":"id","columns":[{"key":"name","title":"Name"}]}),
        json!({"type":"tree","id":"tree","source":"rpc","key":"id","title":"name"}),
        json!({"type":"form","schema":"schemas/settings.json"}),
    ];
    for node in nodes {
        let kind = node["type"].clone();
        let view = json!({"schema":1,"root":node});
        assert!(
            parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok(),
            "node {kind}"
        );
    }
}

#[test]
/// 二维码矩阵只接受不含静区的 21+4n 方阵，目录选择模式不改变既有 open/save 契约。
fn qrcode_matrix_and_directory_picker_follow_contract() {
    let matrix = |side: usize| {
        (0..side)
            .map(|row| {
                (0..side)
                    .map(|column| if (row + column) % 3 == 0 { '1' } else { '0' })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for side in [21, 25, 177] {
        assert!(validate_qr_matrix(&matrix(side)).is_ok(), "side {side}");
    }
    for side in [19, 20, 22, 23, 178, 181] {
        assert!(validate_qr_matrix(&matrix(side)).is_err(), "side {side}");
    }
    let valid = matrix(21);
    assert!(validate_qr_matrix(&valid.replace("1", "2")).is_err());
    assert!(validate_qr_matrix(&(valid.clone() + "\n" + &valid)).is_err());
    assert!(validate_qr_matrix(&format!("{valid}\n{}", "0".repeat(20))).is_err());
    assert!(validate_qr_matrix("").is_err());

    let view = json!({"schema":1,"data":{"qr":valid},"root":{"type":"qrcode","matrix":"$qr"}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok());
    for matrix in ["qr", "$"] {
        let view = json!({"schema":1,"root":{"type":"qrcode","matrix":matrix}});
        assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err());
    }
    for mode in ["open", "save", "directory"] {
        let view = json!({"schema":1,"root":{"type":"path","bind":"form.path","mode":mode}});
        assert!(
            parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok(),
            "mode {mode}"
        );
    }
    let view = json!({"schema":1,"root":{"type":"path","bind":"form.path","mode":"folder"}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err());
}

#[test]
/// 树限制、未知样式和非法绑定在渲染前被拒绝。
fn native_view_rejects_unsafe_or_oversized_structures() {
    assert!(parse_native_view(DEEP_VIEW).is_err());
    let children = (0..512)
        .map(|_| json!({"type":"text","text":"x"}))
        .collect::<Vec<_>>();
    let view = json!({"schema":1,"root":{"type":"stack","children":children}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err());
    for node in [
        json!({"type":"text","text":"x","color":"red"}),
        json!({"type":"text","text":"x","tone":"bright"}),
        json!({"type":"input","bind":"$form.name"}),
        json!({"type":"text","text":"x","visible":"form.visible"}),
        json!({"type":"list","source":"rpc","key":"id","title":"name"}),
    ] {
        let view = json!({"schema":1,"root":node});
        assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err());
    }
    let view = json!({"schema":1,"data":{"rows":vec![json!({});501]},"root":{"type":"list","items":"$rows","key":"id","title":"name"}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err());
}

#[test]
/// 配置校验使用标准 JSON Schema，并禁止读取远程或本地引用。
fn configuration_schema_is_offline_and_checks_values() {
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"port":{"type":"integer","minimum":1,"maximum":65535},"token":{"type":"string","x-secret":true}},"required":["port"]}"#).unwrap();
    assert!(schema.validate(&json!({"port":22})).is_ok());
    assert!(schema.validate(&json!({"port":0})).is_err());
    assert!(ConfigurationSchema::parse(br#"{"$ref":"https://example.com/schema"}"#).is_err());
    assert!(ConfigurationSchema::parse(br#"{"properties":{"x":{"x-secret":"yes"}}}"#).is_err());
}

#[test]
/// 包清点与引用检查共用实际文件，缺失资源不能被描述性清单掩盖。
fn package_checks_real_files_and_platform() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut manifest = valid_manifest();
    manifest["runtime"] = json!({"kind":"sidecar","main":{"windows-x86_64":"bin/app.exe"}});
    write_file(
        root,
        MANIFEST_FILE_NAME,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());
    write_file(root, "bin/app.exe", b"sidecar");
    let package = validate_package(root, ManifestPolicy::ThirdParty).unwrap();
    assert!(
        package
            .validate_for_platform(Platform::WindowsX86_64)
            .is_ok()
    );
    assert!(package.validate_for_platform(Platform::Any).is_err());
}

#[test]
/// 硬链接能绕过目录边界，因此即使文件内容正常也必须拒绝。
fn package_rejects_hard_links() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_file(root, MANIFEST_FILE_NAME, VALID);
    write_file(root, "asset.txt", b"asset");
    fs::hard_link(root.join("asset.txt"), root.join("alias.txt")).unwrap();
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());
}

#[test]
/// JSON-RPC 双向请求 ID 分离，null 成功结果不会变成缺失结果。
fn rpc_envelopes_handshake_and_permission_mapping() {
    let request = RpcRequest::new("h:1", "initialize", json!({}));
    assert!(request.validate(RpcPeer::Host).is_ok());
    assert!(request.validate(RpcPeer::Plugin).is_err());
    let response: RpcResponse =
        serde_json::from_value(json!({"jsonrpc":"2.0","id":"h:1","result":null})).unwrap();
    assert!(response.validate().is_ok());
    assert_eq!(
        serde_json::to_value(&response).unwrap()["result"],
        Value::Null
    );
    assert!(
        serde_json::from_value::<RpcResponse>(
            json!({"jsonrpc":"2.0","error":{"code":-32700,"message":"bad"}})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<RpcResponse>(
            json!({"jsonrpc":"2.0","id":"h:1","result":null,"error":null})
        )
        .is_err()
    );
    assert!(host_method_supported("ui.pickFile").is_ok());
    assert!(host_method_supported("unexpected.method").is_err());
    let request = InitializeParams {
        protocol: 1,
        host_api: "1.0".into(),
        plugin: PluginIdentity {
            id: "acme.notes".into(),
            version: "1.2.0".into(),
        },
        locale: "en".into(),
        theme: "dark".into(),
        data_dir: "data".into(),
        generation: 1,
    };
    assert!(
        InitializeResult {
            protocol: 1,
            plugin: request.plugin.clone()
        }
        .validate_for(&request)
        .is_ok()
    );
    assert!(
        InitializeResult {
            protocol: 2,
            plugin: request.plugin.clone()
        }
        .validate_for(&request)
        .is_err()
    );
}

#[test]
/// 编辑器 schema 必须与安装器一样接受 x- 扩展并拒绝未知字段。
fn generated_manifest_schema_matches_parser_extensions() {
    let schema = schema::manifest_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut manifest = valid_manifest();
    manifest["x-test"] = json!(true);
    assert!(validator.is_valid(&manifest));
    manifest["mystery"] = json!(true);
    assert!(!validator.is_valid(&manifest));
}

#[test]
/// 视图通用字段与每个节点分支一起生效，schema 不能误拒合法的 ID 和可见性。
fn generated_view_schema_matches_flattened_node_fields() {
    let schema = schema::native_view_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut view = json!({"schema":1,"root":{"type":"text","id":"label","visible":"$shown","text":"Label","x-note":true}});
    assert!(parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok());
    assert!(validator.is_valid(&view));
    view["root"]["color"] = json!("red");
    assert!(!validator.is_valid(&view));
}

#[test]
/// SDK 可以从同一份 RPC schema 找到握手和视图消息，信封不能包含两个结果。
fn generated_rpc_schema_includes_payload_types_and_exact_outcomes() {
    let schema = schema::rpc_json_schema();
    assert!(schema.pointer("/$defs/InitializeParams").is_some());
    assert!(schema.pointer("/$defs/ViewActionParams").is_some());
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&json!({"jsonrpc":"2.0","id":"h:1","result":null})));
    assert!(!validator.is_valid(
        &json!({"jsonrpc":"2.0","id":"h:1","result":null,"error":{"code":1,"message":"bad"}})
    ));
    assert!(!validator.is_valid(&json!({"jsonrpc":"1.0","id":"h:1","result":null})));
}

#[test]
/// 仓库中的 schema 必须与 Rust 类型生成结果一致，避免 SDK 使用过期契约。
fn checked_in_schemas_match_generated_contracts() {
    for (bytes, generated) in [
        (
            include_bytes!("../schema/manifest.schema.json").as_slice(),
            schema::manifest_json_schema(),
        ),
        (
            include_bytes!("../schema/view.schema.json").as_slice(),
            schema::native_view_json_schema(),
        ),
        (
            include_bytes!("../schema/rpc.schema.json").as_slice(),
            schema::rpc_json_schema(),
        ),
    ] {
        assert_eq!(serde_json::from_slice::<Value>(bytes).unwrap(), generated);
    }
}

#[test]
/// 内置图标名不要求包内文件，包内路径仍保持原有存在性校验。
fn icons_accept_builtin_names_without_package_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut manifest = valid_manifest();
    manifest["icon"] = json!("share-2");
    manifest["contributes"] = json!({"tools":[{
        "id": "tool",
        "title": "Tool",
        "view": "main",
        "scope": "window",
        "icon": "arrow-left-right"
    }]});
    manifest["views"] = json!({"main":{"kind":"native","file":"views/main.json"}});
    write_file(
        root,
        MANIFEST_FILE_NAME,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    write_file(
        root,
        "views/main.json",
        br#"{"schema":1,"root":{"type":"stack","children":[]}}"#,
    );
    validate_package(root, ManifestPolicy::ThirdParty).expect("内置图标名不应要求包内存在同名文件");
    assert!(
        jsonschema::validator_for(&schema::manifest_json_schema())
            .unwrap()
            .is_valid(&manifest),
        "静态 schema 必须与运行时接受同一份内置图标声明"
    );

    // 现有 `.svg` 声明继续走包内路径校验，缺失文件仍然失败。
    manifest["icon"] = json!("assets/icon.svg");
    write_file(
        root,
        MANIFEST_FILE_NAME,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());
    write_file(root, "assets/icon.svg", b"<svg/>");
    validate_package(root, ManifestPolicy::ThirdParty).expect("包内 SVG 存在时应保持原兼容行为");
}

#[test]
/// 引用只来自节点契约中的资源和展示文案，插件私有数据不得被当作引用解释。
fn package_references_ignore_extension_and_option_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut manifest = valid_manifest();
    manifest["views"] = json!({"main":{"kind":"native","file":"views/main.json"}});
    manifest["l10n"] = json!("l10n");
    write_file(
        root,
        MANIFEST_FILE_NAME,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    write_file(root, "icons/real.svg", b"<svg/>");
    write_file(root, "icons/empty.svg", b"<svg/>");
    write_file(root, "schemas/form.json", br#"{"type":"object"}"#);
    let translations = json!({"tab":"Tab", "label":"Label", "option":"Option", "menu":"Menu", "empty":"Empty", "column":"Column", "text":"Text", "placeholder":"Placeholder"});
    write_file(
        root,
        "l10n/en.json",
        &serde_json::to_vec(&translations).unwrap(),
    );
    let view = json!({"schema":1,"root":{"type":"stack", "x-meta":{"type":"form","schema":"ghost.json","icon":"ghost.svg","text":"%literal%"},"children":[
        {"type":"tabs","tabs":[{"id":"main","title":"%tab%","content":{"type":"scroll","child":{"type":"icon","icon":"icons/real.svg"}}}]},
        {"type":"form","schema":"schemas/form.json"},
        {"type":"select","bind":"choice","label":"%label%","options":[{"label":"%option%","value":{"type":"form","schema":"ghost.json","icon":"ghost.svg","text":"%literal%"},"x-meta":{"icon":"ghost.svg","text":"%literal%"}}]},
        {"type":"list","items":"$rows","key":"id","title":"name","menu":[{"label":"%menu%","action":"open"}],"empty":{"title":"%empty%","icon":"icons/empty.svg"}},
        {"type":"table","items":"$rows","key":"id","columns":[{"key":"name","title":"%column%"}]},
        {"type":"text","text":"%text%"},
        {"type":"input","bind":"query","placeholder":"%placeholder%"},
        {"type":"code","code":"%literal%"}
    ]}});
    write_file(root, "views/main.json", &serde_json::to_vec(&view).unwrap());
    validate_package(root, ManifestPolicy::ThirdParty).unwrap();

    for blank in ["", " \t "] {
        let mut empty_translation = translations.clone();
        empty_translation["tab"] = json!(blank);
        write_file(
            root,
            "l10n/en.json",
            &serde_json::to_vec(&empty_translation).unwrap(),
        );
        assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());
    }
    write_file(
        root,
        "l10n/en.json",
        &serde_json::to_vec(&translations).unwrap(),
    );

    let mut missing = view.clone();
    missing["root"]["children"][0]["tabs"][0]["content"]["child"]["icon"] =
        json!("icons/missing.svg");
    write_file(
        root,
        "views/main.json",
        &serde_json::to_vec(&missing).unwrap(),
    );
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());

    missing = view.clone();
    missing["root"]["children"][1]["schema"] = json!("schemas/missing.json");
    write_file(
        root,
        "views/main.json",
        &serde_json::to_vec(&missing).unwrap(),
    );
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());

    write_file(root, "views/main.json", &serde_json::to_vec(&view).unwrap());
    let mut incomplete = translations;
    incomplete.as_object_mut().unwrap().remove("column");
    write_file(
        root,
        "l10n/en.json",
        &serde_json::to_vec(&incomplete).unwrap(),
    );
    assert!(validate_package(root, ManifestPolicy::ThirdParty).is_err());
}

#[test]
/// 所有用户触发的动作都使用同一套身份规则，禁止让输入或集合绕过按钮约束。
fn all_native_action_fields_reject_invalid_identifiers() {
    for node in [
        json!({"type":"input","bind":"query","on_submit":"bad action"}),
        json!({"type":"list","items":"$rows","key":"id","title":"name","on_activate":"bad action"}),
        json!({"type":"tree","items":"$rows","key":"id","title":"name","on_activate":"bad action"}),
        json!({"type":"table","items":"$rows","key":"id","columns":[{"key":"name","title":"Name"}],"on_activate":"bad action"}),
        json!({"type":"button","label":"Run","action":"bad action"}),
        json!({"type":"menu","items":[{"label":"Run","action":"bad action"}]}),
    ] {
        let view = json!({"schema":1,"root":node});
        assert!(
            parse_native_view(&serde_json::to_vec(&view).unwrap()).is_err(),
            "{view}"
        );
    }
}

#[test]
/// 旧版依赖中的子 schema 与现代 dependentSchemas 一样禁止外部引用和非法私有标记。
fn configuration_validates_legacy_schema_dependencies() {
    let valid = json!({"$schema":"http://json-schema.org/draft-07/schema#","type":"object","dependencies":{"flag":["secret"],"secret":{"properties":{"secret":{"type":"string","x-secret":true}}}}});
    assert!(ConfigurationSchema::parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
    let mut invalid = valid.clone();
    invalid["dependencies"]["secret"]["properties"]["secret"]["x-secret"] = json!("yes");
    assert!(ConfigurationSchema::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
    invalid = valid;
    invalid["dependencies"]["secret"] = json!({"$ref":"https://example.com/schema.json"});
    assert!(ConfigurationSchema::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
/// 同一分页消息既支持原有调用，也携带树父键与表格排序且拒绝未知方向。
fn view_fetch_supports_tree_parent_and_table_sort() {
    let plain = json!({"instance":"i1","node":"items","range":[0,50]});
    let parsed: ViewFetchParams = serde_json::from_value(plain.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), plain);
    let extended = json!({"instance":"i1","node":"items","range":[0,50],"parent_key":"parent-1","sort":{"column":"name","direction":"asc"}});
    let parsed: ViewFetchParams = serde_json::from_value(extended.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), extended);
    let mut invalid = extended.clone();
    invalid["sort"]["direction"] = json!("random");
    assert!(serde_json::from_value::<ViewFetchParams>(invalid.clone()).is_err());
    let mut descending = extended.clone();
    descending["sort"]["direction"] = json!("desc");
    assert!(serde_json::from_value::<ViewFetchParams>(descending.clone()).is_ok());
    let schema = schema::rpc_json_schema();
    let validator = jsonschema::validator_for(
        &json!({"$defs":schema["$defs"],"$ref":"#/$defs/ViewFetchParams"}),
    )
    .unwrap();
    assert!(validator.is_valid(&plain));
    assert!(validator.is_valid(&extended));
    assert!(validator.is_valid(&descending));
    assert!(!validator.is_valid(&invalid));
}

#[test]
/// 生成的响应契约必须匹配解析与校验：错误不能为 null，所有响应都需要 id。
fn generated_rpc_response_schema_matches_runtime_null_and_id_rules() {
    let schema = schema::rpc_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for (response, expected) in [
        (json!({"jsonrpc":"2.0","id":"p:1","result":null}), true),
        (
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"bad"}}),
            true,
        ),
        (json!({"jsonrpc":"2.0","id":null,"error":null}), false),
        (
            json!({"jsonrpc":"2.0","error":{"code":-32600,"message":"bad"}}),
            false,
        ),
        (json!({"jsonrpc":"2.0","id":null,"result":null}), false),
    ] {
        let runtime_valid = serde_json::from_value::<RpcResponse>(response.clone())
            .ok()
            .is_some_and(|response| response.validate().is_ok());
        assert_eq!(runtime_valid, expected, "runtime: {response}");
        assert_eq!(
            validator.is_valid(&response),
            expected,
            "schema: {response}"
        );
    }
}

#[test]
/// 信封 Schema 和实际收发方向共享 ID、方法、参数的边界。
fn rpc_envelope_schema_matches_runtime_header_validation() {
    let schema = schema::rpc_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let cases = [
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"initialize"}),
            true,
        ),
        (
            json!({"jsonrpc":"2.0","id":"p:1","method":"initialize","params":{}}),
            true,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":[]}),
            true,
        ),
        (
            json!({"jsonrpc":"2.0","id":"q:1","method":"initialize","params":1}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"q:1","method":"initialize"}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:","method":"initialize"}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":format!("h:{}", "x".repeat(127)),"method":"initialize"}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:1\n","method":"initialize"}),
            false,
        ),
        (json!({"jsonrpc":"2.0","id":"h:1","method":""}), false),
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"x".repeat(129)}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"hello\u{85}"}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":1}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":null}),
            false,
        ),
    ];
    for (value, expected) in cases {
        let runtime_valid = serde_json::from_value::<RpcRequest>(value.clone())
            .ok()
            .is_some_and(|request| {
                request.validate(RpcPeer::Host).is_ok() || request.validate(RpcPeer::Plugin).is_ok()
            });
        assert_eq!(runtime_valid, expected, "runtime: {value}");
        assert_eq!(validator.is_valid(&value), expected, "schema: {value}");
    }
    for (value, expected) in [
        (
            json!({"jsonrpc":"2.0","method":"$/progress","params":{}}),
            true,
        ),
        (json!({"jsonrpc":"2.0","method":"$/progress"}), true),
        (json!({"jsonrpc":"2.0","method":""}), false),
        (
            json!({"jsonrpc":"2.0","method":"event","params":null}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","method":"event","params":"text"}),
            false,
        ),
    ] {
        let runtime_valid = serde_json::from_value::<RpcNotification>(value.clone())
            .ok()
            .is_some_and(|notification| notification.validate().is_ok());
        assert_eq!(runtime_valid, expected, "runtime: {value}");
        assert_eq!(validator.is_valid(&value), expected, "schema: {value}");
    }
    for (value, expected) in [
        (json!({"jsonrpc":"2.0","id":"p:1","result":null}), true),
        (
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"bad"}}),
            true,
        ),
        (json!({"jsonrpc":"2.0","id":"q:1","result":null}), false),
        (json!({"jsonrpc":"2.0","id":"h:","result":null}), false),
        (
            json!({"jsonrpc":"2.0","id":"p:1\n","error":{"code":-32600,"message":"bad"}}),
            false,
        ),
    ] {
        let runtime_valid = serde_json::from_value::<RpcResponse>(value.clone())
            .ok()
            .is_some_and(|response| response.validate().is_ok());
        assert_eq!(runtime_valid, expected, "runtime: {value}");
        assert_eq!(validator.is_valid(&value), expected, "schema: {value}");
    }
}

#[test]
/// 权限在 schema 中只是字符串数组；旧网络写法、未知值和重复项都不再被拒绝。
fn permissions_schema_is_descriptive_string_array() {
    let schema = schema::manifest_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut value = valid_manifest();
    for permissions in [
        json!([]),
        json!(["ui.notify"]),
        json!(["network:api.example.com", "network:*"]),
        json!(["unknown"]),
        json!(["ui.notify", "ui.notify"]),
        json!(["full-access"]),
    ] {
        value["permissions"] = permissions.clone();
        assert!(parse(&value).is_ok(), "runtime: {permissions}");
        assert!(validator.is_valid(&value), "schema: {permissions}");
    }
    value["permissions"] = json!([1]);
    assert!(parse(&value).is_err());
    assert!(!validator.is_valid(&value));
}

#[test]
/// 可在 Schema 中判断的节点形状和绑定规则与宿主解析保持一致。
fn native_view_schema_matches_local_runtime_constraints() {
    let schema = schema::native_view_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for (view, expected) in [
        (
            json!({"schema":1,"root":{"type":"button","label":"Run","action":"run"}}),
            true,
        ),
        (json!({"schema":2,"root":{"type":"text","text":"x"}}), false),
        (
            json!({"schema":1,"root":{"type":"button","action":"run"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"button","label":null,"icon":null,"action":"run"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"button","icon":"run","action":"bad action"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"input","bind":"bad[0]"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"input","bind":"name","on_submit":null}}),
            true,
        ),
        (
            json!({"schema":1,"root":{"type":"input","bind":"name","on_submit":"bad action"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"text","text":"x","id":"bad id"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"text","text":"x","visible":"shown"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"split","children":[{"type":"text","text":"x"}]}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"split","children":[{"type":"text","text":"x"},{"type":"text","text":"y"}]}}),
            true,
        ),
        (
            json!({"schema":1,"root":{"type":"table","key":"id","items":"$rows","columns":[]}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"list","key":"id","title":"name","source":"rpc"}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"list","id":"items","key":"id","title":"name","source":"rpc"}}),
            true,
        ),
        (
            json!({"schema":1,"root":{"type":"list","key":"id","title":"name","items":null}}),
            false,
        ),
        (
            json!({"schema":1,"root":{"type":"list","key":"id","title":"name","items":"$rows"}}),
            true,
        ),
        (
            json!({"schema":1,"root":{"type":"form","schema":"../evil.json"}}),
            false,
        ),
    ] {
        let runtime_valid = parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok();
        assert_eq!(runtime_valid, expected, "runtime: {view}");
        assert_eq!(validator.is_valid(&view), expected, "schema: {view}");
    }
}

#[test]
/// 清单中独立可判断的版本、标识、翻译、运行形态和贡献约束同步到 Schema。
fn manifest_schema_matches_local_runtime_constraints() {
    let schema = schema::manifest_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let valid = valid_manifest();
    let mut cases = vec![(valid.clone(), true)];
    for (field, field_value) in [
        ("schema", json!(2)),
        ("id", json!("Bad.name")),
        ("publisher", json!("Bad")),
        ("name", json!({"zh-CN":"笔记"})),
        ("description", json!({"en":" "})),
        ("repository", json!("http://example.com")),
        ("categories", json!(["bad/category"])),
        ("permissions", json!([1])),
        ("runtime", json!({"kind":"sidecar","main":{"any":"app"}})),
        ("runtime", json!({"kind":"sidecar","main":{}})),
        (
            "runtime",
            json!({"kind":"script","main":"dist/main.js","activation":"startup"}),
        ),
        ("runtime", json!({"kind":"script","main":"../evil"})),
        (
            "contributes",
            json!({"commands":[{"id":"bad id","title":"Hi"}]}),
        ),
    ] {
        let mut manifest = valid.clone();
        manifest[field] = field_value;
        cases.push((manifest, false));
    }
    for (manifest, expected) in cases {
        let runtime_valid = parse(&manifest).is_ok();
        assert_eq!(runtime_valid, expected, "runtime: {manifest}");
        assert_eq!(
            validator.is_valid(&manifest),
            expected,
            "schema: {manifest}"
        );
    }
}

#[test]
/// 局部身份必须与注册器一致，分隔符只能出现在首尾字母数字之间。
fn local_ids_share_registry_boundary_rules() {
    let manifest_schema = schema::manifest_json_schema();
    let manifest_validator = jsonschema::validator_for(&manifest_schema).unwrap();
    let view_schema = schema::native_view_json_schema();
    let view_validator = jsonschema::validator_for(&view_schema).unwrap();
    for (id, expected) in [
        ("a", true),
        ("A9", true),
        ("a._-9", true),
        ("9.x", true),
        ("", false),
        (".", false),
        ("..", false),
        (".name", false),
        ("name.", false),
        ("-name", false),
        ("name-", false),
        ("_name", false),
        ("name_", false),
        ("name/path", false),
    ] {
        let mut manifest = valid_manifest();
        manifest["contributes"] = json!({"commands":[{"id":id,"title":"Run"}]});
        assert_eq!(
            parse(&manifest).is_ok(),
            expected,
            "manifest runtime: {id:?}"
        );
        assert_eq!(
            manifest_validator.is_valid(&manifest),
            expected,
            "manifest schema: {id:?}"
        );
        let view = json!({"schema":1,"root":{"type":"button","id":id,"label":"Run","action":id}});
        assert_eq!(
            parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok(),
            expected,
            "view runtime: {id:?}"
        );
        assert_eq!(
            view_validator.is_valid(&view),
            expected,
            "view schema: {id:?}"
        );
    }
}

#[test]
/// 常见跨平台路径边界在离线 Schema 和真实路径校验中必须保持一致。
fn package_path_schema_matches_portable_path_validation() {
    let schema = schema::manifest_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for path in [
        "file.txt",
        "folder/file.txt",
        "目录/图标.svg",
        ".hidden",
        "a..b/file",
        "",
        "/root",
        "folder/",
        "folder//file",
        "../file",
        "folder/../file",
        r"folder\file",
        "C:/test",
        "bin/CON.txt",
        "aux",
        "COM¹.txt",
        "folder/file.",
        "folder/file ",
        "CONIN$",
        "file\nname",
        "file|name",
    ] {
        let mut manifest = valid_manifest();
        manifest["icon"] = json!(path);
        let runtime_valid = parse(&manifest).is_ok();
        assert_eq!(
            validator.is_valid(&manifest),
            runtime_valid,
            "path: {path:?}"
        );
    }
}

#[test]
/// 扩展键与运行时一致地允许换行，Schema 不能额外限制插件私有扩展名。
fn extension_keys_use_the_same_prefix_rule_in_schema_and_runtime() {
    let mut manifest = valid_manifest();
    manifest["x-\n"] = json!({"nested":true});
    assert!(parse(&manifest).is_ok());
    let schema = schema::manifest_json_schema();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&manifest)
    );
}

#[test]
/// 清单身份保留名必须与宿主 PluginId::parse 同判，市场才能构建出宿主可反序列化的条目。
fn manifest_id_reserved_names_match_host_identity_rules() {
    let schema = schema::manifest_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    // Windows 保留设备名只看第一个点前的段，所以整份 ID 与宿主目录名冲突的样本都要拒绝。
    let reserved_publishers = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "conin$",
        "conout$",
    ];
    for publisher in reserved_publishers {
        let mut manifest = valid_manifest();
        manifest["id"] = json!(format!("{publisher}.notes"));
        manifest["publisher"] = json!(publisher);
        assert!(
            parse(&manifest).is_err(),
            "runtime reserved publisher: {publisher}"
        );
        assert!(
            !validator.is_valid(&manifest),
            "schema reserved publisher: {publisher}"
        );
    }
    for id in [
        "state.json",
        "state-recovery-1.json",
        "state-recovery-.json",
    ] {
        let mut manifest = valid_manifest();
        manifest["id"] = json!(id);
        manifest["publisher"] = json!(id.split('.').next().unwrap());
        assert!(parse(&manifest).is_err(), "runtime state file: {id}");
        assert!(!validator.is_valid(&manifest), "schema state file: {id}");
    }
    // 保留名只看第一个点前的段：设备名出现在 name 位或带前缀时仍是合法身份；
    // `com0` 不在任何一侧的保留列表里，也不能被 Schema 额外收紧。
    for id in [
        "acme.notes",
        "acme.con",
        "acme.com1",
        "acme.lpt9",
        "console.notes",
        "companion.notes",
        "com0.notes",
        "stateless.json",
        "state-recovery.json",
    ] {
        let mut manifest = valid_manifest();
        manifest["id"] = json!(id);
        manifest["publisher"] = json!(id.split('.').next().unwrap());
        assert!(parse(&manifest).is_ok(), "runtime legal id: {id}");
        assert!(validator.is_valid(&manifest), "schema legal id: {id}");
    }
    // `kerminal` 不是设备名，`kerminal.con` 不触发保留名规则：Official 接受、
    // ThirdParty 只因官方命名空间策略拒绝。保留名分支的 location 同为 manifest.id，
    // 所以必须连 reason 一起断言，避免把身份规则拒绝误当策略拒绝。Schema 只约束
    // 身份形状，不参与来源信任判断。
    let mut official = valid_manifest();
    official["id"] = json!("kerminal.con");
    official["publisher"] = json!("kerminal");
    let official_bytes = serde_json::to_vec(&official).unwrap();
    assert!(parse_manifest(&official_bytes, ManifestPolicy::Official).is_ok());
    match parse_manifest(&official_bytes, ManifestPolicy::ThirdParty) {
        Err(ProtocolError::Invalid { location, reason })
            if location == "manifest.id" && reason.contains("官方命名空间") => {}
        other => panic!("官方命名空间拒绝应来自 manifest.id 策略: {other:?}"),
    }
    assert!(validator.is_valid(&official));
    // `state-recovery-` 前缀与 `.json` 后缀之间的字符只能来自身份语法允许的集合，
    // 越界的样本在两侧都因语法拒绝，而不是因保留名拒绝。
    let mut manifest = valid_manifest();
    manifest["id"] = json!("acme.state.json");
    manifest["publisher"] = json!("acme");
    assert!(parse(&manifest).is_err(), "runtime syntax: extra dot");
    assert!(!validator.is_valid(&manifest), "schema syntax: extra dot");
}

#[test]
/// 三种集合都必须恰选一种数据来源；可选字段的 null 与缺省具有相同语义。
fn collection_rpc_and_inline_sources_are_mutually_exclusive() {
    let schema = schema::native_view_json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let cases = [
        (None, Some(json!("$rows")), Some("items"), true),
        (Some(Value::Null), Some(json!("$rows")), None, true),
        (Some(json!("rpc")), None, Some("items"), true),
        (Some(json!("rpc")), Some(Value::Null), Some("items"), true),
        (
            Some(json!("rpc")),
            Some(json!("$rows")),
            Some("items"),
            false,
        ),
        (Some(json!("rpc")), None, None, false),
        (None, None, Some("items"), false),
        (Some(Value::Null), Some(Value::Null), Some("items"), false),
    ];
    for kind in ["list", "table", "tree"] {
        for (source, items, id, expected) in &cases {
            let mut node = match kind {
                "table" => {
                    json!({"type":kind,"key":"id","columns":[{"key":"name","title":"Name"}]})
                }
                _ => json!({"type":kind,"key":"id","title":"name"}),
            };
            if let Some(source) = source {
                node["source"] = source.clone();
            }
            if let Some(items) = items {
                node["items"] = items.clone();
            }
            if let Some(id) = id {
                node["id"] = json!(id);
            }
            let view = json!({"schema":1,"root":node});
            let runtime_valid = parse_native_view(&serde_json::to_vec(&view).unwrap()).is_ok();
            assert_eq!(runtime_valid, *expected, "runtime: {view}");
            assert_eq!(validator.is_valid(&view), *expected, "schema: {view}");
        }
    }
}
