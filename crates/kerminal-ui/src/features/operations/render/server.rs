//! 服务器信息概览、资源明细和紧凑时间序列绘制。
//!
//! @author kongweiguang

use super::super::state::{
    SectionPhase, SectionState, ServerInfoMetricSample, ServerInfoPanel, network_rate_label,
    time_series_position,
};
use super::super::{OperationsView, ServerInfoRefreshInterval};
use super::{localized_text, section_feedback};
use crate::theme::ThemeTokens;
use gpui_kit::SharedString;
use gpui_kit::{
    App, Bounds, ClickEvent, Context, Hsla, IntoElement, PathBuilder, Pixels, Window,
    assets::IconName,
    component::{
        Disableable, Selectable,
        button::{Button, ButtonVariants},
        menu::{DropdownMenu as _, PopupMenuItem},
        plot::{Plot, PlotElement},
        tooltip::Tooltip,
    },
    div, point,
    prelude::*,
    px, relative,
};
use kerminal_domain::identity::HostId;
use kerminal_domain::operations::{
    ContainerRuntime, ServerInfoProbeFailure, ServerInfoProbeStatus, ServerInfoTarget,
};
use std::collections::HashMap;

const SERVER_METRIC_PLOT_HEIGHT_PX: f32 = 48.0;
const SERVER_OVERVIEW_METRIC_MIN_COLUMN_WIDTH: f32 = 136.0;

/// 资源页保留用户选择的接口范围；默认只看真实物理接口以避免虚拟流量淹没主链路。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ServerNetworkFilter {
    #[default]
    Primary,
    All,
    Virtual,
}

/// 进程排序只影响当前投影；CPU 是传统默认顺序，内存列可随时恢复原排序。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::features::operations) enum ServerProcessSort {
    #[default]
    Cpu,
    Memory,
}

