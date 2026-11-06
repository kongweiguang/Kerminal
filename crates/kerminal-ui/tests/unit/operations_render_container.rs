//! 固定容器右栏概览文本布局的纯函数回归测试。
//!
//! @author kongweiguang

use super::super::add_container_detail_soft_breaks;
use super::{
    ComposeProjectDisplayMetadata, ContainerRowActivation, ContainerStatsDisplay,
    compose_project_display_metadata, compose_project_element_key, container_row_activation,
    container_stats_display, format_container_stats_raw,
};
use gpui_kit::{ClickEvent, KeyboardButton, KeyboardClickEvent, MouseClickEvent};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ComposeMetadata, ComposeProjectTarget, ContainerRuntime, ContainerStats, ContainerStatus,
        ContainerSummary, ContainerTarget,
    },
};

/// 为纯展示测试提供稳定的完整容器目标，不从项目名推导身份。
///
/// @author kongweiguang
fn container_target() -> ContainerTarget {
    ContainerTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        container_id: "container-123".to_owned(),
    }
}

/// 长镜像和路径只增加零宽断点；实际换行交给可用宽度，复制仍可用原值。
///
/// @author kongweiguang
#[test]
fn container_detail_soft_breaks_preserve_long_ascii_values() {
    let original = "registry.example.com/team-with-a-long-name/container-image:release-2026";
    let display = add_container_detail_soft_breaks(original);

    assert_eq!(display.replace('\u{200b}', ""), original);
    assert!(display.contains('\u{200b}'));
    assert!(!display.contains('\n'));
}

/// 长 Unicode 值不依赖英文分隔符，并且在组合字符的 grapheme 边界提供软断点。
///
/// @author kongweiguang
#[test]
fn container_detail_soft_breaks_keep_unicode_graphemes_intact() {
    let original = "e\u{301}".repeat(25);
    let display = add_container_detail_soft_breaks(&original);

    assert_eq!(display.replace('\u{200b}', ""), original);
    assert!(display.contains('\u{200b}'));
    assert!(!display.contains('\n'));
    assert!(
        display
            .split('\u{200b}')
            .all(|chunk| chunk.ends_with('\u{301}'))
    );
}

/// 使用实际 ClickEvent 区分 Enter 和空格，避免键盘快捷入口覆盖普通详情动作。
///
/// @author kongweiguang
#[test]
fn host_container_row_distinguishes_enter_from_space() {
    let mouse = ClickEvent::Mouse(MouseClickEvent::default());
    let enter = ClickEvent::Keyboard(KeyboardClickEvent {
        button: KeyboardButton::Enter,
        ..Default::default()
    });
    let space = ClickEvent::Keyboard(KeyboardClickEvent {
        button: KeyboardButton::Space,
        ..Default::default()
    });
    assert_eq!(
        container_row_activation(false, &mouse, ContainerStatus::Running),
        ContainerRowActivation::Inspect
    );
    assert_eq!(
        container_row_activation(false, &enter, ContainerStatus::Running),
        ContainerRowActivation::Enter
    );
    assert_eq!(
        container_row_activation(false, &space, ContainerStatus::Running),
        ContainerRowActivation::Inspect
    );
    for event in [&mouse, &enter, &space] {
        assert_eq!(
            container_row_activation(false, event, ContainerStatus::Exited),
            ContainerRowActivation::Inspect
        );
    }
}

/// Compose 项目内的容器名称无论使用哪种激活键都进入详情，终端由独立按钮执行。
///
/// @author kongweiguang
#[test]
fn compose_container_row_activation_opens_details() {
    for button in [KeyboardButton::Enter, KeyboardButton::Space] {
        let event = ClickEvent::Keyboard(KeyboardClickEvent {
            button,
            ..Default::default()
        });
        assert_eq!(
            container_row_activation(true, &event, ContainerStatus::Running),
            ContainerRowActivation::Inspect
        );
    }
}

/// 监控展示只读结构化字段并保留原始 JSON，不依赖本地化摘要反向解析。
///
/// @author kongweiguang
#[test]
fn container_stats_display_keeps_typed_metrics_and_raw_json() {
    let stats = ContainerStats {
        target: container_target(),
        cpu_percent: Some("12.5%".to_owned()),
        memory_percent: Some("31.0%".to_owned()),
        memory_usage: Some("120 MiB / 1 GiB".to_owned()),
        network_io: Some("1.2 MB / 3.4 MB".to_owned()),
        block_io: Some("5 MB / 6 MB".to_owned()),
        pids: Some("8".to_owned()),
        raw: "{\"cpu_stats\":{}}".to_owned(),
    };

    assert_eq!(
        container_stats_display(Some(&stats)),
        ContainerStatsDisplay {
            cpu_percent: "12.5%".to_owned(),
            memory_percent: "31.0%".to_owned(),
            pids: "8".to_owned(),
            network_io: "1.2 MB / 3.4 MB".to_owned(),
            memory_usage: "120 MiB / 1 GiB".to_owned(),
            block_io: "5 MB / 6 MB".to_owned(),
            raw: "{\"cpu_stats\":{}}".to_owned(),
        }
    );
}

