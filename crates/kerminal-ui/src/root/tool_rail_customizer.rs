//! 右栏工具自定义弹框；编辑草稿只在用户确认后进入设置 CAS。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, Render, SharedString, Window,
    assets::IconName,
    base::Selectable as _,
    component::{
        Disableable as _, WindowExt as _,
        button::{Button, ButtonVariants},
        menu::{DropdownMenu as _, PopupMenuItem},
        switch::Switch,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId};

use crate::{features::preferences::PreferencesPanel, theme::ThemeTokens};

use super::{RootView, workbench::tool_rail_icon};

const CUSTOMIZABLE_TOOLS: [ToolRailToolId; 8] = [
    ToolRailToolId::AgentLauncher,
    ToolRailToolId::Sftp,
    ToolRailToolId::Snippets,
    ToolRailToolId::Tmux,
    ToolRailToolId::Ports,
    ToolRailToolId::System,
    ToolRailToolId::Logs,
    ToolRailToolId::Containers,
];

#[derive(Clone, Copy)]
struct LegacyContextSettings {
    order_index: Option<usize>,
    hidden: bool,
    bottom: bool,
    placement: Option<ToolRailPanelPlacement>,
}

/// 唯一工具编辑草稿保留旧 Context 字段作无损回写，但不把退役项显示成入口。
pub(super) struct ToolRailCustomizer {
    preferences_panel: Entity<PreferencesPanel>,
    draft: ToolRailSettings,
    initial: ToolRailSettings,
    legacy_context: LegacyContextSettings,
    open: bool,
    busy: bool,
    error: Option<SharedString>,
}