impl OperationsView {
    /// 页头在右栏与完整页使用同一目标和采样状态；刷新始终指向该服务器区块。
    ///
    /// @author kongweiguang
    pub(super) fn render_server(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let snapshot = self.server.data().cloned();
        let empty = snapshot.is_none();
        let show_feedback = should_render_server_feedback(&self.server);
        let target = snapshot
            .as_ref()
            .map_or(&self.server_info_target, |snapshot| &snapshot.target);
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_server_header(snapshot.as_ref(), target, theme, cx))
            .when(show_feedback, |body| {
                body.child(section_feedback(
                    &self.server,
                    empty,
                    crate::locale::text(
                        "尚未读取服务器信息。",
                        "Server information has not been loaded.",
                    ),
                    theme,
                ))
            });
        if let Some(snapshot) = snapshot {
            body = body.child(match self.server_info_panel {
                ServerInfoPanel::Overview => self
                    .render_server_overview(&snapshot, theme)
                    .into_any_element(),
                ServerInfoPanel::Resources => self
                    .render_server_resources(&snapshot, theme, cx)
                    .into_any_element(),
                ServerInfoPanel::Processes => self
                    .render_server_processes(&snapshot, theme, cx)
                    .into_any_element(),
            });
        }
        server_section_page(
            body,
            theme,
            self.compact_layout,
            self.section_selection.is_fixed(),
        )
    }

    /// 标题、目标与采样态共用页头；状态行的两端都可省略，窄栏和英文长状态也不会挤动下方内容。
    ///
    /// @author kongweiguang
    fn render_server_header(
        &self,
        snapshot: Option<&kerminal_domain::operations::ServerInfoSnapshot>,
        target: &ServerInfoTarget,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let phase = self.server.phase();
        let target_label = if self.operation_target_available() {
            let host_id = server_target_host_id(target);
            let display_name = host_id
                .and_then(|host_id| saved_host_display_name(host_id, &self.host_display_names));
            server_target_label(target, display_name)
        } else if let Some(snapshot) = snapshot {
            let host_id = server_target_host_id(&snapshot.target);
            let display_name = host_id
                .and_then(|host_id| saved_host_display_name(host_id, &self.host_display_names));
            format!(
                "{} · {}",
                localized_text("最近快照", "Last snapshot"),
                server_target_label(&snapshot.target, display_name),
            )
        } else {
            crate::locale::text(
                "无明确目标 · 选择 SSH 主机或恢复当前终端",
                "No explicit target · select an SSH host or restore the current terminal",
            )
            .to_string()
        };
        let target_available = self.operation_target_available();
        let sampling_status = if target_available {
            server_sampling_status(phase, self.server_refresh_interval)
        } else {
            crate::locale::text("当前目标不可用", "Current target unavailable")
        };
        let status_color = if target_available {
            server_sampling_color(theme, phase)
        } else {
            theme.warning
        };
        let last_updated = snapshot
            .and_then(|snapshot| snapshot.sample_times.fast_at_unix_ms)
            .map_or_else(
                || localized_text("无采样时间", "No sample time"),
                |sampled_at| {
                    format!(
                        "{} {}",
                        localized_text("更新于", "Updated"),
                        sample_age_label(sampled_at),
                    )
                },
            );
        let target_title = if !target_available && snapshot.is_none() {
            crate::locale::text("服务器信息", "Server information")
        } else {
            server_page_title(target)
        };
        let target_host_id = server_target_host_id(target).map(|host_id| host_id.as_str());
        let full_target_label = target_label.clone();
        let mut target_row = div()
            .id("operations-server-target-label")
            .min_w_0()
            .flex_1()
            .text_size(theme.text_sm)
            .text_color(theme.muted)
            .truncate()
            .child(target_label);
        if let Some(host_id) = target_host_id {
            let target_tooltip = crate::locale::text(
                format!("目标：{full_target_label} · 主机 ID：{host_id}"),
                format!("Target: {full_target_label}; host ID: {host_id}"),
            );
            let target_accessible_label = target_tooltip.clone();
            target_row = target_row
                .tooltip(move |window, cx| Tooltip::new(target_tooltip.clone()).build(window, cx))
                .aria_label(target_accessible_label);
        }

        let mut actions = div().flex().items_center().gap_1();
        if matches!(self.server_info_target, ServerInfoTarget::Container { .. }) {
            actions = actions.child(
                Button::new("operations-server-host-target")
                    .compact()
                    .label(crate::locale::text("返回主机", "Host"))
                    .disabled(server_refresh_disabled(phase, target_available))
                    .on_click(
                        cx.listener(|view, _: &ClickEvent, _, cx| view.show_host_server_info(cx)),
                    ),
            );
        }
        actions = actions.child(
            Button::new("operations-server-refresh")
                .compact()
                .ghost()
                .icon(IconName::RefreshCw)
                .accessibility_label(crate::locale::text("刷新服务器信息", "Refresh server info"))
                .tooltip(crate::locale::text("刷新服务器信息", "Refresh server info"))
                .disabled(server_refresh_disabled(phase, target_available))
                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.load_server(cx))),
        );

        div()
            .id("operations-server-header")
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui_kit::component::Icon::new(IconName::Server).text_color(theme.accent),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(theme.text_title)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .truncate()
                            .child(target_title),
                    )
                    .child(
                        div()
                            .size(px(8.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(actions),
            )
            .child(div().w_full().flex().items_center().child(target_row))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(self.render_server_tabs(theme, cx))
                    .child(self.render_server_refresh_intervals(cx)),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(status_color)
                            .child(sampling_status),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_color(theme.muted)
                            .text_right()
                            .truncate()
                            .child(last_updated),
                    ),
            )
    }

    /// 概览先展示六项趋势，再用系统卡补齐内核、主机身份与根分区摘要。
    ///
    /// @author kongweiguang
    fn render_server_overview(
        &self,
        snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
        theme: ThemeTokens,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_server_history(snapshot, theme))
            .child(server_system_details(snapshot, theme))
    }

    /// 资源卡先呈现总体占用和真实历史趋势，再显示明细；交互筛选只改本地投影。
    ///
    /// @author kongweiguang
    fn render_server_resources(
        &self,
        snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let fast = self.server_history.samples();
        let cpu_history = fast
            .iter()
            .map(|sample| ServerInfoMetricSample {
                sampled_at_unix_ms: sample.sampled_at_unix_ms,
                value: sample.cpu_percent,
            })
            .collect::<Vec<_>>();
        let memory_history = fast
            .iter()
            .map(|sample| ServerInfoMetricSample {
                sampled_at_unix_ms: sample.sampled_at_unix_ms,
                value: sample.memory_percent,
            })
            .collect::<Vec<_>>();
        let cpu_body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(server_percentage_meter(snapshot.cpu.usage_percent, theme))
            .child(server_history_plot(cpu_history, 0.0, 100.0, theme))
            .child(server_resource_line(
                localized_text("型号", "Model"),
                snapshot.cpu.model.as_deref().unwrap_or("—").to_owned(),
                theme,
            ))
            .child(server_resource_line(
                localized_text("核心", "Cores"),
                if snapshot.cpu.per_core_percent.is_empty() {
                    "—".to_owned()
                } else {
                    crate::locale::text(
                        format!("{} 核", snapshot.cpu.per_core_percent.len()),
                        format!("{} cores", snapshot.cpu.per_core_percent.len()),
                    )
                    .to_string()
                },
                theme,
            ))
            .child(server_resource_line(
                "Load".to_owned(),
                numeric_list(&snapshot.cpu.load_average),
                theme,
            ));
        let mut cpu_body = cpu_body;
        if !snapshot.cpu.per_core_percent.is_empty() {
            let mut cores = div().w_full().flex().flex_row().flex_wrap().gap_2().pt_2();
            for (index, usage) in snapshot.cpu.per_core_percent.iter().copied().enumerate() {
                cores = cores.child(
                    div()
                        .min_w(px(68.))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(format!("C{index}"))
                                .child(percent(usage)),
                        )
                        .child(server_percentage_meter(usage, theme)),
                );
            }
            cpu_body = cpu_body.child(cores);
        }
        let cpu = server_resource_card(
            localized_text("CPU", "CPU"),
            percentage_label(snapshot.cpu.usage_percent),
            cpu_body,
            theme,
        );

        let mut gpu_body = div().w_full().flex().flex_col();
        for gpu in &snapshot.gpus {
            gpu_body = gpu_body.child(server_resource_line(
                format!(
                    "{} · {}",
                    gpu.model.as_deref().unwrap_or(&gpu.id),
                    gpu.vendor.as_deref().unwrap_or("—"),
                ),
                format!(
                    "{} · {} · {} · {}",
                    percent(gpu.utilization_percent),
                    memory_pair(gpu.memory_used_bytes, gpu.memory_total_bytes),
                    gpu.temperature_celsius
                        .map_or_else(|| "—".to_owned(), |value| format!("{value:.0} °C")),
                    gpu.power_watts
                        .map_or_else(|| "—".to_owned(), |value| format!("{value:.1} W")),
                ),
                theme,
            ));
        }
        if snapshot.gpus.is_empty() {
            gpu_body = gpu_body.child(server_resource_empty_state(
                if snapshot.gpu_probe_status == ServerInfoProbeStatus::Available {
                    localized_text("未发现可监控 GPU", "No monitorable GPU found")
                } else {
                    probe_sample_label(
                        &snapshot.gpu_probe_status,
                        snapshot.sample_times.gpu_at_unix_ms,
                    )
                },
                theme,
            ));
        }
        let gpu = server_resource_card(
            localized_text("GPU", "GPU"),
            server_device_count_label(
                snapshot.gpus.len(),
                "张显卡",
                "GPUs",
                &snapshot.gpu_probe_status,
                snapshot.sample_times.gpu_at_unix_ms,
            ),
            gpu_body,
            theme,
        );

        let memory = server_resource_card(
            localized_text("内存", "Memory"),
            memory_pair(snapshot.memory.used_bytes, snapshot.memory.total_bytes),
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(server_percentage_meter(
                    server_memory_percent(&snapshot.memory),
                    theme,
                ))
                .child(server_history_plot(memory_history, 0.0, 100.0, theme))
                .child(server_resource_line(
                    localized_text("可用", "Available"),
                    server_bytes_label(snapshot.memory.available_bytes),
                    theme,
                ))
                .child(server_resource_line(
                    "Cached".to_owned(),
                    server_bytes_label(snapshot.memory.cached_bytes),
                    theme,
                ))
                .child(server_resource_line(
                    "Swap".to_owned(),
                    memory_pair(
                        snapshot.memory.swap_used_bytes,
                        snapshot.memory.swap_total_bytes,
                    ),
                    theme,
                )),
            theme,
        );

        let selected_filter = self.server_network_filter;
        let mut filter_control = div()
            .w_full()
            .flex()
            .gap_1()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary);
        for (filter, zh, en, id) in [
            (ServerNetworkFilter::Primary, "主要", "Primary", "primary"),
            (ServerNetworkFilter::All, "全部", "All", "all"),
            (ServerNetworkFilter::Virtual, "虚拟", "Virtual", "virtual"),
        ] {
            let is_selected = selected_filter == filter;
            let option = Button::new(format!("operations-server-network-filter-{id}"))
                .compact()
                .ghost()
                .flex_1()
                .h(px(32.))
                .selected(is_selected)
                .toggled(is_selected)
                .label(crate::locale::text(zh, en))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    view.server_network_filter = filter;
                    cx.notify();
                }));
            filter_control = filter_control.child(option);
        }
        let network_rates = filtered_network_rates(&snapshot.network_rates, selected_filter);
        let mut network_body = div().w_full().flex().flex_col().child(filter_control);
        if network_rates.is_empty() {
            network_body = network_body.child(server_resource_empty_state(
                if snapshot.network_probe_status == ServerInfoProbeStatus::Available {
                    if snapshot.network_rates.is_empty() {
                        localized_text("未发现网卡", "No interfaces found")
                    } else {
                        localized_text("当前筛选没有网卡", "No interfaces in this filter")
                    }
                } else {
                    probe_sample_label(
                        &snapshot.network_probe_status,
                        snapshot.sample_times.fast_at_unix_ms,
                    )
                },
                theme,
            ));
        } else {
            for rate in network_rates {
                let role = network_interface_role(&rate.name);
                let reset_label = if rate.counter_reset {
                    localized_text(" · 计数器重置", " · counter reset")
                } else {
                    String::new()
                };
                network_body = network_body.child(server_resource_line(
                    format!("{} · {}", rate.name, network_role_label(role)),
                    format!(
                        "↓ {} · ↑ {}{reset_label}",
                        server_rate_label(rate.rx_bytes_per_second),
                        server_rate_label(rate.tx_bytes_per_second),
                    ),
                    theme,
                ));
            }
        }
        let network = server_resource_card(
            localized_text("网络接口", "Network interfaces"),
            probe_sample_label(
                &snapshot.network_probe_status,
                snapshot.sample_times.fast_at_unix_ms,
            ),
            network_body,
            theme,
        );

        let mut disk_body = div().w_full().flex().flex_col();
        if snapshot.disks.is_empty() {
            disk_body = disk_body.child(server_resource_empty_state(
                if snapshot.disk_probe_status == ServerInfoProbeStatus::Available {
                    localized_text("未发现可用挂载项", "No readable mounts found")
                } else {
                    probe_sample_label(
                        &snapshot.disk_probe_status,
                        snapshot.sample_times.slow_at_unix_ms,
                    )
                },
                theme,
            ));
        } else {
            for disk in &snapshot.disks {
                disk_body = disk_body
                    .child(server_resource_line(
                        format!("{} · {}", disk.mount, disk.filesystem),
                        format!(
                            "{} / {} · {} {}",
                            server_bytes_label(disk.used_bytes),
                            server_bytes_label(disk.total_bytes),
                            server_bytes_label(disk.available_bytes),
                            localized_text("可用", "free"),
                        ),
                        theme,
                    ))
                    .child(server_percentage_meter(server_disk_percent(disk), theme));
            }
        }
        let disks = server_resource_card(
            localized_text("文件系统", "Filesystems"),
            probe_sample_label(
                &snapshot.disk_probe_status,
                snapshot.sample_times.slow_at_unix_ms,
            ),
            disk_body,
            theme,
        );

        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(cpu)
            .child(gpu)
            .child(memory)
            .child(network)
            .child(disks);
        if snapshot.npu_probe_status != ServerInfoProbeStatus::NotSampled
            || !snapshot.npus.is_empty()
        {
            let mut npu_body = div().w_full().flex().flex_col();
            if snapshot.npus.is_empty() {
                npu_body = npu_body.child(server_resource_empty_state(
                    if snapshot.npu_probe_status == ServerInfoProbeStatus::Available {
                        localized_text("未发现可监控 NPU", "No monitorable NPU found")
                    } else {
                        probe_sample_label(
                            &snapshot.npu_probe_status,
                            snapshot.sample_times.npu_at_unix_ms,
                        )
                    },
                    theme,
                ));
            } else {
                for npu in &snapshot.npus {
                    npu_body = npu_body.child(server_resource_line(
                        format!(
                            "{} · {}",
                            npu.model.as_deref().unwrap_or(&npu.id),
                            npu.bus_id.as_deref().unwrap_or("—"),
                        ),
                        format!(
                            "{} · {} · HBM {} / {} · {} · {}",
                            npu.health.as_deref().unwrap_or("—"),
                            percent(npu.aicore_percent),
                            server_bytes_label(npu.hbm_used_bytes),
                            server_bytes_label(npu.hbm_total_bytes),
                            npu.temperature_celsius
                                .map_or_else(|| "—".to_owned(), |value| format!("{value:.0} °C"),),
                            npu.power_watts
                                .map_or_else(|| "—".to_owned(), |value| format!("{value:.1} W"),),
                        ),
                        theme,
                    ));
                }
            }
            body = body.child(server_resource_card(
                localized_text("NPU", "NPU"),
                server_device_count_label(
                    snapshot.npus.len(),
                    "张加速卡",
                    "accelerators",
                    &snapshot.npu_probe_status,
                    snapshot.sample_times.npu_at_unix_ms,
                ),
                npu_body,
                theme,
            ));
        }
        body
    }

    /// 进程页按固定列宽展示进程、CPU 和内存，排序切换仅重新投影现有快照。
    ///
    /// @author kongweiguang
    fn render_server_processes(
        &self,
        snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut processes = snapshot.processes.clone();
        processes
            .sort_by(|left, right| compare_server_processes(left, right, self.server_process_sort));
        let displayed_count = processes.len().min(12);
        let mut rows = div().w_full().flex().flex_col();
        if processes.is_empty() {
            rows = rows.child(server_resource_empty_state(
                if snapshot.process_probe_status == ServerInfoProbeStatus::Available {
                    localized_text("未返回进程排行", "No process data returned")
                } else {
                    probe_sample_label(
                        &snapshot.process_probe_status,
                        snapshot.sample_times.slow_at_unix_ms,
                    )
                },
                theme,
            ));
        } else {
            let cpu_selected = self.server_process_sort == ServerProcessSort::Cpu;
            let memory_selected = self.server_process_sort == ServerProcessSort::Memory;
            let cpu_sort = Button::new("operations-server-process-sort-cpu")
                .compact()
                .ghost()
                .w(px(70.))
                .h(px(32.))
                .justify_end()
                .text_size(theme.text_xs)
                .selected(cpu_selected)
                .toggled(cpu_selected)
                .label("CPU")
                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                    view.server_process_sort = ServerProcessSort::Cpu;
                    cx.notify();
                }));
            let memory_sort = Button::new("operations-server-process-sort-memory")
                .compact()
                .ghost()
                .w(px(88.))
                .h(px(32.))
                .justify_end()
                .text_size(theme.text_xs)
                .selected(memory_selected)
                .toggled(memory_selected)
                .label(localized_text("内存", "Memory"))
                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                    view.server_process_sort = ServerProcessSort::Memory;
                    cx.notify();
                }));
            rows = rows.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pb_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(localized_text("进程", "Process")),
                    )
                    .child(cpu_sort)
                    .child(memory_sort),
            );
            for (index, process) in processes.iter().take(12).enumerate() {
                let row = div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .when(index + 1 < displayed_count, |row| {
                        row.border_b_1().border_color(theme.border)
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(process.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(format!("PID {}", process.pid)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(70.))
                            .text_right()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(server_process_percent(process.cpu_percent)),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .text_right()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(server_bytes_label(process.memory_bytes)),
                    );
                rows = rows.child(row);
            }
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(server_resource_card(
                localized_text("进程", "Processes"),
                probe_sample_label(
                    &snapshot.process_probe_status,
                    snapshot.sample_times.slow_at_unix_ms,
                ),
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(format!(
                                "{} {} · {} {} · {} {}",
                                localized_text("进程", "Processes"),
                                snapshot
                                    .process_count
                                    .map_or_else(|| "—".to_owned(), |count| count.to_string(),),
                                localized_text("运行中", "running"),
                                snapshot
                                    .running_process_count
                                    .map_or_else(|| "—".to_owned(), |count| count.to_string(),),
                                localized_text("展示前", "showing top"),
                                displayed_count,
                            )),
                    )
                    .child(rows),
                theme,
            ))
    }
    /// 以三组双列指标收紧趋势高度；GPU 只显示当前快照，不把单点伪装成历史曲线。
    ///
    /// @author kongweiguang
    fn render_server_history(
        &self,
        snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
        theme: ThemeTokens,
    ) -> impl IntoElement {
        let fast = self.server_history.samples();
        let latest = fast.back().copied();
        let fast_points = fast
            .iter()
            .map(|sample| sample.sampled_at_unix_ms)
            .zip(fast.iter().map(|sample| sample.cpu_percent))
            .map(|(sampled_at_unix_ms, value)| ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            })
            .collect::<Vec<_>>();
        let memory_points = fast
            .iter()
            .map(|sample| sample.sampled_at_unix_ms)
            .zip(fast.iter().map(|sample| sample.memory_percent))
            .map(|(sampled_at_unix_ms, value)| ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            })
            .collect::<Vec<_>>();
        let receive_points = fast
            .iter()
            .map(|sample| sample.sampled_at_unix_ms)
            .zip(fast.iter().map(|sample| sample.receive_bytes_per_second))
            .map(|(sampled_at_unix_ms, value)| ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            })
            .collect::<Vec<_>>();
        let transmit_points = fast
            .iter()
            .map(|sample| sample.sampled_at_unix_ms)
            .zip(fast.iter().map(|sample| sample.transmit_bytes_per_second))
            .map(|(sampled_at_unix_ms, value)| ServerInfoMetricSample {
                sampled_at_unix_ms,
                value,
            })
            .collect::<Vec<_>>();
        let disk_points = self
            .server_history
            .disk_samples()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let disk_current = disk_points.last().and_then(|sample| sample.value);
        let receive_max = dynamic_rate_max(&receive_points);
        let transmit_max = dynamic_rate_max(&transmit_points);
        let receive_plot_max = receive_max.unwrap_or(1.0);
        let transmit_plot_max = transmit_max.unwrap_or(1.0);
        let gpu_current = server_gpu_utilization(
            &snapshot.gpu_probe_status,
            snapshot.sample_times.gpu_at_unix_ms,
            snapshot.gpus.iter().map(|gpu| gpu.utilization_percent),
        );
        let gpu_probe = probe_sample_label(
            &snapshot.gpu_probe_status,
            snapshot.sample_times.gpu_at_unix_ms,
        );
        let disk_title = self.server_history.disk_mount().map_or_else(
            || crate::locale::text("磁盘", "Disk").to_string(),
            |mount| {
                crate::locale::text(format!("磁盘 · {mount}"), format!("Disk · {mount}"))
                    .to_string()
            },
        );
        let cpu = server_metric_row(
            ServerMetricRowData {
                title: crate::locale::text("CPU", "CPU").to_string(),
                current: percentage_label(latest.and_then(|sample| sample.cpu_percent)),
                samples: fast_points,
                minimum: 0.0,
                maximum: 100.0,
            },
            theme,
        );
        let memory = server_metric_row(
            ServerMetricRowData {
                title: crate::locale::text("内存", "Memory").to_string(),
                current: percentage_label(latest.and_then(|sample| sample.memory_percent)),
                samples: memory_points,
                minimum: 0.0,
                maximum: 100.0,
            },
            theme,
        );
        let receive = server_metric_row(
            ServerMetricRowData {
                title: crate::locale::text("下行", "Receive").to_string(),
                current: network_rate_label(
                    latest.and_then(|sample| sample.receive_bytes_per_second),
                ),
                samples: receive_points,
                minimum: 0.0,
                maximum: receive_plot_max,
            },
            theme,
        );
        let transmit = server_metric_row(
            ServerMetricRowData {
                title: crate::locale::text("上行", "Transmit").to_string(),
                current: network_rate_label(
                    latest.and_then(|sample| sample.transmit_bytes_per_second),
                ),
                samples: transmit_points,
                minimum: 0.0,
                maximum: transmit_plot_max,
            },
            theme,
        );
        let disk = server_metric_row(
            ServerMetricRowData {
                title: disk_title,
                current: percentage_label(disk_current),
                samples: disk_points,
                minimum: 0.0,
                maximum: 100.0,
            },
            theme,
        );
        let gpu = server_metric_value_row(
            crate::locale::text("GPU", "GPU").to_string(),
            server_gpu_overview_label(snapshot, gpu_current),
            gpu_probe,
            theme,
        );
        let metrics = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(server_metric_pair(cpu, memory, theme, false))
            .child(server_metric_pair(receive, transmit, theme, false))
            .child(server_metric_pair(disk, gpu, theme, true));
        div().w_full().child(metrics)
    }

    /// 选中页签用主题中性表面强调并保留 Kit 选中语义，避免把导航误读为主操作。
    ///
    /// @author kongweiguang
    fn render_server_tabs(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .min_w(px(196.))
            .flex_1()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary);
        for (panel, zh, en, id) in [
            (ServerInfoPanel::Overview, "概览", "Overview", "overview"),
            (ServerInfoPanel::Resources, "资源", "Resources", "resources"),
            (ServerInfoPanel::Processes, "进程", "Processes", "processes"),
        ] {
            let selected = self.server_info_panel == panel;
            let button = Button::new(format!("operations-server-tab-{id}"))
                .compact()
                .ghost()
                .flex_1()
                .h(px(32.))
                .text_size(px(16.))
                .selected(selected)
                .when(selected, |button| {
                    button.bg(theme.panel).text_color(theme.foreground)
                })
                .when(!selected, |button| button.text_color(theme.muted))
                .label(crate::locale::text(zh, en))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if view.server_info_panel != panel {
                        view.server_info_panel = panel;
                        cx.notify();
                    }
                }));
            row = row.child(button);
        }
        row
    }

    /// 提供旧工具栏支持的固定周期选项，并将自动刷新留在同一监控 driver 中。
    ///
    /// @author kongweiguang
    fn render_server_refresh_intervals(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.server_refresh_interval;
        let (zh, en) = selected.label();
        let entity = cx.entity().clone();
        Button::new("operations-server-refresh-interval-picker")
            .compact()
            .ghost()
            .w(px(76.))
            .flex_shrink_0()
            .h(px(32.))
            .icon(IconName::ChevronDown)
            .label(crate::locale::text(zh, en))
            .dropdown_menu(move |mut menu, _, _| {
                for interval in ServerInfoRefreshInterval::OPTIONS {
                    let (zh, en) = interval.label();
                    let entity = entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text(zh, en))
                            .checked(interval == selected)
                            .on_click(move |_, _, app| {
                                entity.update(app, |view, cx| {
                                    view.set_server_refresh_interval(interval, cx);
                                });
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }
}

/// 系统字段放在六项趋势之后，复用快照值并仅对真实根挂载点生成容量摘要。
///
/// @author kongweiguang
fn server_system_details(
    snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
    theme: ThemeTokens,
) -> impl IntoElement {
    let root_disk = server_root_disk(&snapshot.disks);
    let root_partition = root_disk.map_or_else(
        || "—".to_owned(),
        |disk| {
            format!(
                "{} · {} {}",
                percentage_label(server_disk_percent(disk)),
                server_bytes_label(disk.available_bytes),
                localized_text("可用", "available"),
            )
        },
    );
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_0()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .w_full()
                .pb_2()
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(16.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(localized_text("系统", "System")),
        )
        .child(server_system_value_row(
            localized_text("主机", "Host"),
            snapshot.hostname.as_deref().unwrap_or("—"),
            theme,
        ))
        .child(server_system_value_row(
            localized_text("系统", "OS"),
            snapshot.operating_system.as_deref().unwrap_or("—"),
            theme,
        ))
        .child(server_system_value_row(
            localized_text("架构", "Architecture"),
            snapshot.architecture.as_deref().unwrap_or("—"),
            theme,
        ))
        .child(server_system_value_row(
            "Kernel".to_owned(),
            snapshot.kernel.as_deref().unwrap_or("—"),
            theme,
        ))
        .child(server_system_value_row(
            localized_text("运行时间", "Uptime"),
            &server_uptime_label(snapshot.uptime_seconds),
            theme,
        ))
        .child(server_system_value_row(
            localized_text("根分区", "Root partition"),
            &root_partition,
            theme,
        ))
}

/// 将 uptime 秒数压成一行可扫读时长；不推导开机时间或使用本机时区。
///
/// @author kongweiguang
fn server_uptime_label(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "—".to_owned();
    };
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        crate::locale::text(
            format!("{days} 天 {hours} 小时"),
            format!("{days}d {hours}h"),
        )
        .to_string()
    } else if hours > 0 {
        crate::locale::text(
            format!("{hours} 小时 {minutes} 分钟"),
            format!("{hours}h {minutes}m"),
        )
        .to_string()
    } else if minutes > 0 {
        crate::locale::text(format!("{minutes} 分钟"), format!("{minutes}m")).to_string()
    } else {
        crate::locale::text(format!("{seconds} 秒"), format!("{seconds}s")).to_string()
    }
}

/// 每个系统字段保持单行键值，长值截断以免抢占窄栏趋势空间。
///
/// @author kongweiguang
fn server_system_value_row(label: String, value: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_right()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .truncate()
                .child(value.to_owned()),
        )
}

