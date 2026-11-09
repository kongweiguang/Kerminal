//! 插件设置 schema 中秘密字段的受限投影。
//! @author kongweiguang

use crate::{AppError, AppResult};
use kerminal_plugin_protocol::ConfigurationSchema;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use zeroize::Zeroize;

/// JSON Pointer 保留斜杠和波浪号转义，避免不同字段映射到同一个 Vault 槽。
fn pointer_child(parent: &str, child: &str) -> String {
    format!("{parent}/{}", child.replace('~', "~0").replace('/', "~1"))
}

/// 本地锚点也可能来自动态引用；保守检查所有同名目标，避免漏标秘密。
fn local_targets<'a>(root: &'a Value, reference: &str) -> AppResult<Vec<&'a Value>> {
    let fragment = reference
        .strip_prefix('#')
        .ok_or_else(|| AppError::invalid("插件设置 schema 引用不是本地引用"))?;
    let fragment = percent_encoding::percent_decode_str(fragment)
        .decode_utf8()
        .map_err(|_| AppError::invalid("插件设置 schema 引用编码无效"))?;
    if fragment.is_empty() {
        return Ok(vec![root]);
    }
    if fragment.starts_with('/') {
        return root
            .pointer(&fragment)
            .map(|value| vec![value])
            .ok_or_else(|| AppError::invalid("插件设置 schema 引用无法解析"));
    }
    let mut pending = vec![root];
    let mut matches = Vec::new();
    while let Some(value) = pending.pop() {
        if let Some(object) = value.as_object() {
            if ["$anchor", "$dynamicAnchor"]
                .iter()
                .any(|key| object.get(*key).and_then(Value::as_str) == Some(fragment.as_ref()))
            {
                matches.push(value);
            }
            pending.extend(object.values());
        } else if let Some(array) = value.as_array() {
            pending.extend(array);
        }
    }
    if matches.is_empty() {
        return Err(AppError::invalid("插件设置 schema 锚点无法解析"));
    }
    Ok(matches)
}

/// 正则语义由协议同一个 JSON Schema 校验器决定，不能换成不同方言。
fn pattern_matches(pattern: &str, name: &str) -> AppResult<bool> {
    let source = serde_json::to_vec(&serde_json::json!({"type":"string","pattern":pattern}))
        .map_err(|_| AppError::invalid("插件设置 schema 模式无效"))?;
    let schema = ConfigurationSchema::parse(&source)
        .map_err(|_| AppError::invalid("插件设置 schema 模式无效"))?;
    Ok(schema.validate(&Value::String(name.to_owned())).is_ok())
}

/// 只识别 schema 在当前实例上可能应用的秘密注解，遇到无法解析的引用时拒绝写入。
pub(crate) fn secret_paths(
    schema: &ConfigurationSchema,
    value: &Value,
) -> AppResult<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    let mut visited = HashSet::new();
    visit_schema(
        &schema.document,
        &schema.document,
        value,
        "",
        &mut paths,
        &mut visited,
        0,
    )?;
    if paths.contains("") {
        return Err(AppError::invalid("插件设置根不能是秘密字段"));
    }
    Ok(paths)
}

