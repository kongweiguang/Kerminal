//! 文件路径的系统文本剪贴板动作；文件传输继续使用明确的队列入口。
//!
//! @author kongweiguang

use kerminal_domain::files::FileEndpoint;

use super::FilesPane;
use crate::locale;

impl FilesPane {
    /// 系统文本剪贴板只接收无损路径；成功不打断文件浏览，无法无损复制时才反馈错误。
    ///
    /// @author kongweiguang
    pub(super) fn copy_path_to_clipboard(
        &mut self,
        endpoint: &FileEndpoint,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let path = match endpoint {
            FileEndpoint::Local { path } => path.to_str().map(str::to_owned),
            FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
                Some(path.as_str().to_owned())
            }
        };
        let Some(path) = path.filter(|path| !path.chars().any(char::is_control)) else {
            self.status = Some(
                locale::text(
                    "此路径无法无损复制到文本剪贴板。",
                    "This path cannot be copied losslessly to the text clipboard.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path));
        // 成功由剪贴板本身承载，不新增或覆盖底部状态；真正失败仍保留可见反馈。
    }
}
