//! Compose YAML 页内预览请求身份和文本边界测试。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    files::{FileRevision, RemotePath},
    identity::HostId,
    operations::ContainerRuntime,
};

/// 构造包含两个 YAML 文件的项目，覆盖路径切换和项目身份重置。
///
/// @author kongweiguang
fn project(name: &str, working_dir: &str) -> ComposeProjectTarget {
    ComposeProjectTarget {
        host_id: Some(HostId::new()),
        runtime: ContainerRuntime::Docker,
        project: name.to_owned(),
        working_dir: working_dir.to_owned(),
        config_files: vec![
            format!("{working_dir}/compose.yaml"),
            format!("{working_dir}/compose.override.yaml"),
        ],
    }
}

/// 为纯状态测试创建不含已绑定终端的 ticket。
///
/// @author kongweiguang
fn action_ticket(generation: u64) -> ContainerPanelActionTicket {
    ContainerPanelActionTicket {
        context: None,
        runtime: ContainerRuntime::Docker,
        generation,
        target: None,
    }
}

/// Compose endpoint 使用远端 HostId 和绝对 RemotePath，避免测试依赖 OS 路径规则。
///
/// @author kongweiguang
fn endpoint(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::Remote {
        host_id: host_id.clone(),
        path: RemotePath::parse(path).expect("absolute remote path"),
    }
}

/// 文件样本只暴露请求响应中的有效元数据，不需要真实 host 或文件服务。
///
/// @author kongweiguang
fn file_preview(endpoint: FileEndpoint, bytes: &[u8], binary: bool) -> FilePreview {
    FilePreview {
        endpoint,
        bytes: bytes.to_vec(),
        total_size: Some(bytes.len() as u64),
        truncated: false,
        binary,
        revision: Some(FileRevision {
            size: bytes.len() as u64,
            modified: Some("2026-10-27T00:00:00Z".to_owned()),
            permissions: Some(0o644),
            content_digest: None,
        }),
    }
}

/// 每个请求拥有单调 request id，迟到的 id 或不同路径回执不能命中当前请求。
///
/// @author kongweiguang
#[test]
fn request_identity_rejects_stale_id_and_endpoint() {
    let project = project("web", "/srv/web");
    let first_path = project.config_files[0].clone();
    let first_endpoint = endpoint(project.host_id.as_ref().unwrap(), &first_path);
    let other_endpoint = endpoint(
        project.host_id.as_ref().unwrap(),
        "/srv/web/compose.override.yaml",
    );
    let mut state = ComposeYamlPreviewState::default();
    state.select_project(&project);

    let request_id = state
        .begin_request(
            &project,
            &first_path,
            first_endpoint.clone(),
            action_ticket(1),
        )
        .expect("request id");

    assert!(state.is_loading());
    assert!(state.response_matches(request_id, &first_endpoint));
    assert!(!state.response_matches(request_id + 1, &first_endpoint));
    assert!(!state.response_matches(request_id, &other_endpoint));
}

/// 项目切换和显式失效会清除旧预览，但不会复用此前发出的 request id。
///
/// @author kongweiguang
#[test]
fn project_change_and_invalidation_drop_old_preview_tickets() {
    let first = project("web", "/srv/web");
    let first_path = first.config_files[0].clone();
    let first_endpoint = endpoint(first.host_id.as_ref().unwrap(), &first_path);
    let mut state = ComposeYamlPreviewState::default();
    state.select_project(&first);
    let old_id = state
        .begin_request(
            &first,
            &first_path,
            first_endpoint.clone(),
            action_ticket(1),
        )
        .expect("first request id");

    let second = project("api", "/srv/api");
    assert!(state.select_project(&second));
    assert!(!state.response_matches(old_id, &first_endpoint));
    assert_eq!(
        state.selected_path(),
        second.config_files.first().map(String::as_str)
    );

    let second_path = second.config_files[0].clone();
    let second_endpoint = endpoint(second.host_id.as_ref().unwrap(), &second_path);
    let new_id = state
        .begin_request(
            &second,
            &second_path,
            second_endpoint.clone(),
            action_ticket(2),
        )
        .expect("second request id");
    state.invalidate();

    assert!(new_id > old_id);
    assert!(!state.response_matches(new_id, &second_endpoint));
    assert!(!state.is_loading());
    assert!(state.preview().is_none());
}

/// 更新同项目的 selected path 会失效旧请求，再由新 request id 独占结果位置。
///
/// @author kongweiguang
#[test]
fn selecting_another_config_path_replaces_the_pending_request() {
    let project = project("web", "/srv/web");
    let first_path = project.config_files[0].clone();
    let second_path = project.config_files[1].clone();
    let first_endpoint = endpoint(project.host_id.as_ref().unwrap(), &first_path);
    let second_endpoint = endpoint(project.host_id.as_ref().unwrap(), &second_path);
    let mut state = ComposeYamlPreviewState::default();
    state.select_project(&project);
    let first_id = state
        .begin_request(
            &project,
            &first_path,
            first_endpoint.clone(),
            action_ticket(1),
        )
        .expect("first request id");
    let second_id = state
        .begin_request(
            &project,
            &second_path,
            second_endpoint.clone(),
            action_ticket(1),
        )
        .expect("second request id");

    assert!(second_id > first_id);
    assert!(!state.response_matches(first_id, &first_endpoint));
    assert!(state.response_matches(second_id, &second_endpoint));
    assert_eq!(state.selected_path(), Some(second_path.as_str()));
}

/// binary 标记和非法 UTF-8 都不会将字节替换后冒充用户可读的 YAML 文本。
///
/// @author kongweiguang
#[test]
fn preview_text_rejects_binary_and_invalid_utf8() {
    let host_id = HostId::new();
    let yaml_endpoint = endpoint(&host_id, "/srv/web/compose.yaml");
    let text = file_preview(yaml_endpoint.clone(), b"services:\r\n  web:\r\n", false);
    let binary = file_preview(yaml_endpoint.clone(), b"\0\xff", true);
    let invalid_utf8 = file_preview(yaml_endpoint, b"\xff", false);

    assert_eq!(
        compose_yaml_text(&text),
        Ok(Some("services:\r\n  web:\r\n".to_owned()))
    );
    assert_eq!(compose_yaml_text(&binary), Ok(None));
    assert_eq!(
        compose_yaml_text(&invalid_utf8),
        Err(ComposeYamlContentIssue::InvalidUtf8)
    );
}

/// 截断提示和文件 revision 元数据保持可见，预览状态明确标记为只读。
///
/// @author kongweiguang
#[test]
fn metadata_keeps_preview_boundary_and_readonly_contract_visible() {
    let host_id = HostId::new();
    let mut preview = file_preview(endpoint(&host_id, "/srv/web/compose.yaml"), b"a\r\n", false);
    preview.total_size = Some(1024);
    preview.truncated = true;

    let metadata = compose_yaml_metadata_items(&preview);

    assert!(metadata.contains(&"3 B/1.0 KB".to_owned()));
    assert!(metadata.contains(&"0644".to_owned()));
    assert!(metadata.contains(&"UTF-8".to_owned()));
    assert!(metadata.contains(&"CRLF".to_owned()));
    assert!(metadata.contains(&"RO".to_owned()));
    assert!(metadata.contains(&"2026-10-27T00:00:00Z".to_owned()));
}
