//! 仅 patch 应用支持的设置字段，保留文件中的注释、未知键与旧 rendererType。
//!
//! @author kongweiguang

use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use kerminal_application::AppResult;

use super::{KeybindingPreference, SettingsSnapshot};

/// 将已校验快照合并进原文档，避免 serde 全量重写破坏用户注释和扩展字段。
///
/// @author kongweiguang
pub(super) fn apply_settings_snapshot(
    document: &mut DocumentMut,
    settings: &SettingsSnapshot,
) -> AppResult<()> {
    let mut projected = super::serialize_settings_document(settings)?;
    let projected_bindings = projected.as_table_mut().remove("keybindings");
    expand_projected_inline_tables(projected.as_table_mut());
    merge_table(document.as_table_mut(), projected.as_table());

    if let Some(projected_bindings) = projected_bindings {
        merge_keybindings(
            &mut document["keybindings"],
            &projected_bindings,
            &settings.keybindings,
        );
    }
    Ok(())
}

/// 把 serde 投影中的嵌套 inline table 展开，才能递归 patch 用户原有的标准表并保留其注释。
fn expand_projected_inline_tables(table: &mut Table) {
    let keys = table
        .iter()
        .map(|(key, _)| key.to_owned())
        .collect::<Vec<_>>();
    for key in keys {
        let Some(item) = table.get_mut(&key) else {
            continue;
        };
        if let Item::Value(Value::InlineTable(inline)) = item {
            let mut nested = std::mem::take(inline).into_table();
            expand_projected_inline_tables(&mut nested);
            *item = Item::Table(nested);
        }
        match item {
            Item::Table(nested) => expand_projected_inline_tables(nested),
            Item::ArrayOfTables(rows) => {
                for row in rows.iter_mut() {
                    expand_projected_inline_tables(row);
                }
            }
            _ => {}
        }
    }
}

/// 递归更新已知嵌套表；标量替换保留原值的行内注释及格式装饰。
fn merge_table(existing: &mut Table, projected: &Table) {
    let fields = projected
        .iter()
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect::<Vec<_>>();
    for (key, incoming) in fields {
        match existing.get_mut(&key) {
            Some(current) => merge_item(current, incoming),
            None => {
                existing.insert(&key, incoming);
            }
        }
    }
}

/// 依据 TOML 节点类型做局部合并；未知项始终留在已有文档中。
fn merge_item(existing: &mut Item, mut projected: Item) {
    if projected.as_table().is_some() && matches!(existing, Item::Value(Value::InlineTable(_))) {
        expand_existing_inline_table(existing);
    }
    match (existing, &mut projected) {
        (Item::Table(current), Item::Table(incoming)) => merge_table(current, incoming),
        (Item::ArrayOfTables(current), Item::ArrayOfTables(incoming)) => {
            merge_array_of_tables(current, incoming)
        }
        (Item::Value(current), Item::Value(incoming)) => {
            preserve_value_decor(current, incoming);
            preserve_array_value_decor(current, incoming);
            *current = incoming.clone();
        }
        (current, _) => *current = projected,
    }
}

/// 仅当用户原配置把将被递归更新的子文档写成 inline table 时转为普通表，保留其未知键和值装饰。
fn expand_existing_inline_table(item: &mut Item) {
    let expanded = match item {
        Item::Value(Value::InlineTable(inline)) => {
            Some(Item::Table(std::mem::take(inline).into_table()))
        }
        _ => None,
    };
    if let Some(expanded) = expanded {
        *item = expanded;
    }
}

/// 对齐数组表时保留相同 ID 行及其未知字段、注释，并遵循快照中的数组顺序。
fn merge_array_of_tables(existing: &mut ArrayOfTables, projected: &ArrayOfTables) {
    let mut merged = Vec::with_capacity(projected.len());
    for incoming in projected.iter() {
        let identity = table_identity(incoming);
        let previous = identity.and_then(|identity| {
            existing
                .iter()
                .find(|table| table_identity(table) == Some(identity))
        });
        if let Some(previous) = previous {
            let mut table = previous.clone();
            merge_table(&mut table, incoming);
            merged.push(table);
        } else {
            merged.push(incoming.clone());
        }
    }
    existing.clear();
    existing.extend(merged);
}

/// 读取 keybindings 或 keyword rules 的稳定行 ID，不依赖数组内字段顺序。
fn table_identity(table: &Table) -> Option<&str> {
    table
        .get("action")
        .and_then(Item::as_value)
        .and_then(Value::as_str)
        .or_else(|| {
            table
                .get("id")
                .and_then(Item::as_value)
                .and_then(Value::as_str)
        })
}