impl ToolRailCustomizer {
    /// 首次打开时创建已标记为打开的持久实体；Kit 的 dialog builder 重跑时不能重建草稿。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        settings: ToolRailSettings,
        preferences_panel: Entity<PreferencesPanel>,
    ) -> Self {
        let mut customizer = Self {
            preferences_panel,
            draft: ToolRailSettings::default(),
            initial: ToolRailSettings::default(),
            legacy_context: LegacyContextSettings {
                order_index: None,
                hidden: false,
                bottom: false,
                placement: None,
            },
            open: true,
            busy: false,
            error: None,
        };
        customizer.replace_settings(settings);
        customizer
    }

    /// 每次显式打开都从最新 Root 快照重建草稿，旧配置中的退役 Context 值单独保留。
    ///
    /// @author kongweiguang
    pub(super) fn prepare(&mut self, settings: ToolRailSettings, cx: &mut Context<Self>) {
        self.replace_settings(settings);
        self.open = true;
        self.busy = false;
        self.error = None;
        cx.notify();
    }

    /// Root 的终端输入 fence 读取实际 Dialog 生命周期，不把复用实体存在误判为弹框仍打开。
    ///
    /// @author kongweiguang
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// 所有用户关闭路径都同步释放 modal 输入 fence；重复关闭保持幂等。
    ///
    /// @author kongweiguang
    pub(super) fn mark_closed(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        }
    }

    /// Root 只在保存操作仍属于当前弹框时消费回执，避免迟到结果关闭新弹框。
    ///
    /// @author kongweiguang
    pub(super) fn is_saving(&self) -> bool {
        self.busy
    }

    /// CAS 失败只结束忙碌态并保留全部草稿，未知结果由偏好页的回读门禁继续约束。
    ///
    /// @author kongweiguang
    pub(super) fn finish_failed_save(&mut self, message: String, cx: &mut Context<Self>) -> bool {
        if !self.busy {
            return false;
        }
        self.busy = false;
        self.error = Some(message.into());
        cx.notify();
        true
    }

    /// 旧 Context 不是本次可见工具，但它的配置键仍须跨保存和恢复默认完整保留。
    fn replace_settings(&mut self, settings: ToolRailSettings) {
        self.legacy_context = LegacyContextSettings {
            order_index: settings
                .order
                .iter()
                .position(|tool| *tool == ToolRailToolId::Context),
            hidden: settings.hidden.contains(&ToolRailToolId::Context),
            bottom: settings.bottom.contains(&ToolRailToolId::Context),
            placement: settings
                .panel_placements
                .get(&ToolRailToolId::Context)
                .copied(),
        };
        self.draft = customizable_settings(settings, self.legacy_context);
        self.initial = self.draft.clone();
    }

    /// 显示、顺序、分区和停靠位置作为一份 typed settings 快照，仅一次 CAS 提交。
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.draft == self.initial {
            return;
        }
        self.busy = true;
        self.error = None;
        let settings_candidate = self.draft.clone();
        let result = self.preferences_panel.update(cx, |panel, cx| {
            panel.apply_tool_rail_settings_change(settings_candidate, cx)
        });
        if let Err(message) = result {
            self.busy = false;
            self.error = Some(message.into());
        }
        cx.notify();
    }

    /// 所有 8 项至少留一项可发现；显示开关不改动排序或底部成员关系。
    fn set_visible(&mut self, tool: ToolRailToolId, visible: bool, cx: &mut Context<Self>) {
        if self.busy || !is_customizable_tool(tool) {
            return;
        }
        if visible {
            self.draft.hidden.retain(|candidate| *candidate != tool);
        } else {
            let visible_count = CUSTOMIZABLE_TOOLS
                .iter()
                .filter(|candidate| !self.draft.hidden.contains(candidate))
                .count();
            if visible_count <= 1 {
                return;
            }
            if !self.draft.hidden.contains(&tool) {
                self.draft.hidden.push(tool);
            }
        }
        cx.notify();
    }

    /// 停靠配置按工具独立保存，不因位置冲突改写其它工具的偏好。
    fn set_placement(
        &mut self,
        tool: ToolRailToolId,
        placement: ToolRailPanelPlacement,
        cx: &mut Context<Self>,
    ) {
        if self.busy || !is_customizable_tool(tool) {
            return;
        }
        self.draft.panel_placements.insert(tool, placement);
        cx.notify();
    }

    /// 相邻排序只在同一可见分区内移动，不让拖放改变底部固定语义。
    fn move_by(&mut self, tool: ToolRailToolId, offset: isize, cx: &mut Context<Self>) {
        if self.busy || !is_customizable_tool(tool) || !matches!(offset, -1 | 1) {
            return;
        }
        let bottom = self.draft.bottom.contains(&tool);
        let mut sections = split_sections(&self.draft);
        let section = if bottom {
            &mut sections.bottom
        } else {
            &mut sections.main
        };
        let Some(index) = section.iter().position(|candidate| *candidate == tool) else {
            return;
        };
        let target = index as isize + offset;
        if !(0..section.len() as isize).contains(&target) {
            return;
        }
        section.swap(index, target as usize);
        self.apply_sections(sections, cx);
    }

    /// 拖到某行前面会自动切换到该行分区，空分区落点则追加到区尾。
    fn drop_before(
        &mut self,
        source: ToolRailToolId,
        target: ToolRailToolId,
        cx: &mut Context<Self>,
    ) {
        if self.busy
            || !is_customizable_tool(source)
            || !is_customizable_tool(target)
            || source == target
        {
            return;
        }
        let mut sections = split_sections(&self.draft);
        if !move_before_in_sections(&mut sections, source, target) {
            return;
        }
        self.apply_sections(sections, cx);
    }

    /// 空分区是可操作的拖放目标；释放在空白處表示将工具追加到该分区末尾。
    fn drop_at_end(&mut self, source: ToolRailToolId, bottom: bool, cx: &mut Context<Self>) {
        if self.busy || !is_customizable_tool(source) {
            return;
        }
        let mut sections = split_sections(&self.draft);
        move_to_section_end(&mut sections, source, bottom);
        self.apply_sections(sections, cx);
    }

    /// 恢复 main 的八项缺省值但不删除旧 Context 配置字段。
    fn reset_defaults(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.draft = customizable_settings(ToolRailSettings::default(), self.legacy_context);
        cx.notify();
    }

    /// 只恢复本轮八项的可见性，保留退役 Context 的隐藏状态和用户已调整的排序、位置。
    fn show_all(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.draft
            .hidden
            .retain(|tool| !is_customizable_tool(*tool));
        cx.notify();
    }

    /// section 拆分后重建全局可见顺序，并把 legacy Context 放回原索引附近。
    fn apply_sections(&mut self, sections: ToolRailSections, cx: &mut Context<Self>) {
        let visible_order = [sections.main, sections.bottom.clone()].concat();
        self.draft.order = insert_legacy_context(visible_order, self.legacy_context.order_index);
        self.draft
            .bottom
            .retain(|tool| !is_customizable_tool(*tool));
        self.draft.bottom.extend(sections.bottom);
        cx.notify();
    }

    /// render 的轻量投影不复制用户草稿，避免每帧产生不同的 dirty 结果。
    fn sections(&self) -> ToolRailSections {
        split_sections(&self.draft)
    }
}

