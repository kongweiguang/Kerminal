//! 设置弹框内嵌三栏扩展页；操作状态不改变列表行高。
//! @author kongweiguang

use super::{Confirmation, ExtensionsEvent, ExtensionsPage, Filter, Operation, label, selection};
use crate::theme::ThemeTokens;
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, Render, StatefulInteractiveElement as _, Window,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
        input::Input,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::extensions::{ExtensionChange, InstallProgress};
use std::collections::BTreeSet;

impl Render for ExtensionsPage {
    /// 三列分别承载筛选、稳定行高的结果和所选条目详情；写操作由共享服务执行。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 统一经设置主题入口取色；该入口当前可能没有视觉差异，保留以便后续主题演进。
        let theme = ThemeTokens::current(window, cx).for_settings();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut categories = BTreeSet::new();
        if let Some(catalog) = &self.catalog {
            for plugin in &catalog.catalog.plugins {
                categories.extend(plugin.categories.iter().cloned());
            }
        }
        let installed = self.state.as_ref().map_or(0, |state| state.plugins.len());
        let pending = self.state.as_ref().map_or(0, |state| state.pending.len());
        let mut sidebar = div()
            .id("extensions-filter-sidebar")
            .w(px(192.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .min_h(px(0.))
            .overflow_y_scroll()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(Input::new(&self.search).w_full())
            .child(filter_button(
                "extensions-all",
                tr("全部", "All").into(),
                self.filter == Filter::All,
                Filter::All,
                cx,
            ))
            .child(filter_button(
                "extensions-installed",
                format!("{} ({installed})", tr("已安装", "Installed")),
                self.filter == Filter::Installed,
                Filter::Installed,
                cx,
            ))
            .child(filter_button(
                "extensions-updates",
                tr("可更新", "Updates available").into(),
                self.filter == Filter::Updates,
                Filter::Updates,
                cx,
            ));
        for category in categories {
            sidebar = sidebar.child(filter_button(
                format!("extensions-category-{category}"),
                category.clone(),
                self.filter == Filter::Category(category.clone()),
                Filter::Category(category),
                cx,
            ));
        }
        sidebar = sidebar.child(div().flex_1()).child(
            Button::new("extensions-file")
                .ghost()
                .label(tr("从文件安装…", "Install from file…"))
                .disabled(self.busy)
                .on_click(cx.listener(|page, _, _, cx| page.choose_file(cx))),
        );

        let mut list = div()
            .id("extensions-results")
            .flex_1()
            .min_w(px(260.))
            .h_full()
            .flex()
            .flex_col()
            .overflow_y_scroll();
        if let Some(catalog) = &self.catalog {
            for plugin in &catalog.catalog.plugins {
                let current = self
                    .state
                    .as_ref()
                    .and_then(|state| state.plugins.get(&plugin.id));
                let update = current.zip(selection(plugin, &self.host_api)).is_some_and(
                    |(installed, available)| {
                        available.version.as_semver() > installed.active.as_semver()
                    },
                );
                let name = label(&plugin.name, plugin.id.as_str());
                let description = label(&plugin.description, "");
                if !query.is_empty()
                    && !format!(
                        "{name} {} {} {description}",
                        plugin.id.as_str(),
                        plugin.publisher
                    )
                    .to_lowercase()
                    .contains(&query)
                {
                    continue;
                }
                if match &self.filter {
                    Filter::All => false,
                    Filter::Installed => current.is_none(),
                    Filter::Updates => !update,
                    Filter::Category(category) => !plugin.categories.contains(category),
                } {
                    continue;
                }
                let id = plugin.id.clone();
                let pending_change = self.state.as_ref().is_some_and(|state| {
                    state.pending.iter().any(|change| change.plugin_id() == &id)
                });
                let broken = self
                    .state
                    .as_ref()
                    .is_some_and(|state| state.failures.contains_key(&id));
                let row_status = if broken {
                    tr("损坏", "Broken")
                } else if pending_change {
                    tr("重启生效", "Restart required")
                } else if update {
                    tr("可更新", "Updates available")
                } else if let Some(current) = current {
                    if current.enabled {
                        tr("已启用", "Enabled")
                    } else {
                        tr("已禁用", "Disabled")
                    }
                } else if selection(plugin, &self.host_api).is_none() {
                    tr("不支持当前平台", "Not supported on this platform")
                } else {
                    tr("可安装", "Install")
                };
                let selected = self.selected.as_ref() == Some(&id);
                let install = selection(plugin, &self.host_api);
                let actionable = install.is_some()
                    && (current.is_none() || update)
                    && !pending_change
                    && !broken
                    && !self.offline
                    && !self.busy;
                let installing = self.active_install.as_ref() == Some(&id);
                let progress = self
                    .progress
                    .as_ref()
                    .map(progress_label)
                    .unwrap_or_default();
                let select_id = id.clone();
                let name_button = Button::new(format!("extension-select-{}", id.as_str()))
                    .ghost()
                    .label(name.to_owned())
                    .on_click(
                        cx.listener(move |page, _, _, cx| page.select(select_id.clone(), cx)),
                    );
                let action = Button::new(format!("extension-action-{}", id.as_str()))
                    .ghost()
                    .label(if installing {
                        format!("{} · {}", progress, tr("取消", "Cancel"))
                    } else {
                        row_status.to_owned()
                    })
                    .disabled(if installing {
                        self.cancel.is_none()
                    } else {
                        !actionable
                    })
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if installing {
                            page.cancel();
                            cx.notify();
                        } else if let Some(target) = install.clone() {
                            page.confirmation = Some(Confirmation::Install(target));
                            cx.notify();
                        }
                    }));
                list = list.child(
                    div()
                        .id(format!("extension-row-{}", id.as_str()))
                        .h(px(72.))
                        .flex_shrink_0()
                        .px_4()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(if selected {
                            theme.selected
                        } else {
                            theme.background
                        })
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.select(id.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(name_button)
                                .child(action),
                        )
                        .child(
                            div()
                                .text_color(theme.muted)
                                .text_sm()
                                .child(description.to_owned()),
                        ),
                );
            }
        }
        // 本地安装或目录下架的插件仍属于用户的已安装列表。
        if let Some(state) = &self.state {
            for (id, installed) in &state.plugins {
                if self.catalog.as_ref().is_some_and(|catalog| {
                    catalog
                        .catalog
                        .plugins
                        .iter()
                        .any(|plugin| &plugin.id == id)
                }) || !matches!(&self.filter, Filter::All | Filter::Installed)
                    || (!query.is_empty() && !id.as_str().contains(&query))
                {
                    continue;
                }
                let selected = self.selected.as_ref() == Some(id);
                let row_id = id.clone();
                let state_label = if state.pending.iter().any(|change| change.plugin_id() == id) {
                    tr("重启生效", "Restart required")
                } else if installed.enabled {
                    tr("已启用", "Enabled")
                } else {
                    tr("已禁用", "Disabled")
                };
                list = list.child(
                    div()
                        .id(format!("extension-row-{}", id.as_str()))
                        .h(px(72.))
                        .flex_shrink_0()
                        .px_4()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(if selected {
                            theme.selected
                        } else {
                            theme.background
                        })
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.select(row_id.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(id.as_str().to_owned())
                                .child(div().text_color(theme.muted).child(state_label)),
                        )
                        .child(div().text_color(theme.muted).text_sm().child(tr(
                            "本地安装或目录中不可用",
                            "Installed locally or unavailable in the catalog",
                        ))),
                );
            }
        }
        // 本地包首次安装尚未重启时没有 installed 条目，仍需展示排队事实。
        if let Some(state) = &self.state {
            for change in &state.pending {
                let id = change.plugin_id();
                if state.plugins.contains_key(id)
                    || self.catalog.as_ref().is_some_and(|catalog| {
                        catalog
                            .catalog
                            .plugins
                            .iter()
                            .any(|plugin| &plugin.id == id)
                    })
                    || !matches!(&self.filter, Filter::All | Filter::Installed)
                    || (!query.is_empty() && !id.as_str().contains(&query))
                {
                    continue;
                }
                let selected_id = id.clone();
                list = list.child(
                    div()
                        .id(format!("extension-row-{}", id.as_str()))
                        .h(px(72.))
                        .on_click(
                            cx.listener(move |page, _, _, cx| page.select(selected_id.clone(), cx)),
                        )
                        .flex_shrink_0()
                        .px_4()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(id.as_str().to_owned())
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted)
                                .child(tr("等待重启后安装", "Installation pending restart")),
                        ),
                );
            }
        }
        if self.catalog.is_none() {
            list = list.child(
                div()
                    .p_4()
                    .text_color(theme.muted)
                    .child(tr("目录尚未加载。", "The catalog has not loaded.")),
            );
        }
        let mut detail = div()
            .w(px(330.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .id("extension-detail")
            .overflow_y_scroll();
        if let Some(plugin) = self.selected_plugin().cloned() {
            let current = self
                .state
                .as_ref()
                .and_then(|state| state.plugins.get(&plugin.id))
                .cloned();
            let pending_change = self.state.as_ref().is_some_and(|state| {
                state
                    .pending
                    .iter()
                    .any(|change| change.plugin_id() == &plugin.id)
            });
            let offline = self.offline;
            detail = detail
                .child(
                    div()
                        .text_lg()
                        .text_color(theme.foreground)
                        .child(label(&plugin.name, plugin.id.as_str()).to_owned()),
                )
                .child(div().text_sm().text_color(theme.muted).child(format!(
                    "{} · {}",
                    plugin.publisher,
                    plugin.id.as_str()
                )))
                .child(
                    div()
                        .text_color(theme.foreground)
                        .child(label(&plugin.description, "").to_owned()),
                );
            if let Some(available) = selection(&plugin, &self.host_api) {
                let installable = current.as_ref().is_none_or(|installed| {
                    available.version.as_semver() > installed.active.as_semver()
                });
                if installable {
                    let caption = if current.is_some() {
                        format!("{} {}", tr("更新到", "Update to"), available.version)
                    } else {
                        tr("安装", "Install").into()
                    };
                    detail = detail.child(
                        Button::new("extension-install")
                            .primary()
                            .label(caption)
                            .disabled(self.busy || pending_change || offline)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.confirmation = Some(Confirmation::Install(available.clone()));
                                cx.notify();
                            })),
                    );
                }
            } else {
                detail = detail.child(
                    div()
                        .text_color(theme.muted)
                        .child(tr("不支持当前平台", "Not supported on this platform")),
                );
            }
            if let Some(installed) = current {
                detail = detail.child(div().text_sm().text_color(theme.muted).child(format!(
                    "{} {}",
                    tr("当前版本", "Current version"),
                    installed.active
                )));
                let id = plugin.id.clone();
                let enabled = installed.enabled;
                detail = detail.child(
                    Button::new("extension-toggle")
                        .ghost()
                        .label(if enabled {
                            tr("禁用", "Disable")
                        } else {
                            tr("启用", "Enable")
                        })
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(if enabled {
                                    ExtensionChange::Disable(id.clone())
                                } else {
                                    ExtensionChange::Enable(id.clone())
                                }),
                                cx,
                            )
                        })),
                );
                if let Some(previous) = installed.previous {
                    let id = plugin.id.clone();
                    detail = detail.child(
                        Button::new("extension-rollback")
                            .ghost()
                            .label(format!("{} {previous}", tr("回退到", "Roll back to")))
                            .disabled(self.busy || pending_change)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.run_operation(
                                    Operation::Change(ExtensionChange::Rollback(id.clone())),
                                    cx,
                                )
                            })),
                    );
                }
                let id = plugin.id.clone();
                detail = detail.child(
                    Button::new("extension-uninstall")
                        .ghost()
                        .label(tr("卸载", "Uninstall"))
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::Uninstall(id.clone())),
                                cx,
                            )
                        })),
                );
                let id = plugin.id.clone();
                detail = detail.child(
                    Button::new("extension-delete-data")
                        .ghost()
                        .label(tr("删除插件数据…", "Delete extension data…"))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.confirmation = Some(Confirmation::DeleteData(id.clone()));
                            cx.notify();
                        })),
                );
            }
            if let Some(available) = selection(&plugin, &self.host_api) {
                detail = detail.child(div().text_sm().text_color(theme.muted).child(format!(
                    "{} {}",
                    tr("可用版本", "Available version"),
                    available.version
                )));
                if let Some(url) = plugin
                    .versions
                    .iter()
                    .find(|version| version.version == available.version)
                    .and_then(|version| version.changelog_url.clone())
                {
                    detail = detail.child(external_link(
                        "extension-changelog",
                        tr("更新日志", "Changelog"),
                        url,
                        cx,
                    ));
                }
            }
            detail = detail.child(external_link(
                "extension-repository",
                tr("仓库", "Repository"),
                plugin.repository.clone(),
                cx,
            ));
            if let Some(readme) = &self.readme {
                detail = detail.child(super::markdown::render_readme(
                    format!("extension-readme-{}", plugin.id.as_str()),
                    readme.clone(),
                ));
            }
            let full_access = selection(&plugin, &self.host_api)
                .and_then(|selected| {
                    plugin
                        .versions
                        .iter()
                        .find(|version| version.version == selected.version)
                })
                .is_some_and(|version| version.requires_full_access());
            detail = detail.child(div().text_sm().text_color(theme.foreground).child(
                if full_access {
                    tr(
                        "完全访问：此插件以当前用户权限运行",
                        "Full access: this extension runs with the current user's permissions",
                    )
                } else {
                    tr(
                        "受限访问：通过声明的宿主权限运行",
                        "Restricted access: uses declared host permissions",
                    )
                },
            ));
            for permission in &plugin.permissions {
                detail = detail.child(div().text_sm().text_color(theme.muted).child(format!(
                    "{} — {}",
                    permission,
                    permission_description(permission)
                )));
            }
            let permissions = if plugin.permissions.is_empty() {
                tr("无额外权限", "No additional permissions").to_owned()
            } else {
                plugin.permissions.join(" · ")
            };
            detail = detail.child(
                div()
                    .text_sm()
                    .text_color(theme.muted)
                    .child(format!("{}: {permissions}", tr("权限", "Permissions"))),
            );
        } else if let Some((id, installed)) = self.selected.as_ref().and_then(|id| {
            self.state
                .as_ref()?
                .plugins
                .get(id)
                .map(|entry| (id.clone(), entry.clone()))
        }) {
            let pending_change = self
                .state
                .as_ref()
                .is_some_and(|state| state.pending.iter().any(|change| change.plugin_id() == &id));
            let enabled = installed.enabled;
            detail = detail
                .child(div().text_lg().child(id.as_str().to_owned()))
                .child(div().text_color(theme.muted).child(format!(
                    "{} {} · {}",
                    tr("当前版本", "Current version"),
                    installed.active,
                    tr("目录不可用", "Catalog unavailable")
                )));
            let target = id.clone();
            detail = detail.child(
                Button::new("extension-toggle-local")
                    .ghost()
                    .label(if enabled {
                        tr("禁用", "Disable")
                    } else {
                        tr("启用", "Enable")
                    })
                    .disabled(self.busy || pending_change)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(if enabled {
                                ExtensionChange::Disable(target.clone())
                            } else {
                                ExtensionChange::Enable(target.clone())
                            }),
                            cx,
                        )
                    })),
            );
            if let Some(previous) = installed.previous {
                let target = id.clone();
                detail = detail.child(
                    Button::new("extension-rollback-local")
                        .ghost()
                        .label(format!("{} {previous}", tr("回退到", "Roll back to")))
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::Rollback(target.clone())),
                                cx,
                            )
                        })),
                );
            }
            let target = id.clone();
            detail = detail.child(
                Button::new("extension-uninstall-local")
                    .ghost()
                    .label(tr("卸载", "Uninstall"))
                    .disabled(self.busy || pending_change)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(ExtensionChange::Uninstall(target.clone())),
                            cx,
                        )
                    })),
            );
            detail = detail.child(
                Button::new("extension-delete-local")
                    .ghost()
                    .label(tr("删除插件数据…", "Delete extension data…"))
                    .disabled(self.busy)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.confirmation = Some(Confirmation::DeleteData(id.clone()));
                        cx.notify();
                    })),
            );
        } else {
            detail = detail.child(div().text_color(theme.muted).child(tr(
                "选择扩展查看详情",
                "Select an extension to see its details",
            )));
        }
        // 失败的 pending 通过修订和完整意图绑定撤销，避免覆盖其它窗口的新变更。
        if let Some((revision, pending, failure)) = self.selected.as_ref().and_then(|id| {
            let state = self.state.as_ref()?;
            let failure = state.failures.get(id)?.clone();
            let pending = state
                .pending
                .iter()
                .find(|change| change.plugin_id() == id)?
                .clone();
            Some((state.revision, pending, failure))
        }) {
            detail = detail.child(div().text_sm().text_color(theme.muted).child(failure));
            detail = detail.child(
                Button::new("extension-cancel-failed")
                    .label(tr("撤销失败的变更", "Cancel failed change"))
                    .disabled(self.busy)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(ExtensionChange::CancelFailedChange {
                                expected_revision: revision,
                                pending: pending.clone(),
                            }),
                            cx,
                        )
                    })),
            );
        }
        let mut toolbar = div()
            .h(px(44.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_color(theme.foreground)
                    .child(tr("扩展", "Extensions")),
            )
            .child(
                Button::new("extensions-refresh")
                    .ghost()
                    .label(tr("刷新目录", "Refresh catalog"))
                    .disabled(self.busy)
                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx))),
            );
        if self.offline {
            toolbar = toolbar.child(
                div()
                    .text_color(theme.muted)
                    .child(tr("离线 · 使用缓存目录", "Offline · using cached catalog")),
            );
        }
        if let Some(progress) = &self.progress {
            let text = match progress {
                InstallProgress::Preparing => tr("准备中", "Preparing").to_owned(),
                InstallProgress::Downloading { received, total } => {
                    format!(
                        "{} {received}/{total} {}",
                        tr("下载中", "Downloading"),
                        tr("字节", "bytes")
                    )
                }
                InstallProgress::Verifying => tr("校验中", "Verifying").to_owned(),
                InstallProgress::Publishing => tr("保存中", "Saving").to_owned(),
            };
            toolbar = toolbar.child(div().text_color(theme.muted).child(text));
        }
        if self.cancel.is_some() {
            toolbar = toolbar.child(
                Button::new("extensions-cancel")
                    .ghost()
                    .label(tr("取消", "Cancel"))
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.cancel();
                        cx.notify();
                    })),
            );
        }
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(toolbar)
            .child(
                div()
                    .id("extensions-columns")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    // 设置弹框可能窄于三栏自然宽度；保留横向滚动，避免详情和操作被裁掉。
                    .overflow_x_scroll()
                    .child(sidebar)
                    .child(list)
                    .child(detail),
            );
        if let Some(message) = self.status.clone() {
            root = root.child(
                div()
                    .px_4()
                    .py_2()
                    .text_color(theme.error)
                    .child(message)
                    .child(
                        Button::new("extensions-retry")
                            .ghost()
                            .label(if self.retry_operation.is_some() {
                                tr("重试操作", "Retry operation")
                            } else {
                                tr("刷新状态", "Refresh state")
                            })
                            .on_click(cx.listener(|page, _, _, cx| {
                                if let Some(operation) = page.retry_operation.take() {
                                    page.run_operation(operation, cx);
                                } else {
                                    page.activate(cx);
                                }
                            })),
                    ),
            );
        }
        if let Some(confirmation) = self.confirmation.clone() {
            let (message, action) = match confirmation {
                Confirmation::Install(selection) => {
                    let plugin = self.catalog.as_ref().and_then(|catalog| {
                        catalog
                            .catalog
                            .plugins
                            .iter()
                            .find(|plugin| plugin.id == selection.id)
                    });
                    let permissions = plugin
                        .map(|plugin| {
                            plugin
                                .permissions
                                .iter()
                                .map(|permission| {
                                    format!(
                                        "{} — {}",
                                        permission,
                                        permission_description(permission)
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    let full = plugin
                        .and_then(|plugin| {
                            plugin
                                .versions
                                .iter()
                                .find(|version| version.version == selection.version)
                        })
                        .is_some_and(|version| version.requires_full_access());
                    (
                        format!(
                            "{} {} {}. {}. {}",
                            tr("安装扩展", "Install extension"),
                            selection.id.as_str(),
                            selection.version,
                            if full {
                                tr(
                                    "完全访问：以当前用户权限运行",
                                    "Full access: runs with the current user's permissions",
                                )
                            } else {
                                tr("受限访问", "Restricted access")
                            },
                            if permissions.is_empty() {
                                tr("无额外权限", "No additional permissions").to_owned()
                            } else {
                                permissions
                            }
                        ),
                        Operation::Install(selection),
                    )
                }
                Confirmation::File(path, inspection) => {
                    let full = inspection
                        .permissions
                        .iter()
                        .any(|permission| permission == "full-access");
                    let permissions = if inspection.permissions.is_empty() {
                        tr("无额外权限", "No additional permissions").to_owned()
                    } else {
                        inspection
                            .permissions
                            .iter()
                            .map(|permission| {
                                format!("{} — {}", permission, permission_description(permission))
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    };
                    (
                        format!(
                            "{} {} ({}) · {} {}. {}. {}",
                            tr(
                                "本地插件未经市场审核",
                                "This local extension has not been reviewed by the marketplace"
                            ),
                            label(&inspection.name, inspection.id.as_str()),
                            inspection.id.as_str(),
                            tr("版本", "version"),
                            inspection.version,
                            if full {
                                tr(
                                    "完全访问：以当前用户权限运行",
                                    "Full access: runs with the current user's permissions",
                                )
                            } else {
                                tr("受限访问", "Restricted access")
                            },
                            permissions
                        ),
                        Operation::File(path, inspection),
                    )
                }
                Confirmation::DeleteData(id) => (
                    format!(
                        "{} {}? {}",
                        tr("删除插件数据", "Delete extension data for"),
                        id.as_str(),
                        tr("此操作不可撤销。", "This cannot be undone.")
                    ),
                    Operation::Delete(id),
                ),
            };
            root = root.child(
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.panel)
                    .child(div().flex_1().min_w(px(0.)).child(message))
                    .child(
                        Button::new("extensions-confirm")
                            .label(tr("确认", "Confirm"))
                            .flex_shrink_0()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.run_operation(action.clone(), cx)
                            })),
                    )
                    .child(
                        Button::new("extensions-dismiss")
                            .ghost()
                            .label(tr("取消", "Cancel"))
                            .flex_shrink_0()
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.confirmation = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        if pending > 0 {
            root = root.child(
                div()
                    .h(px(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.secondary)
                    .child(div().flex_1().child(format!(
                        "{pending} {}",
                        tr(
                            "项变更将在重启后生效",
                            "changes will take effect after restart"
                        )
                    )))
                    .child(
                        Button::new("extensions-restart")
                            .label(tr("立即重启", "Restart now"))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(ExtensionsEvent::RestartRequested)
                            })),
                    ),
            );
        }
        root
    }
}

/// 分类选择只改变页面状态，保持搜索输入与详情实体稳定。
fn filter_button(
    id: impl Into<gpui_kit::ElementId>,
    title: String,
    selected: bool,
    filter: Filter,
    cx: &mut Context<ExtensionsPage>,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .label(if selected {
            format!("● {title}")
        } else {
            title
        })
        .on_click(cx.listener(move |page, _, _, cx| {
            page.filter = filter.clone();
            cx.notify();
        }))
}

/// 使用应用当前语言而不是缓存语言，切换语言后页面能随重绘更新。
fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => zh,
        crate::locale::UiLanguage::EnUs => en,
    }
}

