//! 插件 Markdown 只允许文本与经权限校验的链接，禁用 HTML 和图片加载。
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
use kerminal_plugin_protocol::Permission;
use std::collections::BTreeSet;

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

/// 所有链接都经过声明权限和 HTTPS 约束；拦截器阻止 HTML 与图片被解析成资源。
pub(super) fn render_markdown(
    id: String,
    text: String,
    permissions: BTreeSet<Permission>,
) -> AnyElement {
    let extensions = MarkdownExtensions::default()
        .plugin(StripUnsafe { block: false })
        .plugin(StripUnsafe { block: true });
    TextView::markdown(id, text)
        .markdown_extensions(extensions)
        .on_link_click(move |url, _, _, cx| {
            if let Some(url) = allowed_external_url(&permissions, url) {
                cx.open_url(url.as_str());
            }
        })
        .into_any_element()
}
