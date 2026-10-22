//! About 信息直接取自 Cargo 包元数据与仓库根许可证，避免版本和许可副本漂移。
//!
//! @author kongweiguang

use gpui_kit::Context;
use url::Url;

use super::UpdatesPanel;

/// 正在运行的 UI 包版本；更新候选版本仍由 UpdateService 单独提供。
pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// SPDX 标识沿用当前 package metadata，避免 About 页维护第二份许可配置。
pub(super) const LICENSE_SPDX: &str = env!("CARGO_PKG_LICENSE");
/// 项目仓库沿用当前 package metadata，不把仓库地址复制进 UI 文案。
pub(super) const PROJECT_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// 直接嵌入仓库根许可证，查看内容时不依赖网络或安装目录布局。
pub(super) const LICENSE_TEXT: &str = include_str!("../../../../../LICENSE");

/// 仅把无凭据 HTTPS 仓库地址交给桌面打开器，拒绝构建元数据中的非网页 URI。
pub(super) fn repository_url() -> Option<&'static str> {
    let repository = Url::parse(PROJECT_REPOSITORY).ok()?;
    if repository.scheme() != "https"
        || repository.host_str().is_none_or(str::is_empty)
        || !repository.username().is_empty()
        || repository.password().is_some()
    {
        return None;
    }
    Some(PROJECT_REPOSITORY)
}

impl UpdatesPanel {
    /// 许可证正文以内联折叠方式展开，避免 About 入口新建窗口或干扰更新状态。
    pub(super) fn toggle_about_license(&mut self, cx: &mut Context<Self>) {
        self.about_license_expanded = !self.about_license_expanded;
        cx.notify();
    }
}
