//! 扩展详情中的 README 是不可信文本，禁止加载内嵌资源。
//! @author kongweiguang

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
    /// 块级与行内分别拦截，以免 HTML 或图片落回默认资源加载器。
    fn is_block(&self) -> bool {
        self.block
    }
    /// 明确插件名称供 Markdown 解析器区分节点处理器。
    fn name(&self) -> &str {
        "kerminal-extension-readme-disabled-markup"
    }
    /// 禁止不可信 HTML 与图片，同时保留正文中的其他 Markdown 结构。
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

/// README 外链交由系统浏览器打开，并复用已有 HTTPS URL 校验。
pub(super) fn render_readme(id: String, text: String) -> AnyElement {
    TextView::markdown(id, text)
        .markdown_extensions(
            MarkdownExtensions::default()
                .plugin(StripUnsafe { block: false })
                .plugin(StripUnsafe { block: true }),
        )
        .on_link_click(move |url, _, _, cx| {
            if let Some(url) = crate::plugins::external::view::allowed_external_url(url) {
                cx.open_url(url.as_str());
            }
        })
        .into_any_element()
}