/// 一张卡片只用主题令牌承载边界和表面，标题/正文/辅助信息分别遵循 16/14/12px 层级。
///
/// @author kongweiguang
fn server_resource_card(
    title: String,
    trailing: String,
    body: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .pb_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(px(16.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .max_w(px(220.))
                        .text_right()
                        .text_size(theme.text_sm)
                        .text_color(theme.foreground)
                        .truncate()
                        .child(trailing),
                ),
        )
        .child(body)
}

/// 信息行统一把可变长标签和值压进同一水平节奏，值溢出时截断而不挤坏卡片宽度。
///
/// @author kongweiguang
fn server_resource_line(label: String, value: String, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .truncate()
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_right()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .truncate()
                .child(value),
        )
}

/// 空状态保留采样结果或失败原因，不把“尚未返回”误画成零值。
///
/// @author kongweiguang
fn server_resource_empty_state(message: String, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .py_3()
        .text_size(theme.text_sm)
        .text_color(theme.muted)
        .child(message)
}

/// 只有有限的 0–100 实测百分比才填充进度条；未知值保留空轨道而非伪造零利用率。
///
/// @author kongweiguang
fn server_percentage_meter(value: Option<f64>, theme: ThemeTokens) -> impl IntoElement {
    let ratio = value
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .map_or(0.0, |value| (value / 100.0) as f32);
    div()
        .w_full()
        .h(px(8.))
        .rounded_full()
        .bg(theme.border)
        .child(
            div()
                .w(relative(ratio))
                .h_full()
                .rounded_full()
                .bg(theme.accent),
        )
}

