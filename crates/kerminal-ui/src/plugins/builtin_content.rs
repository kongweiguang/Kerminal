//! 内置内容的真实绘制工厂；资源仍由既有 typed 工作区持有。
//!
//! @author kongweiguang

use gpui_kit::{AnyElement, IntoElement, div, prelude::*, px};
use kerminal_application::{AppError, AppResult};

/// 输入只携带现有工作区已准备好的视图，不允许绘制工厂取得 Root 或创建资源。
pub(crate) enum BuiltinContentInput {
    Terminal {
        view: AnyElement,
        broadcast: Option<AnyElement>,
        inset: f32,
    },
    Files {
        view: AnyElement,
    },
    WorkspaceFile {
        view: AnyElement,
    },
    Commands {
        view: AnyElement,
    },
    Connections {
        view: AnyElement,
    },
    Preferences {
        view: AnyElement,
    },
    Agents {
        view: AnyElement,
    },
}

type BuiltinContentFactory = fn(BuiltinContentInput) -> AppResult<AnyElement>;

/// 页面动作只表达既有导航意图；具体焦点、模态及标签身份仍由 Root 校验和管理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinPageRoute {
    Terminal,
    Files,
    Editor,
    Preferences,
    Commands,
    Connections,
    Agents,
    /// 传输标签必须依赖当前实际标签，不能凭页面动作新建或猜测传输目标。
    SftpTransfer,
    /// 工作区文件标签必须依赖当前实际标签，不能凭页面动作新建或猜测文件。
    WorkspaceFile,
}

/// 稳定贡献身份映射到真正绘制内容的工厂；同一文件面板可作为页面或传输标签。
const FACTORIES: &[(&str, BuiltinContentFactory, BuiltinPageRoute)] = &[
    ("terminal", render_terminal, BuiltinPageRoute::Terminal),
    ("files", render_files, BuiltinPageRoute::Files),
    ("sftpTransfer", render_files, BuiltinPageRoute::SftpTransfer),
    ("editor", render_workspace_file, BuiltinPageRoute::Editor),
    (
        "workspaceFile",
        render_workspace_file,
        BuiltinPageRoute::WorkspaceFile,
    ),
    ("commands", render_commands, BuiltinPageRoute::Commands),
    (
        "connections",
        render_connections,
        BuiltinPageRoute::Connections,
    ),
    (
        "preferences",
        render_preferences,
        BuiltinPageRoute::Preferences,
    ),
    ("agents", render_agents, BuiltinPageRoute::Agents),
];

/// 只识别内置身份，扩展贡献继续交给内容宿主解析和创建。
///
/// @author kongweiguang
pub(crate) fn page_route(id: &str) -> Option<BuiltinPageRoute> {
    FACTORIES
        .iter()
        .find_map(|(registered, _, route)| (*registered == id).then_some(*route))
}

/// 按注册身份选择绘制工厂；未知身份和类型不匹配都显式失败。
pub(crate) fn render(id: &str, input: BuiltinContentInput) -> AppResult<AnyElement> {
    let factory = FACTORIES
        .iter()
        .find_map(|(registered, factory, _)| (*registered == id).then_some(factory))
        .ok_or_else(|| AppError::not_found("内置内容工厂不可用"))?;
    factory(input)
}

/// 终端保留原有广播栏和密度间距，实体本身仍由工作区运行时保持。
fn render_terminal(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Terminal {
        view,
        broadcast,
        inset,
    } = input
    else {
        return Err(AppError::invalid("内置终端内容输入不匹配"));
    };
    Ok(div()
        .id("active-terminal-workspace")
        .size_full()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .pt(px(if broadcast.is_some() { inset } else { 0. }))
        .children(broadcast)
        .child(
            div()
                .id("terminal-pane-region")
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .py(px(inset))
                .overflow_hidden()
                .child(view),
        )
        .into_any_element())
}

/// 文件页面和 SFTP 传输标签共用容器，不转移已有面板实体的所有权。
fn render_files(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Files { view } = input else {
        return Err(AppError::invalid("内置文件内容输入不匹配"));
    };
    Ok(div()
        .id("files-workspace")
        .size_full()
        .child(view)
        .into_any_element())
}

/// 编辑器维持原有共享实体和脏文件门禁，工厂只负责挂载它。
fn render_workspace_file(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::WorkspaceFile { view } = input else {
        return Err(AppError::invalid("内置工作区文件内容输入不匹配"));
    };
    Ok(div()
        .id("editor-workspace")
        .size_full()
        .child(view)
        .into_any_element())
}

/// 命令页面维持既有实体和尺寸约束，执行动作仍由原面板处理。
fn render_commands(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Commands { view } = input else {
        return Err(AppError::invalid("内置命令页面输入不匹配"));
    };
    Ok(div()
        .id("commands-workspace")
        .size_full()
        .child(view)
        .into_any_element())
}

/// 连接页面只挂载现有实体，主机编辑和模态所有权仍由 Root 管理。
fn render_connections(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Connections { view } = input else {
        return Err(AppError::invalid("内置连接页面输入不匹配"));
    };
    Ok(div()
        .id("connections-workspace")
        .size_full()
        .child(view)
        .into_any_element())
}

/// 设置目前位于模态窗口；保留既有中央占位的布局和文案。
fn render_preferences(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Preferences { view } = input else {
        return Err(AppError::invalid("内置设置页面输入不匹配"));
    };
    Ok(view)
}

/// Agent 会话仍位于右栏；中央占位直接沿用调用方准备的元素。
fn render_agents(input: BuiltinContentInput) -> AppResult<AnyElement> {
    let BuiltinContentInput::Agents { view } = input else {
        return Err(AppError::invalid("内置 Agent 页面输入不匹配"));
    };
    Ok(view)
}

#[cfg(test)]
#[path = "../../tests/unit/builtin_content.rs"]
mod tests;
