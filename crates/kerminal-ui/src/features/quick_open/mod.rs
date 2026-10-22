//! Quick Open 与命令面板共享动态目录查询、稳定目标身份和统一取消边界。
//!
//! @author kongweiguang

mod model;
mod view;

pub use model::{
    QuickOpenEntry, QuickOpenEvent, QuickOpenMode, QuickOpenOpenContext, QuickOpenSelection,
    QuickOpenTerminalTarget,
};
pub use view::QuickOpenView;

#[cfg(test)]
#[path = "../../../tests/unit/quick_open.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/quick_open_interaction.rs"]
mod interaction_tests;