/// Used/total 缺失或自相矛盾时不推算百分比，只有同一快照的有效容量对参与计算。
///
/// @author kongweiguang
fn server_memory_percent(memory: &kerminal_domain::operations::MemorySnapshot) -> Option<f64> {
    let total = memory.total_bytes.filter(|total| *total > 0)?;
    let used = memory.used_bytes.or_else(|| {
        memory
            .available_bytes
            .filter(|available| *available <= total)
            .map(|available| total - available)
    })?;
    (used <= total).then(|| used as f64 / total as f64 * 100.0)
}

/// 根分区百分比优先用已用量，缺失时才由同一挂载点的总量与可用量推导。
///
/// @author kongweiguang
fn server_disk_percent(disk: &kerminal_domain::operations::DiskSnapshot) -> Option<f64> {
    let total = disk.total_bytes.filter(|total| *total > 0)?;
    let used = disk.used_bytes.or_else(|| {
        disk.available_bytes
            .filter(|available| *available <= total)
            .map(|available| total - available)
    })?;
    (used <= total).then(|| used as f64 / total as f64 * 100.0)
}

/// 只将 POSIX 根挂载或 Windows 驱动器根视为根分区，避免把首个任意挂载误标为系统盘。
///
/// @author kongweiguang
fn server_root_disk(
    disks: &[kerminal_domain::operations::DiskSnapshot],
) -> Option<&kerminal_domain::operations::DiskSnapshot> {
    disks.iter().find(|disk| {
        let mount = disk.mount.as_str();
        mount == "/"
            || mount == "\\"
            || (mount.len() == 3
                && mount.as_bytes()[0].is_ascii_alphabetic()
                && mount.as_bytes()[1] == b':'
                && matches!(mount.as_bytes()[2], b'/' | b'\\'))
    })
}

