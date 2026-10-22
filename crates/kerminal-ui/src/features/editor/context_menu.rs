//! 文件编辑器右键菜单复用 GPUI Kit PopupMenu，统一主题和键盘操作上下文。
//!
//! @author kongweiguang

use gpui_kit::component::{
    input::{Copy, Cut, Paste, Redo, SelectAll, Undo},
    menu::{PopupMenu, PopupMenuItem},
};
use gpui_kit::{Entity, FocusHandle};
use kerminal_domain::files::FileEndpoint;
use kerminal_domain::files::editor::SaveResolution;

use super::workspace::FileEditorWorkspace;
use crate::locale;

/// 为编辑区装配带只读门禁的菜单项，动作仍分发给原编辑器以保留光标和选择。
///
/// @author kongweiguang
pub(super) fn editor_context_menu(
    menu: PopupMenu,
    editor_focus: FocusHandle,
    workspace: Entity<FileEditorWorkspace>,
    can_edit: bool,
    can_save: bool,
) -> PopupMenu {
    let shortcut_prefix = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+"
    };
    let find_label = locale::text(
        format!("查找（{shortcut_prefix}F）"),
        format!("Find ({shortcut_prefix}F)"),
    );
    let replace_label = locale::text(
        format!("查找并替换（{shortcut_prefix}H）"),
        format!("Find and replace ({shortcut_prefix}H)"),
    );
    let save_label = locale::text(
        format!("保存（{shortcut_prefix}S）"),
        format!("Save ({shortcut_prefix}S)"),
    );
    let find_workspace = workspace.clone();
    let replace_workspace = workspace.clone();
    let save_workspace = workspace;

    menu.action_context(editor_focus)
        .item(
            PopupMenuItem::new(locale::text("撤销", "Undo"))
                .action(Box::new(Undo))
                .disabled(!can_edit),
        )
        .item(
            PopupMenuItem::new(locale::text("重做", "Redo"))
                .action(Box::new(Redo))
                .disabled(!can_edit),
        )
        .separator()
        .item(
            PopupMenuItem::new(locale::text("剪切", "Cut"))
                .action(Box::new(Cut))
                .disabled(!can_edit),
        )
        .item(PopupMenuItem::new(locale::text("复制", "Copy")).action(Box::new(Copy)))
        .item(
            PopupMenuItem::new(locale::text("粘贴（Shift+Insert）", "Paste (Shift+Insert)"))
                .action(Box::new(Paste))
                .disabled(!can_edit),
        )
        .item(PopupMenuItem::new(locale::text("全选", "Select all")).action(Box::new(SelectAll)))
        .separator()
        .item(
            PopupMenuItem::new(find_label).on_click(move |_, window, cx| {
                find_workspace.update(cx, |workspace, cx| {
                    workspace.show_find_search(window, cx);
                });
            }),
        )
        .item(
            PopupMenuItem::new(replace_label)
                .disabled(!can_edit)
                .on_click(move |_, window, cx| {
                    replace_workspace.update(cx, |workspace, cx| {
                        workspace.show_replace_search(window, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(save_label)
                .disabled(!can_save)
                .on_click(move |_, _, cx| {
                    save_workspace.update(cx, |workspace, cx| {
                        workspace.save_active(SaveResolution::CompareRevision, false, false, cx);
                    });
                }),
        )
}

/// 为编辑器内部文件标签提供路径复制和安全重载；目标取右键时捕获的 endpoint 与读取代次。
///
/// @author kongweiguang
pub(super) fn editor_tab_context_menu(
    menu: PopupMenu,
    workspace: Entity<FileEditorWorkspace>,
    endpoint: FileEndpoint,
    load_generation: u64,
) -> PopupMenu {
    let copy_workspace = workspace.clone();
    let copy_endpoint = endpoint.clone();
    menu.item(
        PopupMenuItem::new(locale::text("复制完整路径", "Copy full path")).on_click(
            move |_, _, cx| {
                copy_workspace.update(cx, |workspace, cx| {
                    workspace.copy_file_path_for_generation(
                        copy_endpoint.clone(),
                        load_generation,
                        cx,
                    );
                });
            },
        ),
    )
    .item(
        PopupMenuItem::new(locale::text("重新加载", "Reload")).on_click(move |_, _, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.request_user_reload(endpoint.clone(), Some(load_generation), cx);
            });
        }),
    )
}