/// 合并组合 schema 的秘密注解，保守处理所有分支以防隐藏字段落盘为明文。
fn visit_schema<'a>(
    root: &'a Value,
    schema: &'a Value,
    value: &Value,
    path: &str,
    paths: &mut BTreeSet<String>,
    visited: &mut HashSet<(usize, String)>,
    depth: usize,
) -> AppResult<()> {
    if depth > 128 {
        return Err(AppError::invalid("插件设置 schema 嵌套过深"));
    }
    if !visited.insert((schema as *const Value as usize, path.to_owned())) {
        return Ok(());
    }
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if object.get("x-secret") == Some(&Value::Bool(true)) {
        paths.insert(path.to_owned());
        return Ok(());
    }
    for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
        if let Some(reference) = object.get(key).and_then(Value::as_str) {
            for target in local_targets(root, reference)? {
                visit_schema(root, target, value, path, paths, visited, depth + 1)?;
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            for branch in branches {
                visit_schema(root, branch, value, path, paths, visited, depth + 1)?;
            }
        }
    }
    for key in ["if", "then", "else", "not"] {
        if let Some(branch) = object.get(key) {
            visit_schema(root, branch, value, path, paths, visited, depth + 1)?;
        }
    }
    if let Some(fields) = value.as_object() {
        let properties = object.get("properties").and_then(Value::as_object);
        let patterns = object.get("patternProperties").and_then(Value::as_object);
        for (name, field) in fields {
            let child_path = pointer_child(path, name);
            if let Some(property_schema) = object.get("propertyNames") {
                let mut key_secrets = BTreeSet::new();
                let mut key_visited = HashSet::new();
                visit_schema(
                    root,
                    property_schema,
                    &Value::String(name.clone()),
                    &child_path,
                    &mut key_secrets,
                    &mut key_visited,
                    depth + 1,
                )?;
                if !key_secrets.is_empty() {
                    return Err(AppError::invalid("插件设置字段名不能是秘密"));
                }
            }
            let mut matched = false;
            if let Some(child) = properties.and_then(|properties| properties.get(name)) {
                matched = true;
                visit_schema(root, child, field, &child_path, paths, visited, depth + 1)?;
            }
            if let Some(patterns) = patterns {
                for (pattern, child) in patterns {
                    if pattern_matches(pattern, name)? {
                        matched = true;
                        visit_schema(root, child, field, &child_path, paths, visited, depth + 1)?;
                    }
                }
            }
            if !matched {
                for key in ["additionalProperties", "unevaluatedProperties"] {
                    if let Some(child) = object.get(key) {
                        visit_schema(root, child, field, &child_path, paths, visited, depth + 1)?;
                    }
                }
            }
        }
        for dependency_key in ["dependentSchemas", "dependencies"] {
            if let Some(dependencies) = object.get(dependency_key).and_then(Value::as_object) {
                for (name, child) in dependencies {
                    if fields.contains_key(name) && !child.is_array() {
                        visit_schema(root, child, value, path, paths, visited, depth + 1)?;
                    }
                }
            }
        }
    }
    if let Some(items) = value.as_array() {
        let prefix = object.get("prefixItems").and_then(Value::as_array);
        for (index, item) in items.iter().enumerate() {
            let child_path = pointer_child(path, &index.to_string());
            if let Some(child) = prefix.and_then(|prefix| prefix.get(index)) {
                visit_schema(root, child, item, &child_path, paths, visited, depth + 1)?;
            } else if let Some(child) = object.get("items") {
                if let Some(tuple) = child.as_array() {
                    if let Some(child) = tuple.get(index).or_else(|| object.get("additionalItems"))
                    {
                        visit_schema(root, child, item, &child_path, paths, visited, depth + 1)?;
                    }
                } else {
                    visit_schema(root, child, item, &child_path, paths, visited, depth + 1)?;
                }
            }
            for key in ["contains", "unevaluatedItems"] {
                if let Some(child) = object.get(key) {
                    visit_schema(root, child, item, &child_path, paths, visited, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

/// 原字段必须存在，避免错误路径在保存时创建未声明的对象。
pub(crate) fn set_pointer(value: &mut Value, pointer: &str, replacement: Value) -> AppResult<()> {
    let target = value
        .pointer_mut(pointer)
        .ok_or_else(|| AppError::invalid("插件设置字段路径无效"))?;
    *target = replacement;
    Ok(())
}

/// 删除只支持对象字段；数组中删除会改变后续槽身份，必须先整体编辑数组。
pub(crate) fn remove_pointer(value: &mut Value, pointer: &str) -> AppResult<()> {
    let (parent, leaf) = pointer
        .rsplit_once('/')
        .ok_or_else(|| AppError::invalid("插件设置字段路径无效"))?;
    let leaf = leaf.replace("~1", "/").replace("~0", "~");
    value
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AppError::invalid("数组秘密字段不能单独删除"))?
        .remove(&leaf);
    Ok(())
}

/// 显式清零临时 JSON 中的明文，避免校验后草稿继续持有原字符串。
pub(crate) fn clear_secret_values(value: &mut Value, paths: &BTreeSet<String>) {
    for path in paths {
        if let Some(target) = value.pointer_mut(path) {
            if let Value::String(secret) = target {
                secret.zeroize();
            }
            *target = Value::Null;
        }
    }
}