/// 用二进制单位展示人类可读容量，不在未知时借用 0 或其他采样层级的值。
///
/// @author kongweiguang
fn server_bytes_label(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if value >= TIB {
        format!("{:.1} TB", value as f64 / TIB as f64)
    } else if value >= GIB {
        format!("{:.1} GB", value as f64 / GIB as f64)
    } else if value >= MIB {
        format!("{:.1} MB", value as f64 / MIB as f64)
    } else if value >= KIB {
        format!("{:.1} KB", value as f64 / KIB as f64)
    } else {
        format!("{value} B")
    }
}

/// 网络速率必须为有限非负值；负数或非有限样本不拿来填充资源行。
///
/// @author kongweiguang
fn server_rate_label(value: Option<f64>) -> String {
    let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) else {
        return "—".to_owned();
    };
    if value >= 1_073_741_824.0 {
        format!("{:.1} GB/s", value / 1_073_741_824.0)
    } else if value >= 1_048_576.0 {
        format!("{:.1} MB/s", value / 1_048_576.0)
    } else if value >= 1024.0 {
        format!("{:.1} KB/s", value / 1024.0)
    } else {
        format!("{value:.0} B/s")
    }
}

/// GPU 卡数只有在慢探针完成时才是真实结果；未采样或失败时继续标为未知。
///
/// @author kongweiguang
fn server_gpu_overview_label(
    snapshot: &kerminal_domain::operations::ServerInfoSnapshot,
    utilization: Option<f64>,
) -> String {
    if let Some(value) =
        utilization.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        && snapshot.gpu_probe_status == ServerInfoProbeStatus::Available
        && snapshot.sample_times.gpu_at_unix_ms.is_some()
    {
        return percentage_label(Some(value));
    }
    server_device_count_label(
        snapshot.gpus.len(),
        "张",
        "devices",
        &snapshot.gpu_probe_status,
        snapshot.sample_times.gpu_at_unix_ms,
    )
}

/// 设备数只在探针成功且本次存在采样时间时输出，防止“未采样”被渲染成零张卡。
///
/// @author kongweiguang
fn server_device_count_label(
    count: usize,
    unit_zh: &str,
    unit_en: &str,
    status: &ServerInfoProbeStatus,
    sampled_at_unix_ms: Option<u64>,
) -> String {
    if *status != ServerInfoProbeStatus::Available || sampled_at_unix_ms.is_none() {
        return probe_sample_label(status, sampled_at_unix_ms);
    }
    crate::locale::text(format!("{count} {unit_zh}"), format!("{count} {unit_en}")).to_string()
}