impl Render for ToolRailCustomizer {
    /// main 在标题分隔线后留 8px；GPUI 不能模糊玻璃表面的背景，因此正文用不透明工具面板表面避免底层文字透出。
    /// 分区单独滚动以保留操作栏，避免内容增多时保存或取消入口离开视区。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        let sections = self.sections();
        let visible_count = CUSTOMIZABLE_TOOLS
            .iter()
            .filter(|tool| !self.draft.hidden.contains(tool))
            .count();
        let at_default =
            self.draft == customizable_settings(ToolRailSettings::default(), self.legacy_context);
        let status = self.error.clone();
        let saving = self.busy;

        let main_section = self.render_section(
            ToolRailSection::Main,
            crate::locale::text("主区域", "Main area"),
            sections.main,
            theme,
            cx,
        );
        let bottom_section = self.render_section(
            ToolRailSection::Bottom,
            crate::locale::text("底部固定区", "Pinned bottom area"),
            sections.bottom,
            theme,
            cx,
        );

        let sections = div()
            .id("tool-rail-customizer-sections")
            .w_full()
            .min_h(px(0.))
            .max_h(px(
                (window.viewport_size().height.as_f32() - 225.).clamp(0., 560.)
            ))
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .child(main_section)
            .child(bottom_section);
        #[cfg(feature = "right-rail-acceptance")]
        let sections = sections.debug_selector(|| "rr-customizer-sections".into());
        let mut content = div()
            .id("tool-rail-customizer-content")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .px(px(20.))
            .pt(px(8.))
            .pb(px(16.))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        format!(
                            "已显示 {visible_count} / {} 个工具",
                            CUSTOMIZABLE_TOOLS.len()
                        ),
                        format!(
                            "{visible_count} of {} tools shown",
                            CUSTOMIZABLE_TOOLS.len()
                        ),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("tool-rail-customizer-show-all")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Eye)
                                    .label(crate::locale::text("全部显示", "Show all"))
                                    .disabled(saving || visible_count == CUSTOMIZABLE_TOOLS.len())
                                    .on_click(
                                        cx.listener(|customizer, _, _, cx| customizer.show_all(cx)),
                                    ),
                            )
                            .child(
                                Button::new("tool-rail-customizer-reset")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::RotateCcw)
                                    .label(crate::locale::text("恢复默认", "Restore defaults"))
                                    .disabled(saving || at_default)
                                    .on_click(cx.listener(|customizer, _, _, cx| {
                                        customizer.reset_defaults(cx);
                                    })),
                            ),
                    ),
            )
            .child(sections);

        if let Some(status) = status {
            content = content.child(
                div()
                    .id("tool-rail-customizer-error")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.error.opacity(0.08))
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(status),
            );
        }

        content.into_any_element()
    }
}

impl ToolRailCustomizer {
    /// 操作栏由 Dialog 固定承载，正文滚动时仍能保存或取消；不复制草稿和提交状态。
    ///
    /// @author kongweiguang
    fn render_footer(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let saving = self.busy;
        let dirty = self.draft != self.initial;
        let cancel = Button::new("tool-rail-customizer-cancel")
            .compact()
            .ghost()
            .label(crate::locale::text("取消", "Cancel"))
            .disabled(saving)
            .on_click(move |_, window, app| {
                entity.update(app, |customizer, cx| customizer.mark_closed(cx));
                window.close_dialog(app);
            });
        #[cfg(feature = "right-rail-acceptance")]
        let cancel = cancel.debug_selector(|| "rr-customizer-cancel".into());
        let save = Button::new("tool-rail-customizer-save")
            .compact()
            .primary()
            .icon(if saving {
                IconName::LoaderCircle
            } else {
                IconName::Check
            })
            .label(crate::locale::text(
                if saving { "保存中…" } else { "保存" },
                if saving { "Saving…" } else { "Save" },
            ))
            .disabled(saving || !dirty)
            .on_click(cx.listener(|customizer, _, _, cx| customizer.submit(cx)));
        #[cfg(feature = "right-rail-acceptance")]
        let save = save.debug_selector(|| "rr-customizer-save".into());
        div()
            .w_full()
            .flex()
            .justify_end()
            .gap_2()
            .px(px(20.))
            .py(px(16.))
            .border_t_1()
            .border_color(theme.tool_panel_border)
            .child(cancel)
            .child(save)
            .into_any_element()
    }
}

