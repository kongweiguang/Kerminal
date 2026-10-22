// @author kongweiguang

use std::env;

/// 固定同一资源目标给 GPUI 与 Windows Shell，避免开发窗口失去图标；仅 MSVC 主窗口增加栈预算。
fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "windows" {
        // GPUI Windows 固定读取当前模块的图标资源 1；开发启动也必须携带此资源。
        println!("cargo:rerun-if-changed=kerminal.rc");
        println!("cargo:rerun-if-changed=../../resources/icons/icon.ico");
        embed_resource::compile_for("kerminal.rc", ["kerminal"], embed_resource::NONE)
            .manifest_required()
            .expect("Windows 应用图标资源编译失败");
    }

    if target_os == "windows" && target_env == "msvc" {
        // Windows 默认 reserve 为 1 MiB；复杂首屏 render tree 可能超过默认预算。
        // Zed 的 Windows build 也采用 8 MiB：
        // https://github.com/zed-industries/zed/blob/main/crates/zed/build.rs
        println!("cargo:rustc-link-arg-bin=kerminal=/STACK:8388608");
    }
}
