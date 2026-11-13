//! 插件 Markdown 只允许文本与合法 HTTPS 链接，禁用 HTML 和图片加载。
//!
//! @author kongweiguang

use super::view::allowed_external_url;
use gpui_kit::{
    AnyElement, IntoElement,
    base::{
        MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, TextView,
        markdown_ast,
    },
};

struct StripUnsafe {
    block: bool,
}
impl MarkdownPlugin for StripUnsafe {
    /// 块级 HTML 与行内 HTML / 图片分别拦截，防止解析器落回默认资源加载路径。
    fn is_block(&self) -> bool {
        self.block
    }
    /// 两个拦截器共用无副作用的文本节点，不登记网络或文件资源。
    fn name(&self) -> &str {
        "kerminal-plugin-disabled-markup"
    }
    /// 只替换不允许的节点，其余语法仍由 Kit 的原生 Markdown 解析器处理。
    fn parse(
        &self,
        node: &markdown_ast::Node,
        _: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let blocked = match node {
            markdown_ast::Node::Html(_) => true,
            markdown_ast::Node::Image(_) | markdown_ast::Node::ImageReference(_) => !self.block,
            _ => false,
        };
        blocked.then(|| MarkdownNode::new(self.name(), ()).text(" "))
    }
}

/// 链接统一走 HTTPS 输入校验，不再按权限集合放行；拦截器仅保留内容渲染安全，
/// 阻止 HTML 与图片被解析成宿主资源，这不是能力门禁。
pub(super) fn render_markdown(id: String, text: String) -> AnyElement {
    let extensions = MarkdownExtensions::default()
        .plugin(StripUnsafe { block: false })
        .plugin(StripUnsafe { block: true });
    TextView::markdown(id, text)
        .markdown_extensions(extensions)
        .on_link_click(move |url, _, _, cx| {
            if let Some(url) = allowed_external_url(url) {
                cx.open_url(url.as_str());
            }
        })
        .into_any_element()
}