impl ToolRailCustomizer {
    /// 两个视觉分区共用行模板；只变化分区计数和排序边界，避免设置语义漂移；文本与控件显式映射 main 的尺寸和主题表面。
    ///
    /// @author kongweiguang
    fn render_section(
        &self,
        section: ToolRailSection,
        title: SharedString,
        tools: Vec<ToolRailToolId>,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let saving = self.busy;
        let bottom = section == ToolRailSection::Bottom;
        let description = if bottom {
            crate::locale::text(
                "始终贴在右栏底部，内部顺序仍可调整",
                "Pinned to the rail bottom; its order remains adjustable",
            )
        } else {
            crate::locale::text(
                "从上到下排列，可在空间不足时滚动",
                "Ordered from top to bottom; scrolls when space is limited",
            )
        };
        let mut section_view = div()
            .id(if bottom {
                "tool-rail-customizer-bottom-section"
            } else {
                "tool-rail-customizer-main-section"
            })
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(theme.tool_panel_border)
            .bg(theme.subtle_background())
            .on_drop(
                cx.listener(move |customizer, dragged: &ToolRailToolId, _, cx| {
                    customizer.drop_at_end(*dragged, bottom, cx);
                }),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_1()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted.opacity(0.72))
                                    .child(description),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme.panel)
                            .text_size(px(10.))
                            .text_color(theme.muted)
                            .child(tools.len().to_string()),
                    ),
            );

        if tools.is_empty() {
            section_view = section_view.child(
                div()
                    .w_full()
                    .h(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.tool_panel_border)
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "将工具拖到这里，或使用每行的固定按钮",
                        "Drop tools here or use the pin button on a row",
                    )),
            );
        }

        for (index, tool) in tools.iter().copied().enumerate() {
            let visible = !self.draft.hidden.contains(&tool);
            let last_visible = visible
                && CUSTOMIZABLE_TOOLS
                    .iter()
                    .filter(|candidate_tool| !self.draft.hidden.contains(candidate_tool))
                    .count()
                    == 1;
            let placement = self
                .draft
                .panel_placements
                .get(&tool)
                .copied()
                .unwrap_or_default();
            let placement_options = placement_options();
            let current_placement = placement_options
                .iter()
                .find(|(_, candidate)| *candidate == placement)
                .map(|(label, _)| label.clone())
                .unwrap_or_else(|| crate::locale::text("贴靠右栏", "Attach to right rail"));
            let label = customizable_tool_label(tool);
            let description = tool_rail_description(tool);
            let is_bottom = bottom;
            let pin_tooltip = crate::locale::text(
                if is_bottom {
                    "移回主区域"
                } else {
                    "固定到底部"
                },
                if is_bottom {
                    "Move to main area"
                } else {
                    "Pin to bottom"
                },
            );
            let drag_entity = entity.clone();
            let up_entity = entity.clone();
            let down_entity = entity.clone();
            let pin_entity = entity.clone();
            let visibility_entity = entity.clone();
            let menu_entity = entity.clone();
            let can_move_up = index > 0;
            let can_move_down = index + 1 < tools.len();
            let drag_tooltip = crate::locale::text(
                format!("拖动排序 {label}"),
                format!("Drag to reorder {label}"),
            );

            let drag_handle = div()
                .id(format!("tool-rail-drag-{tool:?}"))
                .w(px(28.))
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_color(theme.muted)
                .cursor(gpui_kit::CursorStyle::OpenHand)
                .tooltip(move |window, cx| Tooltip::new(drag_tooltip.clone()).build(window, cx))
                .child(IconName::GripVertical)
                .when(!saving, move |this| {
                    this.on_drag(tool, move |dragged, _, _, app| {
                        drag_entity.update(app, |customizer, cx| {
                            customizer.error = None;
                            cx.notify();
                        });
                        app.new(|_| ToolRailDragPreview {
                            label: customizable_tool_label(*dragged),
                            theme,
                        })
                    })
                });

            let mut row = div()
                .id(format!("tool-rail-customizer-row-{tool:?}"))
                .w_full()
                .min_h(px(56.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x_2()
                .gap_y_1()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.panel.opacity(0.0))
                .bg(if visible {
                    theme.panel
                } else {
                    theme.subtle_background()
                })
                .opacity(if visible { 1.0 } else { 0.62 })
                .on_drop(
                    cx.listener(move |customizer, dragged: &ToolRailToolId, _, cx| {
                        customizer.drop_before(*dragged, tool, cx);
                    }),
                )
                .drag_over::<ToolRailToolId>(move |style, dragged, _, _| {
                    if *dragged != tool {
                        style.border_color(theme.accent)
                    } else {
                        style
                    }
                })
                .child(drag_handle)
                .child(
                    div()
                        .id(format!("tool-rail-customizer-icon-{tool:?}"))
                        .w(px(32.))
                        .h(px(32.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .bg(theme.subtle_background())
                        .text_color(theme.muted)
                        .child(tool_rail_icon(tool)),
                )
                .child(
                    div()
                        .min_w(px(128.))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(13.))
                                        .line_height(px(19.5))
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(theme.foreground)
                                        .child(label.clone()),
                                )
                                .when(is_bottom, |this| {
                                    this.child(
                                        div()
                                            .px_1()
                                            .rounded_sm()
                                            .bg(theme.selected)
                                            .text_size(theme.text_xs)
                                            .text_color(theme.selected_foreground)
                                            .child(crate::locale::text("底部", "Bottom")),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .line_height(px(16.5))
                                .text_color(theme.muted.opacity(0.78))
                                .child(description),
                        ),
                )
                .child({
                    let placement_button = Button::new(format!("tool-rail-placement-{tool:?}"))
                        .compact()
                        .outline()
                        .disabled(saving)
                        .w(px(112.))
                        .rounded(theme.control_radius)
                        .bg(theme.field_surface())
                        .border_color(theme.strong_border())
                        .text_color(theme.foreground)
                        .when(saving, |button| button.opacity(0.5))
                        .dropdown_caret(true)
                        .accessibility_label(crate::locale::text(
                            format!("打开位置 {label}"),
                            format!("Open placement for {label}"),
                        ))
                        .child(
                            div()
                                .min_w(px(0.))
                                .flex_1()
                                .truncate()
                                .text_size(px(11.))
                                .line_height(px(16.5))
                                .child(current_placement.clone()),
                        );
                    #[cfg(feature = "right-rail-acceptance")]
                    let placement_button = placement_button
                        .debug_selector(move || format!("rr-customizer-placement-{tool:?}"));
                    placement_button.dropdown_menu(move |mut menu, _, _| {
                        for (label, candidate_placement) in placement_options.iter().cloned() {
                            let item_entity = menu_entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(label)
                                    .checked(candidate_placement == placement)
                                    .on_click(move |_, _, app| {
                                        item_entity.update(app, |customizer, cx| {
                                            customizer.set_placement(tool, candidate_placement, cx);
                                        });
                                    }),
                            );
                        }
                        menu
                    })
                })
                .child(
                    Button::new(format!("tool-rail-pin-{tool:?}"))
                        .compact()
                        .ghost()
                        .icon(IconName::PanelBottom)
                        .selected(is_bottom)
                        .disabled(saving)
                        .accessibility_label(crate::locale::text(
                            format!(
                                "{} {label}",
                                if is_bottom {
                                    "移回主区域"
                                } else {
                                    "固定到底部"
                                }
                            ),
                            format!(
                                "{} {label}",
                                if is_bottom {
                                    "Move to main area"
                                } else {
                                    "Pin to bottom"
                                }
                            ),
                        ))
                        .tooltip(pin_tooltip)
                        .on_click(move |_, _, app| {
                            pin_entity.update(app, |customizer, cx| {
                                customizer.drop_at_end(tool, !is_bottom, cx);
                            });
                        }),
                )
                .child(
                    Button::new(format!("tool-rail-up-{tool:?}"))
                        .compact()
                        .ghost()
                        .icon(IconName::ArrowUp)
                        .accessibility_label(crate::locale::text(
                            format!("上移 {label}"),
                            format!("Move up {label}"),
                        ))
                        .disabled(saving || !can_move_up)
                        .on_click(move |_, _, app| {
                            up_entity
                                .update(app, |customizer, cx| customizer.move_by(tool, -1, cx));
                        }),
                )
                .child(
                    Button::new(format!("tool-rail-down-{tool:?}"))
                        .compact()
                        .ghost()
                        .icon(IconName::ArrowDown)
                        .accessibility_label(crate::locale::text(
                            format!("下移 {label}"),
                            format!("Move down {label}"),
                        ))
                        .disabled(saving || !can_move_down)
                        .on_click(move |_, _, app| {
                            down_entity
                                .update(app, |customizer, cx| customizer.move_by(tool, 1, cx));
                        }),
                )
                .child(
                    Switch::new(format!("tool-rail-visible-{tool:?}"))
                        .checked(visible)
                        .color(theme.accent)
                        .disabled(saving || last_visible)
                        .accessibility_label(crate::locale::text(
                            format!("{} {label}", if visible { "隐藏" } else { "显示" }),
                            format!("{} {label}", if visible { "Hide" } else { "Show" }),
                        ))
                        .on_change(move |next, _, app| {
                            visibility_entity.update(app, |customizer, cx| {
                                customizer.set_visible(tool, *next, cx);
                            });
                        }),
                );
            if !visible {
                row = row.child(div().text_color(theme.muted).child(IconName::EyeOff));
            }
            row = row.when(saving, |this| this.opacity(0.72));
            section_view = section_view.child(row);
        }
        section_view.into_any_element()
    }
}