/// 缺失监控字段显示为统一占位符，避免空字符串被误解为零值。
///
/// @author kongweiguang
#[test]
fn container_stats_display_marks_missing_metrics_without_losing_raw_data() {
    let stats = ContainerStats {
        target: container_target(),
        cpu_percent: None,
        memory_percent: None,
        memory_usage: None,
        network_io: None,
        block_io: None,
        pids: None,
        raw: "raw".to_owned(),
    };

    let display = container_stats_display(Some(&stats));

    assert_eq!(display.cpu_percent, "-");
    assert_eq!(display.memory_percent, "-");
    assert_eq!(display.pids, "-");
    assert_eq!(display.network_io, "-");
    assert_eq!(display.memory_usage, "-");
    assert_eq!(display.block_io, "-");
    assert_eq!(display.raw, "raw");
}

/// 尚未收到监控结果时仍渲染 main 的占位符与空原始数据状态。
///
/// @author kongweiguang
#[test]
fn container_stats_display_has_stable_empty_state_before_first_response() {
    let display = container_stats_display(None);

    assert_eq!(display.cpu_percent, "-");
    assert_eq!(display.memory_percent, "-");
    assert_eq!(display.pids, "-");
    assert_eq!(display.network_io, "-");
    assert_eq!(display.memory_usage, "-");
    assert_eq!(display.block_io, "-");
    assert!(display.raw.is_empty());
}

/// JSON 展示可折行但不能丢字段或裁掉长 ID；反序列化对比防止漂亮输出改变原始语义。
///
/// @author kongweiguang
#[test]
fn container_stats_json_is_multiline_and_preserves_complete_values() {
    let original = serde_json::json!({
        "Container": "a".repeat(128),
        "CPUPerc": "12.5%",
        "nested": { "samples": [1, 2, 3], "名称": "容器统计" }
    });
    let raw = serde_json::to_string(&original).expect("serialize sample");
    let formatted = format_container_stats_raw(&raw);

    assert!(formatted.lines().count() > 5);
    assert!(formatted.contains(&"a".repeat(128)));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&formatted).expect("valid pretty JSON"),
        original
    );
}

/// 非 JSON 诊断须逐字保留，不能因格式化失败把可恢复的原始输出变为空白。
///
/// @author kongweiguang
#[test]
fn container_stats_non_json_keeps_complete_diagnostic() {
    for raw in [
        "",
        "runtime returned no sample\nretry later",
        "{\"partial\":",
    ] {
        assert_eq!(format_container_stats_raw(raw), raw);
    }
}

/// Compose 空项目仍使用完整目标解析相对 YAML 路径并稳定显示 runtime family。
///
/// @author kongweiguang
#[test]
fn compose_project_display_resolves_paths_for_retained_empty_projects() {
    let target = ComposeProjectTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Podman,
        project: "web".to_owned(),
        working_dir: "/srv/web".to_owned(),
        config_files: vec!["compose.yaml".to_owned()],
    };

    let metadata = compose_project_display_metadata(&target, &[]);

    assert_eq!(
        metadata,
        ComposeProjectDisplayMetadata {
            runtime_family: "podman",
            config_paths: vec!["/srv/web/compose.yaml".to_owned()],
            warnings: Vec::new(),
        }
    );
}

/// 同名项目在列表重排后仍按完整身份保留菜单及焦点，不能复用另一个目录的控件。
///
/// @author kongweiguang
#[test]
fn compose_project_controls_use_full_stable_identity() {
    let first = ComposeProjectTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        project: "web".to_owned(),
        working_dir: "/srv/one".to_owned(),
        config_files: vec!["compose.yaml".to_owned()],
    };
    let mut second = first.clone();
    second.working_dir = "/srv/two".to_owned();
    assert_eq!(
        compose_project_element_key(&first),
        compose_project_element_key(&first.clone())
    );
    assert_ne!(
        compose_project_element_key(&first),
        compose_project_element_key(&second)
    );
}

/// 每个 Compose 容器缺少 workingDir 时保留警告，不把未解析相对路径伪装成绝对路径。
///
/// @author kongweiguang
#[test]
fn compose_project_display_warns_for_relative_yaml_without_working_directory() {
    let target = ComposeProjectTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        project: "web".to_owned(),
        working_dir: "/stale/target/fallback".to_owned(),
        config_files: vec!["fallback.yaml".to_owned()],
    };
    let container = ContainerSummary {
        target: container_target(),
        short_id: "container".to_owned(),
        name: "web-1".to_owned(),
        image: "nginx:latest".to_owned(),
        status: ContainerStatus::Running,
        status_text: "running".to_owned(),
        ports: Vec::new(),
        labels: vec![("io.podman.compose.project".to_owned(), "web".to_owned())],
        compose: Some(ComposeMetadata {
            project: Some("web".to_owned()),
            config_files: vec!["compose.yaml".to_owned()],
            ..ComposeMetadata::default()
        }),
    };

    let metadata = compose_project_display_metadata(&target, &[container]);

    assert_eq!(metadata.runtime_family, "podman");
    assert_eq!(metadata.config_paths, vec!["compose.yaml".to_owned()]);
    assert_eq!(
        metadata.warnings,
        vec!["Compose YAML 包含相对路径但缺少 workingDir".to_owned()]
    );
}