/// 快捷键只 patch可编辑绑定字段；None 保留旧 TOML 的缺省状态并维持兼容回退。
///
/// @author kongweiguang
fn merge_keybindings(existing: &mut Item, projected: &Item, bindings: &[KeybindingPreference]) {
    if let Some(existing_rows) = existing.as_array_of_tables_mut() {
        for binding in bindings {
            if let Some(current) = existing_rows
                .iter_mut()
                .find(|row| table_identity(row) == Some(binding.action.as_str()))
            {
                merge_keybinding_table(current, binding);
            } else if let Some(incoming) = projected_keybinding_table(projected, &binding.action) {
                existing_rows.push(incoming);
            }
        }
        return;
    }

    if let Some(existing_rows) = existing.as_value_mut().and_then(Value::as_array_mut) {
        for binding in bindings {
            if let Some(current) = existing_rows.iter_mut().find_map(|row| {
                let inline = row.as_inline_table_mut()?;
                (inline_table_identity(inline) == Some(binding.action.as_str())).then_some(inline)
            }) {
                merge_keybinding_inline_table(current, binding);
            } else if let Some(incoming) = projected_keybinding_value(projected, &binding.action) {
                existing_rows.push(incoming);
            }
        }
        return;
    }

    *existing = projected.clone();
}

/// 保持原行的 key、label、说明、用户注释和扩展元数据，只更新快捷键值。
fn merge_keybinding_table(row: &mut Table, binding: &KeybindingPreference) {
    for (key, value) in keybinding_values(binding) {
        let incoming = toml_edit::value(value.to_owned());
        match row.get_mut(key) {
            Some(existing) => merge_item(existing, incoming),
            None => {
                row.insert(key, incoming);
            }
        }
    }
}

/// 更新 inline table 中的绑定字段并复用每个值原有的 TOML decor。
fn merge_keybinding_inline_table(row: &mut InlineTable, binding: &KeybindingPreference) {
    for (key, value) in keybinding_values(binding) {
        let mut incoming = Value::from(value.to_owned());
        match row.get_mut(key) {
            Some(existing) => {
                preserve_value_decor(existing, &mut incoming);
                *existing = incoming;
            }
            None => {
                row.insert(key, incoming);
            }
        }
    }
}

/// 两种 TOML 表结构复用同一份字段列表，None 不写入以保留旧配置的字段缺失语义。
///
/// @author kongweiguang
fn keybinding_values(binding: &KeybindingPreference) -> Vec<(&'static str, &str)> {
    let mut values: Vec<(&'static str, &str)> = vec![
        ("binding", binding.binding.as_str()),
        ("windowsBinding", binding.windows_binding.as_str()),
        ("macBinding", binding.mac_binding.as_str()),
    ];
    if let Some(value) = binding.linux_binding.as_deref() {
        values.push(("linuxBinding", value));
    }
    if let Some(value) = binding.windows_alternate_binding.as_deref() {
        values.push(("windowsAlternateBinding", value));
    }
    values
}

/// 从 serde 生成的两种数组结构中提取新增动作，现有行仍优先保留原始装饰。
fn projected_keybinding_table(projected: &Item, action: &str) -> Option<Table> {
    if let Some(rows) = projected.as_array_of_tables() {
        return rows
            .iter()
            .find(|row| table_identity(row) == Some(action))
            .cloned();
    }
    projected.as_value()?.as_array()?.iter().find_map(|value| {
        let inline = value.as_inline_table()?;
        (inline_table_identity(inline) == Some(action)).then(|| inline.clone().into_table())
    })
}

/// 从新快照提取 inline row，使 inline 原文在补入新动作时仍保持同一种表结构。
fn projected_keybinding_value(projected: &Item, action: &str) -> Option<Value> {
    if let Some(rows) = projected.as_value().and_then(Value::as_array) {
        return rows
            .iter()
            .find(|value| {
                value
                    .as_inline_table()
                    .is_some_and(|inline| inline_table_identity(inline) == Some(action))
            })
            .cloned();
    }
    projected
        .as_array_of_tables()?
        .iter()
        .find(|row| table_identity(row) == Some(action))
        .map(|row| Value::InlineTable(row.clone().into_inline_table()))
}

/// 读取 inline row 的稳定 action/id，和数组表身份解析保持一致。
fn inline_table_identity(table: &InlineTable) -> Option<&str> {
    table
        .get("action")
        .and_then(Value::as_str)
        .or_else(|| table.get("id").and_then(Value::as_str))
}

/// 复制 value 或数组外层的 decor，避免更新值时丢失尾随注释和空白格式。
fn preserve_value_decor(existing: &Value, projected: &mut Value) {
    *projected.decor_mut() = existing.decor().clone();
}

/// 对常规数组按值复用原元素的 decor，让注释随同未变更的枚举/数字选项保留。
fn preserve_array_value_decor(existing: &Value, projected: &mut Value) {
    let (Some(old_array), Some(new_array)) = (existing.as_array(), projected.as_array_mut()) else {
        return;
    };
    let old_values = old_array.iter().collect::<Vec<_>>();
    for new_value in new_array.iter_mut() {
        let Some(old_value) = old_values
            .iter()
            .find(|old_value| old_value.to_string() == new_value.to_string())
        else {
            continue;
        };
        *new_value.decor_mut() = old_value.decor().clone();
    }
}
