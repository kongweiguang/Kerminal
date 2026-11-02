//! 生产构建的 GitHub 更新源与编译期信任根。
//!
//! @author kongweiguang

use kerminal_application::AppResult;
use kerminal_infrastructure::platform::updater::UpdaterSettings;

/// 更新地址固定为本项目的独立 GPUI 清单，避免旧 Tauri feed 被误当成新格式。
const RELEASE_ENDPOINT: &str =
    "https://github.com/kongweiguang/Kerminal/releases/latest/download/kerminal-updater.json";

/// 只接受构建时注入的公钥；未配置的开发候选保留明确的不可用状态与恢复入口。
///
/// @author kongweiguang
pub(crate) fn production_updater_settings() -> AppResult<Option<UpdaterSettings>> {
    settings_for_build(
        env!("CARGO_PKG_VERSION"),
        option_env!("KERMINAL_GPUI_UPDATER_PUBLIC_KEY"),
    )
}

/// 将缺失与无效信任根区分开，使签名发布流程能在构建前拒绝无效配置。
///
/// @author kongweiguang
fn settings_for_build(
    current_version: &str,
    public_key: Option<&str>,
) -> AppResult<Option<UpdaterSettings>> {
    match public_key {
        None | Some("") => Ok(None),
        Some(key) => {
            UpdaterSettings::from_release_config(current_version, RELEASE_ENDPOINT, key).map(Some)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/update_configuration.rs"]
mod tests;
