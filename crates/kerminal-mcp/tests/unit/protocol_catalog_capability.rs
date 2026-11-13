//! 冻结工具目录与能力映射的一致性回归。
//!
//! 通过 `#[path]` 作为 `protocol` 的子模块编译，直接复用生产目录与能力入口；
//! 测试实现留在 `tests/unit/`，src 仅保留模块声明。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use super::*;

/// 能力映射的已知工具名单必须与冻结目录完全一致。
///
/// 两份名单漂移会让禁用插件后仍从 `tools/list` 暴露工具，或把目录工具误判为未知；
/// 因此这里同时断言正向覆盖、反向集合相等和未知名称拒绝。
///
/// @author kongweiguang
#[test]
fn capability_map_covers_frozen_catalog_exactly() {
    let catalog = frozen_catalog()
        .tools
        .iter()
        .filter(|definition| definition.enabled && definition.exposed_to_mcp)
        .map(|definition| definition.name.as_str())
        .collect::<BTreeSet<_>>();
    let known = capabilities::known_tools()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(catalog.len(), 21);
    assert_eq!(catalog, known);
    for tool in &catalog {
        assert!(
            capabilities::is_known_tool(tool),
            "{tool} must be known to the capability map"
        );
    }
    assert!(!capabilities::is_known_tool("ssh.exec"));
}