/// 为主接口筛选保留旧版命名惯例；未知名称按物理接口处理，避免隐藏定制网卡。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerNetworkRole {
    Bridge,
    Loopback,
    Physical,
    Tunnel,
    Virtual,
}

/// 将 Linux 常见桥接、回环、隧道和虚拟接口前缀映射到展示角色。
///
/// @author kongweiguang
fn network_interface_role(name: &str) -> ServerNetworkRole {
    let name = name.to_ascii_lowercase();
    if name == "lo" {
        ServerNetworkRole::Loopback
    } else if ["br-", "bridge", "docker", "virbr"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Bridge
    } else if ["tun", "tap", "tailscale", "wg", "zt"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Tunnel
    } else if ["veth", "cni", "flannel", "kube-ipvs", "dummy"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        ServerNetworkRole::Virtual
    } else {
        ServerNetworkRole::Physical
    }
}

/// 返回某个筛选范围的接口；Primary 无物理接口时回退到首个非回环项。
///
/// @author kongweiguang
fn filtered_network_rates(
    rates: &[kerminal_domain::operations::NetworkRateSnapshot],
    filter: ServerNetworkFilter,
) -> Vec<&kerminal_domain::operations::NetworkRateSnapshot> {
    match filter {
        ServerNetworkFilter::All => rates.iter().collect(),
        ServerNetworkFilter::Virtual => rates
            .iter()
            .filter(|rate| {
                matches!(
                    network_interface_role(&rate.name),
                    ServerNetworkRole::Bridge
                        | ServerNetworkRole::Tunnel
                        | ServerNetworkRole::Virtual
                )
            })
            .collect(),
        ServerNetworkFilter::Primary => {
            let physical = rates
                .iter()
                .filter(|rate| network_interface_role(&rate.name) == ServerNetworkRole::Physical)
                .collect::<Vec<_>>();
            if physical.is_empty() {
                rates
                    .iter()
                    .filter(|rate| {
                        network_interface_role(&rate.name) != ServerNetworkRole::Loopback
                    })
                    .take(1)
                    .collect()
            } else {
                physical
            }
        }
    }
}

/// 简短接口角色名帮助用户理解筛选结果，同时保留原始系统接口名称。
///
/// @author kongweiguang
fn network_role_label(role: ServerNetworkRole) -> String {
    match role {
        ServerNetworkRole::Bridge => localized_text("网桥", "Bridge"),
        ServerNetworkRole::Loopback => localized_text("回环", "Loopback"),
        ServerNetworkRole::Physical => localized_text("主要", "Primary"),
        ServerNetworkRole::Tunnel => localized_text("隧道", "Tunnel"),
        ServerNetworkRole::Virtual => localized_text("虚拟", "Virtual"),
    }
}

/// 可选进程读数按降序比较，未知或非有限 CPU 值排在有效观测之后。
///
/// @author kongweiguang
fn compare_server_processes(
    left: &kerminal_domain::operations::ProcessSnapshot,
    right: &kerminal_domain::operations::ProcessSnapshot,
    sort: ServerProcessSort,
) -> std::cmp::Ordering {
    let primary = match sort {
        ServerProcessSort::Cpu => compare_optional_cpu_desc(left.cpu_percent, right.cpu_percent),
        ServerProcessSort::Memory => {
            compare_optional_memory_desc(left.memory_bytes, right.memory_bytes)
        }
    };
    let secondary = match sort {
        ServerProcessSort::Cpu => {
            compare_optional_memory_desc(left.memory_bytes, right.memory_bytes)
        }
        ServerProcessSort::Memory => compare_optional_cpu_desc(left.cpu_percent, right.cpu_percent),
    };
    primary
        .then(secondary)
        .then_with(|| left.pid.cmp(&right.pid))
}

