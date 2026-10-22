//! cargo-packager updater 的平台适配入口。
//!
//! @author kongweiguang

mod download;
mod metadata;
mod recovery;
mod runtime;
mod settings;

// 多个更新器单元模块共用同一私有 TLS fixture，避免同一文件被按路径重复加载。
#[cfg(test)]
#[path = "../../tests/support/updater_tls_unit.rs"]
mod tls_test_support;

pub use runtime::CargoPackagerUpdater;
pub use settings::UpdaterSettings;
