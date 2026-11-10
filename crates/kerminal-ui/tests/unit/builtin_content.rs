//! 内置工厂按稳定身份渲染，且不能把一种工作区误当成另一种。
//!
//! @author kongweiguang

use super::{BuiltinContentInput, BuiltinPageRoute, FACTORIES, page_route, render};
use gpui_kit::{IntoElement, div};

/// 真实入口必须能创建元素，未知身份和错误输入应返回明确失败。
#[test]
fn registered_factories_render_only_their_matching_workspace() {
    for id in ["files", "sftpTransfer"] {
        assert!(
            render(
                id,
                BuiltinContentInput::Files {
                    view: div().into_any_element()
                }
            )
            .is_ok()
        );
    }
    for id in ["editor", "workspaceFile"] {
        assert!(
            render(
                id,
                BuiltinContentInput::WorkspaceFile {
                    view: div().into_any_element()
                }
            )
            .is_ok()
        );
    }
    assert!(
        render(
            "terminal",
            BuiltinContentInput::Terminal {
                view: div().into_any_element(),
                broadcast: None,
                inset: 4.0,
            }
        )
        .is_ok()
    );
    for (id, input) in [
        (
            "commands",
            BuiltinContentInput::Commands {
                view: div().into_any_element(),
            },
        ),
        (
            "connections",
            BuiltinContentInput::Connections {
                view: div().into_any_element(),
            },
        ),
        (
            "preferences",
            BuiltinContentInput::Preferences {
                view: div().into_any_element(),
            },
        ),
        (
            "agents",
            BuiltinContentInput::Agents {
                view: div().into_any_element(),
            },
        ),
    ] {
        assert!(render(id, input).is_ok());
    }
    assert!(
        render(
            "terminal",
            BuiltinContentInput::Files {
                view: div().into_any_element()
            }
        )
        .is_err()
    );
    assert!(
        render(
            "unknown",
            BuiltinContentInput::Files {
                view: div().into_any_element()
            }
        )
        .is_err()
    );
    assert!(
        render(
            "commands",
            BuiltinContentInput::Connections {
                view: div().into_any_element()
            },
        )
        .is_err()
    );
}

/// 内置声明与真实绘制工厂必须一一对应，新增页面不能只登记元数据。
///
/// @author kongweiguang
#[test]
fn every_builtin_content_declaration_has_one_render_factory() {
    let registry = kerminal_application::plugins::PluginRegistry::builtin().unwrap();
    // 扩展页已由设置弹框持有，内置声明与轻量绘制桥接表必须严格一一对应。
    assert_eq!(registry.content_types().len(), FACTORIES.len());
    for content in registry.content_types() {
        assert_eq!(
            FACTORIES
                .iter()
                .filter(|(id, _, _)| *id == content.id.as_str())
                .count(),
            1,
        );
    }
}

/// 所有内置声明使用稳定的 typed 导航意图；未知扩展身份继续进入插件内容宿主。
///
/// @author kongweiguang
#[test]
fn builtin_page_routes_preserve_tab_specific_intents() {
    for (id, expected) in [
        ("terminal", BuiltinPageRoute::Terminal),
        ("files", BuiltinPageRoute::Files),
        ("editor", BuiltinPageRoute::Editor),
        ("preferences", BuiltinPageRoute::Preferences),
        ("commands", BuiltinPageRoute::Commands),
        ("connections", BuiltinPageRoute::Connections),
        ("agents", BuiltinPageRoute::Agents),
        ("sftpTransfer", BuiltinPageRoute::SftpTransfer),
        ("workspaceFile", BuiltinPageRoute::WorkspaceFile),
    ] {
        assert_eq!(page_route(id), Some(expected));
    }
    assert_eq!(page_route("example.custom"), None);
}
