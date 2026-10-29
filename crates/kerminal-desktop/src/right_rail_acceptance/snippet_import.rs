//! 以实际 JSON 文件驱动片段导入预览，验证其根模态宿主和取消闭环。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_ui::RootView;

use super::{DialogCaptureRecord, RightRailCaptureContext};

pub(super) struct FunctionalSnippetImportSample {
    pub(super) path: PathBuf,
    pub(super) file_name: &'static str,
    pub(super) title: String,
    pub(super) command: String,
}

/// 为隔离验收配置生成唯一导入样本；其命令只作为定义数据保存，绝不发送给终端。
///
/// @author kongweiguang
pub(super) fn write_functional_sample(
    output_directory: &Path,
) -> Result<FunctionalSnippetImportSample, Box<dyn Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let file_name = "snippet-import-functional.json";
    let title = format!("右栏片段导入验收-{nonce}");
    let command = format!("acceptance-import-only-{nonce}");
    let path = output_directory.join(file_name);
    let source = serde_json::json!({
        "schemaVersion": 1,
        "snippets": [{
            "title": title,
            "command": command,
            "description": "仅验证生产导入与存储回读，不执行命令",
            "tags": ["acceptance"],
            "scope": "ssh",
            "risk": "inspect",
            "defaultAction": "insert",
            "variables": [],
            "contextBindings": []
        }]
    });
    fs::write(&path, serde_json::to_vec_pretty(&source)?)?;
    Ok(FunctionalSnippetImportSample {
        path,
        file_name,
        title,
        command,
    })
}

/// 文件选择器由隔离输出路径替代，文件读取、候选校验、模态显示与 Escape 均使用生产路径。
/// 只预览一个只读片段后取消，不向片段目录导入，也不向终端发送命令。
///
/// @author kongweiguang
pub(super) async fn capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &RightRailCaptureContext,
    theme_name: &str,
) -> Result<DialogCaptureRecord, Box<dyn Error>> {
    let file_name = "snippet-import-preview.json";
    let source_path = context.output_directory.join(file_name);
    let source = serde_json::json!({
        "schemaVersion": 1,
        "snippets": [{
            "title": "右栏导入预览验收",
            "command": "pwd",
            "description": "仅预览后取消，不导入、不执行",
            "tags": ["acceptance"],
            "scope": "ssh",
            "risk": "inspect",
            "defaultAction": "insert",
            "variables": [],
            "contextBindings": []
        }]
    });
    fs::write(&source_path, serde_json::to_vec_pretty(&source)?)?;
    let accepted = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.preview_right_rail_acceptance_snippet_import(
                &context.fixture_host_id,
                source_path,
                cx,
            )
        })
    })??;
    if !accepted {
        return Err(io::Error::other("片段导入预览未由生产读取路径受理").into());
    }
    super::wait_for_acceptance_state(cx, root, window, "snippet import preview", |root, cx| {
        root.right_rail_acceptance_snippet_import_ready(&context.fixture_host_id, file_name, cx)
    })
    .await?;
    let capture = super::wait_for_stable_frame(
        cx,
        window,
        context
            .output_directory
            .join(format!("dialog-{theme_name}-snippet-import-preview.png")),
        None,
        "snippet import preview",
    )
    .await?;
    super::dismiss_dialog_with_native_keys(cx, window)?;
    super::wait_for_acceptance_dialog_closed(
        cx,
        root,
        window,
        "snippet import preview",
        |root, cx| root.right_rail_acceptance_snippet_editor_closed(cx),
    )
    .await?;
    Ok(DialogCaptureRecord {
        surface: "snippet-import-preview",
        capture,
        open: true,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    })
}
