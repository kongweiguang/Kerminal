//! 内容入口复用 GPUI 的动作传播，不引入另一条插件命令总线。
//!
//! @author kongweiguang

/// 打开中央页面始终定位已有实例；页面不携带持久标签的恢复状态。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.content.openPage", no_json, no_register)]
pub struct OpenContentPage {
    pub content_id: String,
}

/// 新标签携带明确的内容版本，宿主仍通过工作区事务决定能否提交。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.content.openTab", no_json, no_register)]
pub struct OpenContentTab {
    pub content_id: String,
    pub title: String,
    pub state_version: u32,
    pub state: serde_json::Value,
}
