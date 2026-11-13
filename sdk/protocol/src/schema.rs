//! 由 Rust 契约派生 JSON Schema，供 SDK 与编辑器使用。
//! @author kongweiguang

use serde_json::{Value, json};

use crate::{
    CancelParams, CommandContext, CommandRunParams, EventParams, HostOpenResult,
    HostOperationParams, HostTestResult, InitializeParams, InitializeResult, NativeView,
    PluginIdentity, PluginManifest, ProgressParams, RpcMessage, ViewActionParams, ViewActionResult,
    ViewFetchParams, ViewFetchResult, ViewInstanceParams, ViewOpenParams, ViewOpenResult,
    ViewStateResult, ViewUpdateParams, ViewVisibilityParams,
};

#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct RpcTypeCatalog {
    initialize: InitializeParams,
    initialize_result: InitializeResult,
    plugin_identity: PluginIdentity,
    view_open: ViewOpenParams,
    view_open_result: ViewOpenResult,
    view_visibility: ViewVisibilityParams,
    view_action: ViewActionParams,
    view_action_result: ViewActionResult,
    view_fetch: ViewFetchParams,
    view_fetch_result: ViewFetchResult,
    view_instance: ViewInstanceParams,
    view_state_result: ViewStateResult,
    view_update: ViewUpdateParams,
    command_run: CommandRunParams,
    command_context: CommandContext,
    host_operation: HostOperationParams,
    host_test_result: HostTestResult,
    host_open_result: HostOpenResult,
    event: EventParams,
    cancel: CancelParams,
    progress: ProgressParams,
}

/// 清单 schema 与运行时解析共享 Rust 类型和扩展字段规则。
pub fn manifest_json_schema() -> Value {
    let mut schema: Value = schemars::schema_for!(PluginManifest).into();
    constrain_manifest_schema(&mut schema);
    strict_extensions(schema)
}

/// 原生组件 schema 由同一套节点枚举产生。
pub fn native_view_json_schema() -> Value {
    let mut schema: Value = schemars::schema_for!(NativeView).into();
    merge_native_node_properties(&mut schema);
    constrain_native_view_schema(&mut schema);
    strict_extensions(schema)
}

/// RPC 信封与请求类型保持与 Rust 协议模型一致。
pub fn rpc_json_schema() -> Value {
    let mut schema: Value = schemars::schema_for!(RpcMessage).into();
    let catalog: Value = schemars::schema_for!(RpcTypeCatalog).into();
    if let (Some(definitions), Some(extra)) = (
        schema.get_mut("$defs").and_then(Value::as_object_mut),
        catalog.get("$defs").and_then(Value::as_object),
    ) {
        definitions.extend(extra.clone());
        for name in ["RpcRequest", "RpcNotification", "RpcResponse"] {
            if let Some(properties) = definitions
                .get_mut(name)
                .and_then(|entry| entry.get_mut("properties"))
                .and_then(Value::as_object_mut)
            {
                properties.insert("jsonrpc".into(), json!({"type":"string", "const":"2.0"}));
            }
        }
        for name in ["RpcRequest", "RpcNotification"] {
            if let Some(message) = definitions.get_mut(name) {
                set_property_schema(message, "method", rpc_method_schema());
                set_property_schema(
                    message,
                    "params",
                    json!({"anyOf":[
                        {"type":"object","additionalProperties":true},
                        {"type":"array","items":true}
                    ]}),
                );
            }
        }
        if let Some(request) = definitions.get_mut("RpcRequest") {
            set_property_schema(request, "id", rpc_id_schema());
        }
        if let Some(response) = definitions
            .get_mut("RpcResponse")
            .and_then(Value::as_object_mut)
        {
            // 自定义反序列化要求 id 显式存在，且 error 存在时必须是对象。
            if let Some(properties) = response
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            {
                properties.insert("error".into(), json!({"$ref":"#/$defs/RpcError"}));
                properties.insert("id".into(), nullable(rpc_id_schema()));
            }
            let required = response.entry("required").or_insert_with(|| json!([]));
            if let Some(required) = required.as_array_mut()
                && !required.iter().any(|field| field == "id")
            {
                required.push(json!("id"));
            }
            response.insert(
                "oneOf".into(),
                json!([
                    {"required":["result"], "not":{"required":["error"]}, "properties":{"id":{"type":"string"}}},
                    {"required":["error"], "not":{"required":["result"]}}
                ]),
            );
        }
    }
    schema
}