/// 进度属于当前安装行，字节总量来自下载器而非估算。
fn progress_label(progress: &InstallProgress) -> String {
    match progress {
        InstallProgress::Preparing => tr("准备中", "Preparing").into(),
        InstallProgress::Downloading { received, total } => {
            format!("{received}/{total} {}", tr("字节", "bytes"))
        }
        InstallProgress::Verifying => tr("校验中", "Verifying").into(),
        InstallProgress::Publishing => tr("保存中", "Saving").into(),
    }
}

/// 权限原名始终可见，未知权限不被猜成安全能力。
fn permission_description(permission: &str) -> &'static str {
    match permission {
        "ui.notify" => tr("显示通知", "Show notifications"),
        "ui.dialog" => tr(
            "显示确认和输入对话框",
            "Show confirmation and input dialogs",
        ),
        "ui.file-picker" => tr("请求用户选择文件", "Ask the user to select files"),
        "clipboard.read" => tr("读取剪贴板", "Read the clipboard"),
        "clipboard.write" => tr("写入剪贴板", "Write to the clipboard"),
        "open-external" => tr("在浏览器打开 HTTPS 链接", "Open HTTPS links in the browser"),
        "hosts.read" => tr("读取不含凭据的主机列表", "Read hosts without credentials"),
        "terminal.write" => tr(
            "向所选终端粘贴文本",
            "Paste text into the selected terminal",
        ),
        "events.subscribe" => tr("订阅宿主事件", "Subscribe to host events"),
        "secrets" => tr("访问此插件的密钥槽", "Access this extension's secret slots"),
        "full-access" => tr(
            "以当前用户权限运行",
            "Run with the current user's permissions",
        ),
        value if value.starts_with("network:") => {
            tr("访问指定网络主机", "Access the specified network host")
        }
        _ => tr(
            "查看插件文档了解此权限",
            "See the extension documentation for this permission",
        ),
    }
}

/// 外部仓库与更新日志链接只接受无凭据的 HTTPS 地址。
fn external_link(
    id: &'static str,
    label: &'static str,
    url: String,
    cx: &mut Context<ExtensionsPage>,
) -> impl IntoElement {
    let permissions =
        std::collections::BTreeSet::from([kerminal_plugin_protocol::Permission::OpenExternal]);
    let url = crate::plugins::external::view::allowed_external_url(&permissions, &url);
    Button::new(id)
        .ghost()
        .label(label)
        .disabled(url.is_none())
        .on_click(cx.listener(move |_, _, _, cx| {
            if let Some(url) = &url {
                cx.open_url(url.as_str());
            }
        }))
}