/// None 和坏 CPU 读数置后，读取值按从高到低排序以保持榜单可预测。
///
/// @author kongweiguang
fn compare_optional_cpu_desc(left: Option<f64>, right: Option<f64>) -> std::cmp::Ordering {
    match (
        left.filter(|value| value.is_finite() && *value >= 0.0),
        right.filter(|value| value.is_finite() && *value >= 0.0),
    ) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// None 内存读数排在已采集值后面，避免用零参与资源排行。
///
/// @author kongweiguang
fn compare_optional_memory_desc(left: Option<u64>, right: Option<u64>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// 进程 CPU 可能超过 100%（多核并行），只拒绝负数和非有限采样。
///
/// @author kongweiguang
fn server_process_percent(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map_or_else(|| "—".to_owned(), |value| format!("{value:.1}%"))
}

/// GPU 使用快照内带时间戳的可用探针读数；坏值、未采样或失败均保持未知而非零。
///
/// @author kongweiguang
fn server_gpu_utilization(
    status: &ServerInfoProbeStatus,
    sampled_at_unix_ms: Option<u64>,
    readings: impl Iterator<Item = Option<f64>>,
) -> Option<f64> {
    if *status != ServerInfoProbeStatus::Available || sampled_at_unix_ms.is_none() {
        return None;
    }
    let readings = readings
        .filter_map(|value| {
            value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        })
        .collect::<Vec<_>>();
    (!readings.is_empty()).then(|| readings.iter().sum::<f64>() / readings.len() as f64)
}

/// 已有快照的采集中与完成态都不挂载反馈行，避免空行仍参与 flex gap 导致卡片跳动。
/// 首次加载、失败和取消继续显示明确反馈，不能为了稳定高度而隐藏异常。
///
/// @author kongweiguang
fn should_render_server_feedback<T>(state: &SectionState<T>) -> bool {
    !matches!(state.phase(), SectionPhase::Loading | SectionPhase::Ready) || state.data().is_none()
}

/// 页头始终提供当前服务器的刷新入口；目标不可用或采样进行中时禁用以防重复请求。
///
/// @author kongweiguang
fn server_refresh_disabled(phase: SectionPhase, target_available: bool) -> bool {
    !target_available || phase == SectionPhase::Loading
}

/// 采样文案同时说明手动/周期模式；失败与取消显式区分，避免显示成已连接。
///
/// @author kongweiguang
fn server_sampling_status(
    phase: SectionPhase,
    interval: ServerInfoRefreshInterval,
) -> SharedString {
    match phase {
        SectionPhase::Ready if interval == ServerInfoRefreshInterval::Manual => {
            crate::locale::text("采样完成", "Sampling complete")
        }
        SectionPhase::Ready => crate::locale::text("实时采集中", "Live sampling"),
        SectionPhase::Loading => crate::locale::text("正在采集", "Sampling"),
        SectionPhase::Failed => crate::locale::text("采样失败", "Sampling failed"),
        SectionPhase::Cancelled => crate::locale::text("采样已取消", "Sampling cancelled"),
        SectionPhase::NotLoaded => crate::locale::text("等待首次采样", "Waiting for first sample"),
    }
}

/// 按语义状态色标明读取结果，在浅色与深色主题中继续使用 ThemeTokens。
///
/// @author kongweiguang
fn server_sampling_color(theme: ThemeTokens, phase: SectionPhase) -> Hsla {
    match phase {
        SectionPhase::Ready => theme.success,
        SectionPhase::Loading => theme.accent,
        SectionPhase::Failed => theme.error,
        SectionPhase::Cancelled => theme.warning,
        SectionPhase::NotLoaded => theme.muted,
    }
}

/// 服务器页已有完整专属页头，复用公共页面的材质与窄栏摊平规则但省去重复标题。
///
/// @author kongweiguang
fn server_section_page(
    content: impl IntoElement,
    theme: ThemeTokens,
    compact: bool,
    fixed: bool,
) -> impl IntoElement {
    let flattened = super::is_fixed_compact_operations_panel(compact, fixed);
    let mut page = div().w_full().flex().flex_col().gap_2();
    if !flattened {
        page = page
            .when(compact, |page| page.gap_1().p_2())
            .when(!compact, |page| page.p_3())
            .rounded_md()
            .bg(theme.panel);
    }
    page.child(content)
}

/// PlotElement 让运维趋势直接使用 GPUI Kit 的原生绘制生命周期，不依赖额外图表运行时。
///
/// @author kongweiguang
struct OperationsTimelinePlot {
    samples: Vec<ServerInfoMetricSample>,
    minimum: f64,
    maximum: f64,
    line: Hsla,
    grid: Hsla,
}

/// 将轻量自绘图接入 Kit PlotElement，避免每个 metric 再维护一套 element 状态。
///
/// @author kongweiguang
impl IntoElement for OperationsTimelinePlot {
    type Element = PlotElement<Self>;

    /// 使用 Kit 容器管理绘制生命周期，避免手动注册额外 GPUI observer。
    ///
    /// @author kongweiguang
    fn into_element(self) -> Self::Element {
        PlotElement::new(self)
    }
}

/// 在 Kit 预留的 plot bounds 中按毫秒采样间隔绘折线，断点前后分别构建路径。
///
/// @author kongweiguang
impl Plot for OperationsTimelinePlot {
    /// 按真实样本时间和物理量程绘制折线，空样本会结束当前 path 而不跨越断点。
    ///
    /// @author kongweiguang
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) {
        let left = bounds.origin.x + px(1.0);
        let top = bounds.origin.y + px(1.0);
        let width = bounds.size.width - px(2.0);
        let height = bounds.size.height - px(2.0);

        for row in 0..=2 {
            let y = top + height * (row as f32 / 2.0);
            let mut path = PathBuilder::stroke(px(0.75));
            path.move_to(point(left, y));
            path.line_to(point(left + width, y));
            if let Ok(path) = path.build() {
                window.paint_path(path, self.grid);
            }
        }

        let Some(start_at_unix_ms) = self.samples.first().map(|sample| sample.sampled_at_unix_ms)
        else {
            return;
        };
        let end_at_unix_ms = self
            .samples
            .last()
            .map_or(start_at_unix_ms, |sample| sample.sampled_at_unix_ms);
        let mut segment: Option<PathBuilder> = None;
        let mut segment_points = 0usize;
        let mut last_point = None;

        for sample in &self.samples {
            let Some(value) = sample.value else {
                finish_timeline_segment(
                    segment.take(),
                    segment_points,
                    last_point,
                    window,
                    self.line,
                );
                segment_points = 0;
                last_point = None;
                continue;
            };
            let Some((x_ratio, y_ratio)) = time_series_position(
                sample.sampled_at_unix_ms,
                value,
                start_at_unix_ms,
                end_at_unix_ms,
                self.minimum,
                self.maximum,
            ) else {
                finish_timeline_segment(
                    segment.take(),
                    segment_points,
                    last_point,
                    window,
                    self.line,
                );
                segment_points = 0;
                last_point = None;
                continue;
            };
            let current_point = point(left + width * x_ratio as f32, top + height * y_ratio as f32);
            if let Some(path) = &mut segment {
                path.line_to(current_point);
            } else {
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(current_point);
                segment = Some(path);
            }
            segment_points += 1;
            last_point = Some(current_point);
        }
        finish_timeline_segment(segment, segment_points, last_point, window, self.line);
    }
}

/// 单点样本也画成短线；空值只结束当前路径，不能用线段跨越采样缺口。
///
/// @author kongweiguang
fn finish_timeline_segment(
    mut segment: Option<PathBuilder>,
    point_count: usize,
    last_point: Option<gpui_kit::Point<Pixels>>,
    window: &mut Window,
    color: Hsla,
) {
    let Some(mut segment) = segment.take() else {
        return;
    };
    if point_count == 1
        && let Some(last_point) = last_point
    {
        segment.line_to(point(last_point.x + px(2.0), last_point.y));
    }
    if let Ok(path) = segment.build() {
        window.paint_path(path, color);
    }
}

/// 指标行只保留名称、当前读数和一条轻趋势，避免窄栏重复绘制边框与量程说明。
///
/// @author kongweiguang
struct ServerMetricRowData {
    title: String,
    current: String,
    samples: Vec<ServerInfoMetricSample>,
    minimum: f64,
    maximum: f64,
}

/// 指标格按标题、20px 读数、48px 真实趋势垂直排列；固定层级使三组双列的高度稳定。
///
/// @author kongweiguang
fn server_metric_row(data: ServerMetricRowData, theme: ThemeTokens) -> impl IntoElement {
    let ServerMetricRowData {
        title,
        current,
        samples,
        minimum,
        maximum,
    } = data;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .w_full()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .truncate()
                .child(title),
        )
        .child(
            div()
                .w_full()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .truncate()
                .child(current),
        )
        .child(server_history_plot(samples, minimum, maximum, theme))
}

/// 只在存在有效历史样本时创建绘图元素，空历史显示说明而不将零值冒充曲线。
///
/// @author kongweiguang
fn server_history_plot(
    samples: Vec<ServerInfoMetricSample>,
    minimum: f64,
    maximum: f64,
    theme: ThemeTokens,
) -> impl IntoElement {
    let has_value = samples.iter().any(|sample| sample.value.is_some());
    div()
        .w_full()
        .h(px(SERVER_METRIC_PLOT_HEIGHT_PX))
        .when(!has_value, |plot| {
            plot.flex().items_center().child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.subtle)
                    .child(crate::locale::text("暂无有效样本", "No valid samples")),
            )
        })
        .when(has_value, |plot| {
            plot.child(
                OperationsTimelinePlot {
                    samples,
                    minimum,
                    maximum,
                    line: theme.accent,
                    grid: theme.border,
                }
                .into_element(),
            )
        })
}

/// 六指标按三行双列放进单张卡片；分隔线和内边距维持紧凑但易扫读的层次。
///
/// @author kongweiguang
fn server_metric_pair(
    left: impl IntoElement,
    right: impl IntoElement,
    theme: ThemeTokens,
    last_row: bool,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .when(!last_row, |row| row.border_b_1().border_color(theme.border))
        .child(server_metric_column(left, true, theme))
        .child(server_metric_column(right, false, theme))
}

/// 固定内容下限守住窄栏可读性，左列分隔线对应截图中的双列监控网格。
///
/// @author kongweiguang
fn server_metric_column(
    content: impl IntoElement,
    divider: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .min_w(px(SERVER_OVERVIEW_METRIC_MIN_COLUMN_WIDTH))
        .flex_1()
        .p_3()
        .when(divider, |column| {
            column.border_r_1().border_color(theme.border)
        })
        .child(content)
}

