//! 标签组和标签的原生操作菜单。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, Entity, SharedString, Window,
    component::menu::{PopupMenu, PopupMenuItem},
};

use super::super::{
    GroupTabMoveDirection, WorkspaceNameEditTarget, WorkspaceTabEvent, WorkspaceTabKind,
    WorkspaceTabs, group_color_actions, workspace_file_context_action_events,
};

/// 菜单项捕获稳定 View Entity 与完整事件，弹层关闭后仍只向原视图发布意图。
pub(super) fn event_item(
    entity: Entity<WorkspaceTabs>,
    label: impl Into<SharedString>,
    event: WorkspaceTabEvent,
) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, app| {
        let event = event.clone();
        entity.update(app, |_, cx| cx.emit(event));
    })
}

/// 组操作菜单同时服务右键和标题下拉，批量关闭冻结目标并 defer 到 PopupMenu 恢复原焦点后再请求确认。
/// 非空约束仍避免为 v3 无法表达的空组重排伪造成功；颜色项使用可测试的类型化映射。
///
/// @author kongweiguang
pub(super) fn group_action_menu(
    entity: Entity<WorkspaceTabs>,
    group_id: String,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let entity_for_menu = entity.clone();
    let menu_group_id = group_id.clone();
    move |mut menu, window, app| {
        let (group, populated, close_intent, close_outside_intent) = {
            let view = entity_for_menu.read(app);
            let Some(group) = view
                .groups
                .iter()
                .find(|group| group.id == menu_group_id)
                .cloned()
            else {
                return menu;
            };
            let mut populated = view
                .groups
                .iter()
                .filter(|candidate| {
                    view.projection
                        .iter()
                        .any(|tab| tab.group_id.as_deref() == Some(candidate.id.as_str()))
                })
                .cloned()
                .collect::<Vec<_>>();
            let close_intent = view
                .close_group_intent(&menu_group_id)
                .filter(|intent| !intent.tab_ids().is_empty());
            let close_outside_intent = view
                .close_outside_group_intent(&menu_group_id)
                .filter(|intent| !intent.tab_ids().is_empty());
            populated.sort_by_key(|group| group.order);
            (group, populated, close_intent, close_outside_intent)
        };
        let has_members = close_intent
            .as_ref()
            .is_some_and(|intent| !intent.tab_ids().is_empty());
        let position = populated
            .iter()
            .position(|candidate| candidate.id == group.id);
        let move_up = position
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| populated.get(index))
            .map(|before| WorkspaceTabEvent::MoveGroup {
                group_id: group.id.clone(),
                before_group_id: Some(before.id.clone()),
            });
        let move_down = position.and_then(|index| {
            populated
                .get(index + 1)
                .map(|_| WorkspaceTabEvent::MoveGroup {
                    group_id: group.id.clone(),
                    before_group_id: populated.get(index + 2).map(|next| next.id.clone()),
                })
        });
        menu = menu.item(event_item(
            entity.clone(),
            if group.collapsed {
                crate::locale::text("展开分组", "Expand group")
            } else {
                crate::locale::text("折叠分组", "Collapse group")
            },
            WorkspaceTabEvent::SetGroupCollapsed {
                group_id: group.id.clone(),
                collapsed: !group.collapsed,
            },
        ));
        let color_group_id = group.id.clone();
        let color_entity = entity.clone();
        menu = menu.separator().submenu(
            crate::locale::text("分组颜色", "Group color"),
            window,
            app,
            move |mut submenu, _, _| {
                for (label_zh, label_en, event) in group_color_actions(&color_group_id) {
                    submenu = submenu.item(event_item(
                        color_entity.clone(),
                        crate::locale::text(label_zh, label_en),
                        event,
                    ));
                }
                submenu
            },
        );
        if let Some(event) = move_up.clone() {
            menu = menu.item(event_item(
                entity.clone(),
                crate::locale::text("分组上移", "Move group earlier"),
                event,
            ));
        }
        if let Some(event) = move_down.clone() {
            menu = menu.item(event_item(
                entity.clone(),
                crate::locale::text("分组下移", "Move group later"),
                event,
            ));
        }
        let rename_entity = entity.clone();
        let rename_id = group.id.clone();
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text("重命名分组", "Rename group")).on_click(
                move |_, window, app| {
                    let group_id = rename_id.clone();
                    rename_entity.update(app, |tabs, cx| {
                        tabs.begin_group_rename(group_id, window, cx);
                    });
                },
            ),
        );
        if let Some(close_intent) = close_intent {
            let close_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("关闭分组标签", "Close group tabs"))
                    .on_click(move |_, window, app| {
                        let intent = close_intent.clone();
                        let close_entity = close_entity.clone();
                        window.defer(app, move |window, app| {
                            close_entity.update(app, |tabs, cx| {
                                tabs.request_close_batch(intent, window, cx);
                            });
                        });
                    }),
            );
        }
        if let Some(close_outside_intent) = close_outside_intent {
            let close_entity = entity.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "关闭组外其它标签",
                    "Close tabs outside group",
                ))
                .on_click(move |_, window, app| {
                    let intent = close_outside_intent.clone();
                    let close_entity = close_entity.clone();
                    window.defer(app, move |window, app| {
                        close_entity.update(app, |tabs, cx| {
                            tabs.request_close_batch(intent, window, cx);
                        });
                    });
                }),
            );
        }
        menu.item(event_item(
            entity.clone(),
            if has_members {
                crate::locale::text("解散分组", "Ungroup tabs")
            } else {
                crate::locale::text("删除空分组", "Delete empty group")
            },
            WorkspaceTabEvent::Ungroup(group.id.clone()),
        ))
    }
}