/// 运行时接受空选项，但空值不得绕开存在时的具体字段规则。
fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema, {"type":"null"}]})
}

/// 局部身份只使用 ASCII，Schema 的字符数与运行时的字节数因此一致。
fn local_id_schema(max: usize) -> Value {
    json!({"type":"string", "minLength":1, "maxLength":max,
           "pattern":r"^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$",
           "not":{"pattern":r"[^A-Za-z0-9._-]"}})
}

/// 绑定只允许对象键链；只读形式的长度上限不包括前缀 $。
fn binding_schema(read_only: bool) -> Value {
    let (pattern, invalid, max) = if read_only {
        (
            r"^\$[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$",
            r"[^A-Za-z0-9_.$-]",
            257,
        )
    } else {
        (
            r"^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$",
            r"[^A-Za-z0-9_.-]",
            256,
        )
    };
    json!({"type":"string", "maxLength":max, "pattern":pattern, "not":{"pattern":invalid}})
}

/// 文案允许 Unicode，但不能只有空白字符或包含控制字符。
fn text_schema(max: usize) -> Value {
    json!({"type":"string", "minLength":1, "maxLength":max,
           "pattern":r"[^\u0009-\u000D\u0020\u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]",
           "not":{"pattern":r"[\u0000-\u001F\u007F-\u009F]"}})
}

