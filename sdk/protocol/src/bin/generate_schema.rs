//! 生成与当前 Rust 类型配套的静态 schema。
//! @author kongweiguang

use std::{fs, path::Path};

/// 生成器只写协议 crate 的 schema 目录，便于 SDK 检查漂移。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    fs::create_dir_all(&directory)?;
    for (name, schema) in [
        (
            "manifest.schema.json",
            kerminal_plugin_protocol::schema::manifest_json_schema(),
        ),
        (
            "view.schema.json",
            kerminal_plugin_protocol::schema::native_view_json_schema(),
        ),
        (
            "rpc.schema.json",
            kerminal_plugin_protocol::schema::rpc_json_schema(),
        ),
    ] {
        let mut text = serde_json::to_string_pretty(&schema)?;
        text.push('\n');
        fs::write(directory.join(name), text)?;
    }
    Ok(())
}