/// Root 创建对话框前复用唯一的设置 panel 实体，保存请求仍进入既有草稿校验和 CAS。
impl RootView {
    /// 从当前设置快照准备可复用草稿；Kit 保留焦点门禁，材质参数与 main 的 ModalShell 共用。
    ///
    /// @author kongweiguang
    pub(super) fn open_tool_rail_customizer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tool_panel_overlay_focus_is_blocked(window, cx) || window.has_active_prompt() {
            return;
        }
        let settings = self.current_tool_rail_settings();
        let customizer = if let Some(customizer) = self.tool_rail_customizer.as_ref() {
            let customizer = customizer.clone();
            customizer.update(cx, |customizer, cx| customizer.prepare(settings, cx));
            customizer
        } else {
            let preferences_panel = self.preferences_panel.clone();
            let customizer = cx.new(|_| ToolRailCustomizer::new(settings, preferences_panel));
            self.tool_rail_customizer = Some(customizer.clone());
            customizer
        };
        let close_entity = customizer.clone();
        window.open_dialog(cx, move |dialog, window, app| {
            let busy = customizer.read(app).is_saving();
            let (theme, footer) = customizer.update(app, |customizer, cx| {
                let theme = ThemeTokens::current(window, cx).for_tool_panel();
                (theme, customizer.render_footer(theme, cx))
            });
            let header_close = close_entity.clone();
            let height = px((window.viewport_size().height.as_f32() - 48.).clamp(0., 704.));
            dialog
                .backdrop_blur(theme.modal_blur_radius)
                .surface_backdrop_filter(theme.floating_blur_radius, theme.floating_saturation)
                .p_0()
                .gap_0()
                .h(height)
                .rounded(theme.radius_dialog)
                .bg(theme.floating_surface)
                .margin_top((window.viewport_size().height - height) / 2.)
                .title(
                    div().w_full().flex().items_start().justify_between()
                        .px(px(20.)).py(px(16.)).border_b_1().border_color(theme.tool_panel_border)
                        .child(div().min_w(px(0.)).flex_1()
                            .child(div().text_size(px(17.)).line_height(px(24.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(crate::locale::text("自定义工具栏", "Customize toolbar")))
                            .child(div().mt_1().text_size(px(13.)).line_height(px(20.))
                                .font_weight(gpui_kit::FontWeight::NORMAL).text_color(theme.muted)
                                .child(crate::locale::text(
                                    "每个工具都可独立设置显示、顺序、底部固定位置和内容打开方式。",
                                    "Set each tool's visibility, order, bottom pin, and panel placement independently."))))
                        .child(Button::new("tool-rail-customizer-close").compact().ghost()
                            .icon(IconName::X).disabled(busy)
                            .accessibility_label(crate::locale::text("关闭弹窗", "Close dialog"))
                            .on_click(move |_, window, app| {
                                header_close.update(app, |customizer, cx| customizer.mark_closed(cx));
                                window.close_dialog(app);
                            }))
                )
                .footer(footer)
                .width(px(768.))
                .max_w(px(768.))
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(false)
                .on_close({
                    let close_entity = close_entity.clone();
                    move |_, _, app| {
                        close_entity.update(app, |customizer, cx| customizer.mark_closed(cx));
                    }
                })
                .child(customizer.clone())
        });
    }

    /// 保存回执必须匹配当前忙碌弹框；失败显示错误并留稿，成功关闭对话框并释放实体。
    ///
    /// @author kongweiguang
    pub(super) fn finish_tool_rail_customizer_save(
        &mut self,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(customizer) = self.tool_rail_customizer.clone() else {
            return false;
        };
        if !customizer.read(cx).is_saving() {
            return false;
        }
        match result {
            Ok(()) => {
                self.tool_rail_customizer = None;
                window.close_dialog(cx);
                cx.notify();
                true
            }
            Err(message) => customizer.update(cx, |customizer, cx| {
                customizer.finish_failed_save(message, cx)
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolRailSection {
    Main,
    Bottom,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ToolRailSections {
    main: Vec<ToolRailToolId>,
    bottom: Vec<ToolRailToolId>,
}

/// 只允许计划范围内八项进入编辑器；旧版 Context、未知或内部项不会变成新入口。
fn is_customizable_tool(tool: ToolRailToolId) -> bool {
    CUSTOMIZABLE_TOOLS.contains(&tool)
}

/// 合并旧配置与当前八项默认列表，同时保留 Context 的顺序、可见性、分区和位置字段。
fn customizable_settings(
    mut settings: ToolRailSettings,
    legacy_context: LegacyContextSettings,
) -> ToolRailSettings {
    let mut visible_order = settings
        .order
        .iter()
        .copied()
        .filter(|tool| is_customizable_tool(*tool))
        .collect::<Vec<_>>();
    for tool in CUSTOMIZABLE_TOOLS {
        if !visible_order.contains(&tool) {
            visible_order.push(tool);
        }
    }
    settings.order = insert_legacy_context(visible_order, legacy_context.order_index);

    settings.hidden.retain(|tool| is_customizable_tool(*tool));
    if settings
        .hidden
        .iter()
        .filter(|tool| is_customizable_tool(**tool))
        .count()
        >= CUSTOMIZABLE_TOOLS.len()
        && let Some(first) = settings
            .order
            .iter()
            .copied()
            .find(|tool| is_customizable_tool(*tool))
    {
        settings.hidden.retain(|tool| *tool != first);
    }
    if legacy_context.hidden {
        settings.hidden.push(ToolRailToolId::Context);
    }

    settings.bottom.retain(|tool| is_customizable_tool(*tool));
    if legacy_context.bottom {
        settings.bottom.push(ToolRailToolId::Context);
    }
    for tool in CUSTOMIZABLE_TOOLS {
        settings.panel_placements.entry(tool).or_default();
    }
    if let Some(placement) = legacy_context.placement {
        settings
            .panel_placements
            .insert(ToolRailToolId::Context, placement);
    } else {
        settings.panel_placements.remove(&ToolRailToolId::Context);
    }
    settings
}

/// 将退役的 Context 恢复到原索引附近，不让八项可见排序重写其旧配置位置。
fn insert_legacy_context(
    mut visible_order: Vec<ToolRailToolId>,
    context_index: Option<usize>,
) -> Vec<ToolRailToolId> {
    if let Some(index) = context_index {
        visible_order.insert(index.min(visible_order.len()), ToolRailToolId::Context);
    }
    visible_order
}

/// 根据底部成员集合投影两个拖放分区；排序仍只有一个全局来源。
fn split_sections(settings: &ToolRailSettings) -> ToolRailSections {
    let mut sections = ToolRailSections::default();
    for tool in settings
        .order
        .iter()
        .copied()
        .filter(|tool| is_customizable_tool(*tool))
    {
        if settings.bottom.contains(&tool) {
            sections.bottom.push(tool);
        } else {
            sections.main.push(tool);
        }
    }
    sections
}

/// 拖到目标项前时跟随目标分区移动，拒绝相同或退役身份以保持排序幂等。
fn move_before_in_sections(
    sections: &mut ToolRailSections,
    source: ToolRailToolId,
    target: ToolRailToolId,
) -> bool {
    if source == target || !is_customizable_tool(source) || !is_customizable_tool(target) {
        return false;
    }
    sections.main.retain(|tool| *tool != source);
    sections.bottom.retain(|tool| *tool != source);
    let destination = if sections.bottom.contains(&target) {
        &mut sections.bottom
    } else {
        &mut sections.main
    };
    let index = destination
        .iter()
        .position(|tool| *tool == target)
        .unwrap_or(destination.len());
    destination.insert(index, source);
    true
}

/// 将工具加入指定分区尾部，固定按钮和空白拖放共用同一排序规则。
fn move_to_section_end(sections: &mut ToolRailSections, source: ToolRailToolId, bottom: bool) {
    sections.main.retain(|tool| *tool != source);
    sections.bottom.retain(|tool| *tool != source);
    if bottom {
        sections.bottom.push(source);
    } else {
        sections.main.push(source);
    }
}

/// 返回 main 自定义器的四种方向名称，菜单值仍使用稳定的 placement 枚举。
fn placement_options() -> [(SharedString, ToolRailPanelPlacement); 4] {
    [
        (
            crate::locale::text("贴靠右栏", "Attach to right rail"),
            ToolRailPanelPlacement::Attached,
        ),
        (
            crate::locale::text("左侧栏", "Left sidebar"),
            ToolRailPanelPlacement::Left,
        ),
        (
            crate::locale::text("底部面板", "Bottom panel"),
            ToolRailPanelPlacement::Bottom,
        ),
        (
            crate::locale::text("自由浮窗", "Floating window"),
            ToolRailPanelPlacement::Center,
        ),
    ]
}

/// 编辑器标题采用 main 右栏的产品名称，不透出 Rust 枚举标识。
fn customizable_tool_label(tool: ToolRailToolId) -> SharedString {
    match tool {
        ToolRailToolId::Context => crate::locale::text("当前上下文", "Current context"),
        ToolRailToolId::AgentLauncher => "Agent Launcher".into(),
        ToolRailToolId::Sftp => crate::locale::text("文件", "Files"),
        ToolRailToolId::Containers => crate::locale::text("容器", "Containers"),
        ToolRailToolId::Snippets => crate::locale::text("片段", "Snippets"),
        ToolRailToolId::Tmux => "tmux".into(),
        ToolRailToolId::Ports => crate::locale::text("端口", "Ports"),
        ToolRailToolId::System => crate::locale::text("系统", "System"),
        ToolRailToolId::Logs => crate::locale::text("命令历史", "Command history"),
    }
}

/// 为每个 rail 工具提供 main 对应的一行说明，辅助用户确认目标而不暴露内部服务名称。
fn tool_rail_description(tool: ToolRailToolId) -> SharedString {
    match tool {
        ToolRailToolId::Context => crate::locale::text("历史配置项", "Legacy setting"),
        ToolRailToolId::AgentLauncher => crate::locale::text(
            "Codex、Claude、自定义 Agent",
            "Codex, Claude, and custom agents",
        ),
        ToolRailToolId::Sftp => crate::locale::text(
            "SSH/SFTP 与容器文件浏览",
            "SSH/SFTP and container file browsing",
        ),
        ToolRailToolId::Containers => crate::locale::text(
            "当前 SSH 主机的 Docker、Podman 和 Compose 容器",
            "Docker, Podman, and Compose containers on the current SSH host",
        ),
        ToolRailToolId::Snippets => crate::locale::text("可复用脚本索引", "Reusable script index"),
        ToolRailToolId::Tmux => crate::locale::text(
            "session、window 和 pane 管理",
            "Session, window, and pane management",
        ),
        ToolRailToolId::Ports => crate::locale::text("SSH 端口转发", "SSH port forwarding"),
        ToolRailToolId::System => crate::locale::text(
            "CPU、内存、网络和磁盘状态",
            "CPU, memory, network, and disk status",
        ),
        ToolRailToolId::Logs => {
            crate::locale::text("当前终端的最近命令", "Recent commands from this terminal")
        }
    }
}

/// 拖放预览沿用应用色板，不在主题外添加新的高对比硬编码表面。
struct ToolRailDragPreview {
    label: SharedString,
    theme: ThemeTokens,
}

impl Render for ToolRailDragPreview {
    /// 预览只显示当前工具名称，稳定宽度避免拖动过程中布局跳动。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .min_w(px(140.))
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .text_color(self.theme.foreground)
            .text_size(self.theme.text_sm)
            .child(self.label.clone())
    }
}

/// @author kongweiguang
#[cfg(test)]
#[path = "../../tests/unit/tool_rail_customizer.rs"]
mod tests;