/// 包相对路径的静态约束跨平台一致，实际资源存在性仍由包校验器检查。
fn package_path_schema() -> Value {
    json!({"type":"string", "minLength":1, "allOf":[
        {"not":{"pattern":r"(^/|/$|//)"}},
        {"not":{"pattern":r#"[\\:<>"|?*\u0000-\u001F\u007F-\u009F]"#}},
        {"not":{"pattern":r"(^|/)(?:\.{1,2}|[^/]*[. ])(?:/|$)"}},
        {"not":{"pattern":r"(^|/)(?:[Cc][Oo][Nn]|[Pp][Rr][Nn]|[Aa][Uu][Xx]|[Nn][Uu][Ll]|[Cc][Oo][Nn][Ii][Nn]\$|[Cc][Oo][Nn][Oo][Uu][Tt]\$|[Cc][Oo][Mm][1-9¹²³]|[Ll][Pp][Tt][1-9¹²³])(?:\.[^/]*)?(?:/|$)"}}
    ]})
}

/// RPC 请求与响应共用双向 ID 格式，发起方前缀仍由宿主按方向核验。
fn rpc_id_schema() -> Value {
    json!({"type":"string", "minLength":3, "maxLength":128,
           "pattern":r"^(?:h:|p:)", "not":{"pattern":r"[\u0000-\u001F\u007F-\u009F]"}})
}

/// 方法长度和控制字符约束不能只交给运行时，否则编辑器会接受非法信封。
fn rpc_method_schema() -> Value {
    json!({"type":"string", "minLength":1, "maxLength":128,
           "not":{"pattern":r"[\u0000-\u001F\u007F-\u009F]"}})
}

/// 只覆盖已有字段的形状，避免新增节点时悄悄生成未声明属性。
fn set_property_schema(object: &mut Value, name: &str, schema: Value) {
    if let Some(property) = object
        .get_mut("properties")
        .and_then(|value| value.get_mut(name))
    {
        *property = schema;
    }
}

/// 局部静态约束放入清单 Schema；跨字段身份与来源信任仍由运行时检查。
fn constrain_manifest_schema(schema: &mut Value) {
    set_property_schema(schema, "schema", json!({"type":"integer", "const":1}));
    set_property_schema(
        schema,
        "id",
        // 双重 not 包在 allOf 里：语法拒绝之外，还要镜像 domain PluginId::parse 的保留名
        // 规则——Windows 保留设备名只看第一个点前的段（`conin$` 等含 $ 的名字其实已被
        // 语法拒绝，保留在此是为了与 domain 列表一一对应，避免未来放宽语法时漏掉），
        // `state.json` 与 `state-recovery-*.json` 是宿主状态文件的固定命名。
        json!({"type":"string", "allOf":[
        {"not":{"pattern":r"[^a-z0-9.-]"}},
        {"not":{"anyOf":[
        {"pattern":r"^(?:con|prn|aux|nul|com[1-9]|lpt[1-9]|conin\$|conout\$)(?:\.[a-z0-9][a-z0-9-]{1,62})?$"},
        {"pattern":r"^state\.json$"},
        {"pattern":r"^state-recovery-[a-z0-9-]*\.json$"}
        ]}}
        ],
        "pattern":r"^[a-z0-9][a-z0-9-]{1,38}\.[a-z0-9][a-z0-9-]{1,62}$"}),
    );
    set_property_schema(
        schema,
        "publisher",
        json!({"type":"string",
        "pattern":r"^[a-z0-9][a-z0-9-]{1,38}$", "not":{"pattern":r"[^a-z0-9-]"}}),
    );
    set_property_schema(schema, "api", json!({"type":"string", "maxLength":256}));
    set_property_schema(schema, "license", nullable(text_schema(256)));
    let mut repository = text_schema(2048);
    repository["pattern"] = json!(r"^https://");
    set_property_schema(schema, "repository", nullable(repository));
    for name in ["icon", "l10n"] {
        set_property_schema(schema, name, nullable(package_path_schema()));
    }
    for name in ["name", "description"] {
        set_property_schema(
            schema,
            name,
            json!({"type":"object", "required":["en"],
            "maxProperties":128, "propertyNames":text_schema(64), "additionalProperties":text_schema(4096)}),
        );
    }
    if let Some(categories) = schema.pointer_mut("/properties/categories") {
        categories["items"] = local_id_schema(128);
    }
    if let Some(permissions) = schema.pointer_mut("/properties/permissions") {
        // 权限已退化为展示描述，重复项不再被安装器拒绝，schema 也不应额外收紧。
        permissions["items"] = json!({"type":"string"});
    }
    if let Some(views) = schema.pointer_mut("/properties/views") {
        views["propertyNames"] = local_id_schema(128);
    }
    let Some(definitions) = schema.get_mut("$defs").and_then(Value::as_object_mut) else {
        return;
    };
    for name in [
        "ToolContribution",
        "TabContribution",
        "HostTypeContribution",
        "CommandContribution",
    ] {
        if let Some(contribution) = definitions.get_mut(name) {
            set_property_schema(contribution, "id", local_id_schema(63));
            set_property_schema(contribution, "title", text_schema(512));
            set_property_schema(contribution, "icon", nullable(package_path_schema()));
            set_property_schema(contribution, "schema", package_path_schema());
            set_property_schema(contribution, "keybinding", nullable(text_schema(128)));
            if name == "TabContribution" {
                set_property_schema(
                    contribution,
                    "state_version",
                    json!({"type":"integer","minimum":1,"maximum":u32::MAX}),
                );
            }
        }
    }
    if let Some(settings) = definitions.get_mut("SettingsContribution") {
        set_property_schema(settings, "schema", package_path_schema());
    }
    if let Some(variants) = definitions
        .get_mut("PluginRuntime")
        .and_then(|value| value.get_mut("oneOf"))
        .and_then(Value::as_array_mut)
    {
        for variant in variants {
            if let Some("sidecar") = variant
                .pointer("/properties/kind/const")
                .and_then(Value::as_str)
                && let Some(main) = variant.pointer_mut("/properties/main")
            {
                main["minProperties"] = json!(1);
                if let Some(platforms) = main.get_mut("properties").and_then(Value::as_object_mut) {
                    platforms.remove("any");
                    for path in platforms.values_mut() {
                        *path = package_path_schema();
                    }
                }
            }
        }
    }
    if let Some(variants) = definitions
        .get_mut("ViewDefinition")
        .and_then(|value| value.get_mut("oneOf"))
        .and_then(Value::as_array_mut)
    {
        for variant in variants {
            set_property_schema(variant, "file", package_path_schema());
            set_property_schema(variant, "entry", package_path_schema());
        }
    }
}

/// 单节点约束写入 Schema，集合来源按 null 与缺省等价的互斥分支表达；跨节点约束由宿主校验。
fn constrain_native_view_schema(schema: &mut Value) {
    set_property_schema(schema, "schema", json!({"type":"integer","const":1}));
    let Some(definitions) = schema.get_mut("$defs").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(variants) = definitions
        .get_mut("NativeNode")
        .and_then(|value| value.get_mut("oneOf"))
        .and_then(Value::as_array_mut)
    {
        for variant in variants {
            set_property_schema(variant, "id", nullable(local_id_schema(128)));
            set_property_schema(variant, "visible", nullable(binding_schema(true)));
            let kind = variant
                .pointer("/properties/type/const")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match kind.as_str() {
                "input" | "textarea" | "number" | "select" | "switch" | "checkbox" | "path"
                | "secret" => {
                    set_property_schema(variant, "bind", binding_schema(false));
                    set_property_schema(variant, "on_submit", nullable(local_id_schema(128)));
                }
                "split" => {
                    if let Some(children) = variant.pointer_mut("/properties/children") {
                        children["minItems"] = json!(2);
                    }
                }
                "button" => {
                    set_property_schema(variant, "action", local_id_schema(128));
                    set_property_schema(variant, "busy", nullable(binding_schema(true)));
                    variant["allOf"] = json!([{"anyOf":[
                        {"required":["label"],"properties":{"label":{"type":"string"}}},
                        {"required":["icon"],"properties":{"icon":{"type":"string"}}}
                    ]}]);
                }
                "list" | "table" | "tree" => {
                    set_property_schema(variant, "items", nullable(binding_schema(true)));
                    set_property_schema(variant, "key", binding_schema(false));
                    set_property_schema(variant, "on_activate", nullable(local_id_schema(128)));
                    if kind != "table" {
                        set_property_schema(variant, "title", binding_schema(false));
                    }
                    if kind == "table"
                        && let Some(columns) = variant.pointer_mut("/properties/columns")
                    {
                        columns["minItems"] = json!(1);
                    }
                    variant["allOf"] = json!([{"oneOf":[
                        {"required":["id","source"],"properties":{
                            "id":{"type":"string"},
                            "source":{"const":"rpc"},
                            "items":{"type":"null"}
                        }},
                        {"required":["items"],"properties":{
                            "items":{"type":"string"},
                            "source":{"type":"null"}
                        }}
                    ]}]);
                }
                "form" => {
                    set_property_schema(variant, "bind", nullable(binding_schema(false)));
                    set_property_schema(variant, "schema", package_path_schema());
                }
                // 矩阵只能是实例数据里的只读绑定，静态字面量不进入 Schema；
                // 具体方阵内容由宿主渲染前校验，Schema 只约束绑定与标签形状。
                "qrcode" => {
                    set_property_schema(variant, "matrix", binding_schema(true));
                    set_property_schema(variant, "label", nullable(text_schema(512)));
                }
                _ => {}
            }
        }
    }
    for name in [
        "TabPane",
        "MenuItem",
        "SelectOption",
        "EmptyState",
        "TableColumn",
    ] {
        if let Some(item) = definitions.get_mut(name) {
            match name {
                "TabPane" => {
                    set_property_schema(item, "id", local_id_schema(128));
                    set_property_schema(item, "title", text_schema(512));
                }
                "MenuItem" => {
                    set_property_schema(item, "action", local_id_schema(128));
                    set_property_schema(item, "label", text_schema(512));
                }
                "SelectOption" => set_property_schema(item, "label", text_schema(512)),
                "EmptyState" => set_property_schema(item, "title", text_schema(512)),
                "TableColumn" => {
                    set_property_schema(item, "key", binding_schema(false));
                    set_property_schema(item, "title", text_schema(512));
                }
                _ => {}
            }
        }
    }
}

/// serde 展平的通用字段也属于各个节点分支，合并后才能正确限制额外字段。
fn merge_native_node_properties(schema: &mut Value) {
    let Some(node) = schema
        .pointer_mut("/$defs/NativeNode")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(shared) = node.get("properties").and_then(Value::as_object).cloned() else {
        return;
    };
    let Some(variants) = node.get_mut("oneOf").and_then(Value::as_array_mut) else {
        return;
    };
    for variant in variants {
        if let Some(properties) = variant.get_mut("properties").and_then(Value::as_object_mut) {
            properties.extend(shared.clone());
        }
    }
}

/// 结构对象的扁平扩展映射只允许 x- 键；普通数据字典不受该限制。
fn strict_extensions(mut schema: Value) -> Value {
    match &mut schema {
        Value::Object(object) => {
            let object_with_fields =
                object.contains_key("properties") && object.get("type") == Some(&json!("object"));
            if object_with_fields
                && object
                    .get("additionalProperties")
                    .is_some_and(|value| value == &json!(true) || value == &json!({}))
            {
                object.insert("patternProperties".into(), json!({r"^x-[\s\S]+": {}}));
                object.insert("additionalProperties".into(), json!(false));
            }
            for value in object.values_mut() {
                *value = strict_extensions(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = strict_extensions(std::mem::take(value));
            }
        }
        _ => {}
    }
    schema
}