/// GPU 保留标题、突出读数和探针回执的空间，但没有历史数据时不伪造折线。
///
/// @author kongweiguang
fn server_metric_value_row(
    title: String,
    current: String,
    detail: String,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .w_full()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .truncate()
                .child(title),
        )
        .child(
            div()
                .w_full()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .truncate()
                .child(current),
        )
        .child(
            div()
                .w_full()
                .h(px(SERVER_METRIC_PLOT_HEIGHT_PX))
                .flex()
                .items_center()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .truncate()
                .child(detail),
        )
}

/// 百分比仅接受已验证的 0–100 读数；越界采样保持未知，不伪装成有效边界值。
///
/// @author kongweiguang
fn percentage_label(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .map_or_else(|| "—".to_owned(), |value| format!("{value:.1}%"))
}

/// 网络曲线只从有效观测值推导量程；无观测时返回 None，由绘图层另取安全下界。
///
/// @author kongweiguang
fn dynamic_rate_max(samples: &[ServerInfoMetricSample]) -> Option<f64> {
    samples
        .iter()
        .filter_map(|sample| sample.value)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .reduce(f64::max)
        .map(|maximum| if maximum <= 0.0 { 1.0 } else { maximum * 1.1 })
}

/// 探针状态与最后真实采样时间并列展示，区分未采样、无设备和采集失败。
fn probe_sample_label(status: &ServerInfoProbeStatus, sampled_at_unix_ms: Option<u64>) -> String {
    let label = match status {
        ServerInfoProbeStatus::NotSampled => {
            return localized_text("未采样", "Not sampled");
        }
        ServerInfoProbeStatus::Available => localized_text("采样正常", "Sampled"),
        ServerInfoProbeStatus::Unavailable(reason) => match reason {
            ServerInfoProbeFailure::Unsupported => localized_text("能力不支持", "Unsupported"),
            ServerInfoProbeFailure::NotFound => localized_text("命令或设备未发现", "Not found"),
            ServerInfoProbeFailure::PermissionDenied => {
                localized_text("权限不足", "Permission denied")
            }
            ServerInfoProbeFailure::TimedOut => localized_text("采样超时", "Timed out"),
            ServerInfoProbeFailure::CommandFailed => {
                localized_text("探针命令失败", "Probe command failed")
            }
            ServerInfoProbeFailure::InvalidData => {
                localized_text("返回数据无法解析", "Invalid probe data")
            }
        },
    };
    match sampled_at_unix_ms {
        Some(sampled_at) => format!("{label} · {}", sample_age_label(sampled_at)),
        None => label,
    }
}

/// 时间戳以相对时长呈现，慢速数据保留在界面时不会被误读为当前采样。
fn sample_age_label(sampled_at_unix_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default();
    let seconds = now.saturating_sub(sampled_at_unix_ms) / 1_000;
    if seconds < 2 {
        localized_text("刚刚", "just now")
    } else if seconds < 60 {
        crate::locale::text(format!("{seconds} 秒前"), format!("{seconds}s ago")).to_string()
    } else if seconds < 3_600 {
        let minutes = seconds / 60;
        crate::locale::text(format!("{minutes} 分钟前"), format!("{minutes}m ago")).to_string()
    } else {
        let hours = seconds / 3_600;
        crate::locale::text(format!("{hours} 小时前"), format!("{hours}h ago")).to_string()
    }
}

/// 页面主标题显示已保存主机名；HostId 只作为缺名回退和辅助技术提示。
///
/// @author kongweiguang
fn server_target_label(target: &ServerInfoTarget, host_name: Option<&str>) -> String {
    match target {
        ServerInfoTarget::Local => localized_text("本机", "Local machine"),
        ServerInfoTarget::Ssh { host_id } => host_name
            .map(str::to_owned)
            .unwrap_or_else(|| format!("SSH · {}", host_id.as_str())),
        ServerInfoTarget::Container {
            host_id,
            runtime,
            container_id,
        } => {
            let runtime = match runtime {
                ContainerRuntime::Docker => "Docker",
                ContainerRuntime::Podman => "Podman",
            };
            let host = host_id.as_ref().map_or_else(
                || localized_text("本机容器", "Local container"),
                |host_id| {
                    host_name
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("SSH · {}", host_id.as_str()))
                },
            );
            format!("{host} · {runtime} · {container_id}")
        }
    }
}

/// 只从精确目标提取 HostId 用于提示和辅助技术，不参与任何目标选择或操作 binding。
///
/// @author kongweiguang
fn server_target_host_id(target: &ServerInfoTarget) -> Option<&HostId> {
    match target {
        ServerInfoTarget::Local => None,
        ServerInfoTarget::Ssh { host_id } => Some(host_id),
        ServerInfoTarget::Container { host_id, .. } => host_id.as_ref(),
    }
}

/// 保持用户可读主机名优先，同时在缺少目录投影时退回稳定 HostId。
///
/// @author kongweiguang
pub(super) fn preferred_host_display_name<'a>(
    host_id: &'a HostId,
    names: &'a HashMap<HostId, SharedString>,
) -> &'a str {
    saved_host_display_name(host_id, names).unwrap_or_else(|| host_id.as_str())
}

/// Keep saved names verbatim in headings; only an absent name falls back to protocol plus stable ID.
///
/// @author kongweiguang
pub(super) fn operations_target_label(
    host_id: &HostId,
    names: &HashMap<HostId, SharedString>,
) -> String {
    saved_host_display_name(host_id, names)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("SSH · {}", host_id.as_str()))
}

/// Return a non-blank saved name without rewriting its user-chosen prefix or spacing.
///
/// @author kongweiguang
fn saved_host_display_name<'a>(
    host_id: &HostId,
    names: &'a HashMap<HostId, SharedString>,
) -> Option<&'a str> {
    names
        .get(host_id)
        .map(SharedString::as_ref)
        .filter(|name| !name.trim().is_empty())
}

/// 按目标类型标出本机、远程主机或容器，避免把所有监控页都叫作同一泛化标题。
///
/// @author kongweiguang
fn server_page_title(target: &ServerInfoTarget) -> SharedString {
    match target {
        ServerInfoTarget::Local => crate::locale::text("本机服务器", "Local server"),
        ServerInfoTarget::Ssh { .. } => crate::locale::text("远程服务器", "Remote server"),
        ServerInfoTarget::Container { .. } => crate::locale::text("容器服务器", "Container server"),
    }
}

/// 生成 CPU load 文本时保留采集结果的精度且不将空数组解释成 0。
fn numeric_list(values: &[f64]) -> String {
    if values.is_empty() {
        "—".to_owned()
    } else {
        values
            .iter()
            .map(|value| format!("{value:.2}"))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// 已用/总量均按统一二进制单位格式化，不将未采集字段填成零。
///
/// @author kongweiguang
fn memory_pair(used: Option<u64>, total: Option<u64>) -> String {
    format!(
        "{} / {}",
        server_bytes_label(used),
        server_bytes_label(total)
    )
}

/// 设备、磁盘等百分比只接受有限的 0–100 读数；进程 CPU 的多核语义单独处理。
///
/// @author kongweiguang
fn percent(value: Option<f64>) -> String {
    value
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .map_or_else(|| "—".to_owned(), |value| format!("{value:.1}%"))
}

#[cfg(test)]
#[path = "../../../../tests/unit/operations_render_server.rs"]
mod tests;