/// 标签右键菜单保留全部操作；TabId 与批量关闭目标在菜单打开时固定，关闭请求延至原焦点恢复后派发。
///
/// @author kongweiguang
pub(super) fn tab_action_menu(
    entity: Entity<WorkspaceTabs>,
    tab_id: kerminal_domain::identity::TabId,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let view = entity.clone();
    let tab_for_rename = tab_id.clone();
    move |mut menu, _, app| {
        let (
            current_group_id,
            groups,
            right_intent,
            others_intent,
            group_others_intent,
            can_move_group_tab_earlier,
            can_move_group_tab_later,
            exists,
            tab_kind,
        ) = {
            let tabs = view.read(app);
            let selected_tab = tabs.projection.iter().find(|tab| tab.id == tab_for_rename);
            let exists = selected_tab.is_some();
            let current_group_id = selected_tab.and_then(|tab| tab.group_id.clone());
            let tab_kind = selected_tab.map(|tab| tab.kind.clone());
            let group_tab_ids = current_group_id
                .as_ref()
                .map(|group_id| {
                    tabs.projection
                        .iter()
                        .filter(|tab| tab.group_id.as_ref() == Some(group_id))
                        .map(|tab| tab.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let group_tab_position = group_tab_ids
                .iter()
                .position(|candidate| candidate == &tab_for_rename);
            (
                current_group_id,
                tabs.groups.clone(),
                tabs.close_right_intent(&tab_for_rename)
                    .filter(|intent| !intent.tab_ids().is_empty()),
                tabs.close_others_intent(&tab_for_rename)
                    .filter(|intent| !intent.tab_ids().is_empty()),
                tabs.close_group_others_intent(&tab_for_rename)
                    .filter(|intent| !intent.tab_ids().is_empty()),
                group_tab_position.is_some_and(|position| position > 0),
                group_tab_position.is_some_and(|position| position + 1 < group_tab_ids.len()),
                exists,
                tab_kind,
            )
        };
        if !exists {
            return menu;
        }
        if tab_kind == Some(WorkspaceTabKind::SftpWorkbench) {
            let close_entity = view.clone();
            let close_id = tab_for_rename.clone();
            return menu.item(
                PopupMenuItem::new(crate::locale::text("关闭工作台", "Close workbench")).on_click(
                    move |_, window, app| {
                        let tab_id = close_id.clone();
                        let close_entity = close_entity.clone();
                        window.defer(app, move |window, app| {
                            close_entity.update(app, |tabs, cx| {
                                tabs.request_close_tab(tab_id, window, cx);
                            });
                        });
                    },
                ),
            );
        }
        if let Some(tab_kind) = tab_kind {
            for event in workspace_file_context_action_events(&tab_kind, &tab_for_rename) {
                let WorkspaceTabEvent::FileAction { action, .. } = &event else {
                    continue;
                };
                let (label_zh, label_en) = match action {
                    super::super::WorkspaceFileTabAction::CopyFullPath => {
                        ("复制完整路径", "Copy full path")
                    }
                    super::super::WorkspaceFileTabAction::Reload => ("重新加载", "Reload"),
                };
                menu = menu.item(event_item(
                    view.clone(),
                    crate::locale::text(label_zh, label_en),
                    event,
                ));
            }
        }
        let rename_entity = view.clone();
        let rename_id = tab_for_rename.clone();
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text("重命名", "Rename")).on_click(
                move |_, window, app| {
                    let tab_id = rename_id.clone();
                    rename_entity.update(app, |tabs, cx| {
                        tabs.begin_tab_rename(tab_id, window, cx);
                    });
                },
            ),
        );

        let create_entity = view.clone();
        let create_tab_id = tab_for_rename.clone();
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text(
                "从此标签创建分组",
                "Create group from tab",
            ))
            .on_click(move |_, window, app| {
                let tab_id = create_tab_id.clone();
                create_entity.update(app, |tabs, cx| {
                    tabs.begin_name_edit(
                        WorkspaceNameEditTarget::CreateGroup { tab_id },
                        String::new(),
                        crate::locale::text("分组名称", "Group name"),
                        window,
                        cx,
                    );
                });
            }),
        );

        if current_group_id.is_some() {
            menu = menu.item(event_item(
                view.clone(),
                crate::locale::text("移出分组", "Move out of group"),
                WorkspaceTabEvent::MoveToGroup {
                    tab_id: tab_for_rename.clone(),
                    group_id: None,
                },
            ));
        }
        for group in &groups {
            if current_group_id.as_deref() == Some(group.id.as_str()) {
                continue;
            }
            menu = menu.item(event_item(
                view.clone(),
                crate::locale::text(
                    format!("移入分组：{}", group.title),
                    format!("Move to group: {}", group.title),
                ),
                WorkspaceTabEvent::MoveToGroup {
                    tab_id: tab_for_rename.clone(),
                    group_id: Some(group.id.clone()),
                },
            ));
        }

        if let Some(group_id) = current_group_id.as_ref() {
            if can_move_group_tab_earlier {
                menu = menu.item(event_item(
                    view.clone(),
                    crate::locale::text("组内向左移动", "Move left in group"),
                    WorkspaceTabEvent::MoveWithinGroup {
                        tab_id: tab_for_rename.clone(),
                        group_id: group_id.clone(),
                        direction: GroupTabMoveDirection::Earlier,
                    },
                ));
            }
            if can_move_group_tab_later {
                menu = menu.item(event_item(
                    view.clone(),
                    crate::locale::text("组内向右移动", "Move right in group"),
                    WorkspaceTabEvent::MoveWithinGroup {
                        tab_id: tab_for_rename.clone(),
                        group_id: group_id.clone(),
                        direction: GroupTabMoveDirection::Later,
                    },
                ));
            }
        }

        let close_entity = view.clone();
        let close_id = tab_for_rename.clone();
        menu = menu.item(
            PopupMenuItem::new(crate::locale::text("关闭", "Close")).on_click(
                move |_, window, app| {
                    let tab_id = close_id.clone();
                    let close_entity = close_entity.clone();
                    window.defer(app, move |window, app| {
                        close_entity.update(app, |tabs, cx| {
                            tabs.request_close_tab(tab_id, window, cx);
                        });
                    });
                },
            ),
        );

        if let Some(intent) = group_others_intent {
            let group_others_entity = view.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "关闭同组其他标签",
                    "Close other tabs in group",
                ))
                .on_click(move |_, window, app| {
                    let intent = intent.clone();
                    let group_others_entity = group_others_entity.clone();
                    window.defer(app, move |window, app| {
                        group_others_entity.update(app, |tabs, cx| {
                            tabs.request_close_batch(intent, window, cx);
                        });
                    });
                }),
            );
        }

        if let Some(intent) = right_intent {
            let right_entity = view.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text(
                    "关闭右侧标签",
                    "Close tabs to the right",
                ))
                .on_click(move |_, window, app| {
                    let intent = intent.clone();
                    let right_entity = right_entity.clone();
                    window.defer(app, move |window, app| {
                        right_entity.update(app, |tabs, cx| {
                            tabs.request_close_batch(intent, window, cx);
                        });
                    });
                }),
            );
        }
        if let Some(intent) = others_intent {
            let others_entity = view.clone();
            menu = menu.item(
                PopupMenuItem::new(crate::locale::text("关闭其他标签", "Close other tabs"))
                    .on_click(move |_, window, app| {
                        let intent = intent.clone();
                        let others_entity = others_entity.clone();
                        window.defer(app, move |window, app| {
                            others_entity.update(app, |tabs, cx| {
                                tabs.request_close_batch(intent, window, cx);
                            });
                        });
                    }),
            );
        }
        menu
    }
}

/// 标签类型按真实投影显示，未知 future 类型保留名称且不伪装为可关闭终端。
pub(super) fn tab_kind_label(kind: &WorkspaceTabKind) -> SharedString {
    match kind {
        WorkspaceTabKind::Terminal => crate::locale::text("终端", "Terminal"),
        WorkspaceTabKind::SftpTransfer => crate::locale::text("传输", "Transfer"),
        WorkspaceTabKind::SftpWorkbench => crate::locale::text("SFTP", "SFTP"),
        WorkspaceTabKind::WorkspaceFile => crate::locale::text("文件", "File"),
        WorkspaceTabKind::Unsupported(name) => {
            crate::locale::text(format!("不支持：{name}"), format!("Unsupported: {name}"))
        }
    }
}
