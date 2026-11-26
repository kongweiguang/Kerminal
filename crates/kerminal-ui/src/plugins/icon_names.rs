//! 内置 Lucide 图标名的唯一查找入口；插件清单、工具图标和扩展页共用。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::OnceLock};

use gpui_kit::assets::IconName;

/// 同时接受 kebab-case 文件名和 Rust 变体名，兼容插件清单与协议示例的两种写法。
///
/// 只按精确名称匹配，不做大小写折叠或模糊搜索：插件清单是外部输入，宽松映射
/// 会让 `arrow-left` 与 `ArrowLeft` 在不同宿主版本出现不同解析结果。未知名称
/// 返回 `None`，由调用方统一回退占位图标，而不是猜测相邻图标。
///
/// @author kongweiguang
pub(crate) fn builtin_icon_name(name: &str) -> Option<IconName> {
    static BUILTIN: OnceLock<BTreeMap<String, IconName>> = OnceLock::new();
    let icons = BUILTIN.get_or_init(|| {
        let mut names = BTreeMap::new();
        for icon in IconName::ALL.iter().copied() {
            let path = icon.path();
            if let Some(stem) = path
                .strip_prefix("icons/")
                .and_then(|path| path.strip_suffix(".svg"))
            {
                names.insert(stem.to_owned(), icon);
            }
            names.insert(format!("{icon:?}"), icon);
        }
        names
    });
    icons.get(name).copied()
}
