// @author kongweiguang
use super::*;
use std::collections::BTreeMap;

const DOCKER_COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const DOCKER_COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
const DOCKER_COMPOSE_WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";
const DOCKER_COMPOSE_CONFIG_FILES_LABEL: &str = "com.docker.compose.project.config_files";
const DOCKER_COMPOSE_CONTAINER_NUMBER_LABEL: &str = "com.docker.compose.container-number";
const DOCKER_COMPOSE_ONEOFF_LABEL: &str = "com.docker.compose.oneoff";
const PODMAN_COMPOSE_PROJECT_LABEL: &str = "io.podman.compose.project";
const PODMAN_COMPOSE_SERVICE_LABEL: &str = "io.podman.compose.service";
const PODMAN_COMPOSE_WORKING_DIR_LABEL: &str = "io.podman.compose.project.working_dir";
const PODMAN_COMPOSE_CONFIG_FILES_LABEL: &str = "io.podman.compose.project.config_files";

const COMPOSE_LABEL_KEYS: &[&str] = &[
    DOCKER_COMPOSE_PROJECT_LABEL,
    DOCKER_COMPOSE_SERVICE_LABEL,
    DOCKER_COMPOSE_WORKING_DIR_LABEL,
    DOCKER_COMPOSE_CONFIG_FILES_LABEL,
    DOCKER_COMPOSE_CONTAINER_NUMBER_LABEL,
    DOCKER_COMPOSE_ONEOFF_LABEL,
    PODMAN_COMPOSE_PROJECT_LABEL,
    PODMAN_COMPOSE_SERVICE_LABEL,
    PODMAN_COMPOSE_WORKING_DIR_LABEL,
    PODMAN_COMPOSE_CONFIG_FILES_LABEL,
];

/// Prefer whole-document JSON when available while retaining Docker's line-oriented output contract.
pub fn parse_container_list_output(
    host_id: &str,
    runtime: ContainerRuntime,
    output: &str,
) -> AppResult<Vec<DockerContainerSummary>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        match &value {
            Value::Array(items) => {
                return items
                    .iter()
                    .map(|item| parse_container_value(host_id, runtime, item))
                    .collect();
            }
            Value::Object(_) => {
                return Ok(vec![parse_container_value(host_id, runtime, &value)?]);
            }
            _ => {}
        }
    }

    trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| parse_container_line(host_id, runtime, line))
        .collect()
}

/// Preserve the line-based error context used when Docker emits JSONL.
pub(super) fn parse_container_line(
    host_id: &str,
    runtime: ContainerRuntime,
    line: &str,
) -> AppResult<DockerContainerSummary> {
    let value: Value = serde_json::from_str(line).map_err(|error| {
        AppError::Docker(format!("无法解析容器列表 JSON: {error}; line={line}"))
    })?;
    parse_container_value(host_id, runtime, &value)
}

/// Decode one row without imposing a serialization format on Docker or Podman callers.
fn parse_container_value(
    host_id: &str,
    runtime: ContainerRuntime,
    value: &Value,
) -> AppResult<DockerContainerSummary> {
    let id = field_string(value, "ID")
        .or_else(|| field_string(value, "Id"))
        .ok_or_else(|| AppError::Docker("容器列表缺少 ID 字段".to_owned()))?;
    let name = container_name_field(value, "Names")
        .or_else(|| container_name_field(value, "Name"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| short_container_id(&id));
    let image = field_string(value, "Image").unwrap_or_default();
    let status_text = field_string(value, "Status").unwrap_or_default();
    let state = field_string(value, "State").unwrap_or_default();
    let status = DockerContainerStatus::from_cli_fields(&state, &status_text);
    let ports = container_ports(value.get("Ports"));
    let raw_labels = container_labels_from_list_value(value);
    let labels = compose_labels_from_map(&raw_labels);
    let compose = parse_compose_metadata(&labels);
    let target = RemoteTargetRef::DockerContainer {
        host_id: host_id.to_owned(),
        container_id: id.clone(),
        runtime,
        container_name: Some(name.clone()),
        user: None,
        workdir: None,
    };

    Ok(DockerContainerSummary {
        host_id: host_id.to_owned(),
        short_id: short_container_id(&id),
        id,
        name,
        image,
        status_text,
        status,
        state,
        ports,
        runtime,
        target,
        capabilities: TargetCapabilities::docker_container(),
        compose,
        labels,
    })
}

/// Podman returns names as an array while Docker returns a comma-separated string.
fn container_name_field(value: &Value, key: &str) -> Option<String> {
    let name = match value.get(key)? {
        Value::String(name) => Some(name.as_str()),
        Value::Array(names) => names.iter().find_map(Value::as_str),
        _ => None,
    }?;
    let name = first_csv_value(name.trim());
    (!name.is_empty()).then_some(name)
}

/// Normalize both Docker's display string and Podman's structured port mappings for the existing UI contract.
fn container_ports(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(ports)) => ports
            .split(',')
            .map(str::trim)
            .filter(|port| !port.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::Array(ports)) => ports.iter().filter_map(port_mapping_text).collect(),
        Some(port @ Value::Object(_)) => port_mapping_text(port).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Skip incomplete mappings so a missing optional port field cannot break container discovery.
fn port_mapping_text(value: &Value) -> Option<String> {
    if let Value::String(port) = value {
        let port = port.trim();
        return (!port.is_empty()).then(|| port.to_owned());
    }
    let Value::Object(_) = value else {
        return None;
    };

    let container_port =
        object_string(value, &["ContainerPort", "container_port", "containerPort"])?;
    let protocol = object_string(value, &["Protocol", "protocol"]).unwrap_or_else(|| "tcp".into());
    let container = format!("{container_port}/{protocol}");
    let host_port =
        object_string(value, &["HostPort", "host_port", "hostPort"]).filter(|port| port != "0");
    let Some(host_port) = host_port else {
        return Some(container);
    };
    let host_ip = object_string(value, &["HostIP", "HostIp", "host_ip", "hostIp"])
        .filter(|ip| !ip.is_empty());
    let host = match host_ip {
        Some(ip) if ip.contains(':') && !ip.starts_with('[') => format!("[{ip}]:{host_port}"),
        Some(ip) => format!("{ip}:{host_port}"),
        None => host_port,
    };
    Some(format!("{host}->{container}"))
}

/// Accept numeric and textual fields because Podman CLI versions serialize ports differently.
fn object_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match value.get(*key)? {
        Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

pub fn parse_container_label_inspect_output(
    output: &str,
) -> AppResult<BTreeMap<String, BTreeMap<String, String>>> {
    let mut labels_by_id = BTreeMap::new();
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(labels_by_id);
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(items) = value.as_array() {
            for item in items {
                insert_inspect_labels(&mut labels_by_id, item);
            }
            return Ok(labels_by_id);
        }
        insert_inspect_labels(&mut labels_by_id, &value);
        return Ok(labels_by_id);
    }

    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value: Value = serde_json::from_str(line).map_err(|error| {
            AppError::Docker(format!(
                "无法解析容器 labels inspect JSON: {error}; line={line}"
            ))
        })?;
        insert_inspect_labels(&mut labels_by_id, &value);
    }

    Ok(labels_by_id)
}

pub fn merge_container_summary_labels(
    containers: &mut [DockerContainerSummary],
    labels_by_id: &BTreeMap<String, BTreeMap<String, String>>,
) {
    for container in containers {
        let Some(inspect_labels) = labels_for_container(labels_by_id, &container.id) else {
            continue;
        };
        let mut labels = container.labels.clone();
        labels.extend(compose_labels_from_map(inspect_labels));
        container.compose = parse_compose_metadata(&labels);
        container.labels = labels;
    }
}

pub fn container_summary_needs_label_inspect(container: &DockerContainerSummary) -> bool {
    if container.labels.is_empty() {
        return true;
    }

    container
        .compose
        .as_ref()
        .map(|metadata| {
            matches!(
                metadata.runtime_family,
                DockerComposeRuntimeFamily::DockerCompose
            ) && (metadata.working_dir.is_none() || metadata.config_files.is_empty())
        })
        .unwrap_or(false)
}

pub fn parse_compose_metadata(labels: &BTreeMap<String, String>) -> Option<DockerComposeMetadata> {
    let docker_project = non_empty_label(labels, DOCKER_COMPOSE_PROJECT_LABEL);
    let podman_project = non_empty_label(labels, PODMAN_COMPOSE_PROJECT_LABEL);
    let (project, runtime_family) = if let Some(project) = docker_project {
        (project, DockerComposeRuntimeFamily::DockerCompose)
    } else {
        (podman_project?, DockerComposeRuntimeFamily::PodmanCompose)
    };

    let service = match runtime_family {
        DockerComposeRuntimeFamily::DockerCompose => {
            non_empty_label(labels, DOCKER_COMPOSE_SERVICE_LABEL)
                .or_else(|| non_empty_label(labels, PODMAN_COMPOSE_SERVICE_LABEL))
        }
        DockerComposeRuntimeFamily::PodmanCompose => {
            non_empty_label(labels, PODMAN_COMPOSE_SERVICE_LABEL)
                .or_else(|| non_empty_label(labels, DOCKER_COMPOSE_SERVICE_LABEL))
        }
    };
    let working_dir = match runtime_family {
        DockerComposeRuntimeFamily::DockerCompose => {
            non_empty_label(labels, DOCKER_COMPOSE_WORKING_DIR_LABEL)
                .or_else(|| non_empty_label(labels, PODMAN_COMPOSE_WORKING_DIR_LABEL))
        }
        DockerComposeRuntimeFamily::PodmanCompose => {
            non_empty_label(labels, PODMAN_COMPOSE_WORKING_DIR_LABEL)
                .or_else(|| non_empty_label(labels, DOCKER_COMPOSE_WORKING_DIR_LABEL))
        }
    };
    let config_files = match runtime_family {
        DockerComposeRuntimeFamily::DockerCompose => {
            non_empty_label(labels, DOCKER_COMPOSE_CONFIG_FILES_LABEL)
                .or_else(|| non_empty_label(labels, PODMAN_COMPOSE_CONFIG_FILES_LABEL))
        }
        DockerComposeRuntimeFamily::PodmanCompose => {
            non_empty_label(labels, PODMAN_COMPOSE_CONFIG_FILES_LABEL)
                .or_else(|| non_empty_label(labels, DOCKER_COMPOSE_CONFIG_FILES_LABEL))
        }
    }
    .map(|value| split_compose_config_files(&value))
    .unwrap_or_default();
    let config_paths = resolve_compose_config_paths(working_dir.as_deref(), &config_files);
    let container_number = non_empty_label(labels, DOCKER_COMPOSE_CONTAINER_NUMBER_LABEL);
    let oneoff = non_empty_label(labels, DOCKER_COMPOSE_ONEOFF_LABEL)
        .map(|value| parse_compose_bool(&value))
        .unwrap_or(false);

    Some(DockerComposeMetadata {
        project,
        service,
        working_dir,
        config_files,
        config_paths,
        container_number,
        oneoff,
        runtime_family,
    })
}

fn insert_inspect_labels(
    labels_by_id: &mut BTreeMap<String, BTreeMap<String, String>>,
    value: &Value,
) {
    let id = value
        .get("Id")
        .or_else(|| value.get("ID"))
        .and_then(json_label_string);
    let Some(id) = id.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    let labels = value
        .pointer("/Config/Labels")
        .or_else(|| value.get("Labels"))
        .map(label_map_from_value)
        .unwrap_or_default();
    labels_by_id.insert(id, labels);
}

fn labels_for_container<'a>(
    labels_by_id: &'a BTreeMap<String, BTreeMap<String, String>>,
    container_id: &str,
) -> Option<&'a BTreeMap<String, String>> {
    labels_by_id.get(container_id).or_else(|| {
        labels_by_id
            .iter()
            .find(|(id, _)| id.starts_with(container_id) || container_id.starts_with(id.as_str()))
            .map(|(_, labels)| labels)
    })
}

fn container_labels_from_list_value(value: &Value) -> BTreeMap<String, String> {
    value
        .get("Labels")
        .or_else(|| value.get("labels"))
        .map(label_map_from_value)
        .unwrap_or_default()
}

fn label_map_from_value(value: &Value) -> BTreeMap<String, String> {
    match value {
        Value::Object(map) => map
            .iter()
            .filter_map(|(key, value)| json_label_string(value).map(|text| (key.clone(), text)))
            .collect(),
        Value::String(text) => label_map_from_string(text),
        _ => BTreeMap::new(),
    }
}

fn label_map_from_string(labels: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    for raw_part in labels.split(',') {
        if let Some((key, value)) = raw_part.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                if let Some(previous_key) = current_key.replace(key.to_owned()) {
                    values.insert(previous_key, current_value.trim().to_owned());
                    current_value.clear();
                }
                current_value.push_str(value.trim());
                continue;
            }
        }

        if !current_value.is_empty() {
            current_value.push(',');
        }
        current_value.push_str(raw_part.trim());
    }

    if let Some(key) = current_key {
        values.insert(key, current_value.trim().to_owned());
    }

    values
}

fn compose_labels_from_map(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    COMPOSE_LABEL_KEYS
        .iter()
        .filter_map(|key| non_empty_label(labels, key).map(|value| ((*key).to_owned(), value)))
        .collect()
}

fn non_empty_label(labels: &BTreeMap<String, String>, key: &str) -> Option<String> {
    labels
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn split_compose_config_files(value: &str) -> Vec<String> {
    let separator = if value.contains(',') { ',' } else { ';' };
    value
        .split(separator)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn resolve_compose_config_paths(working_dir: Option<&str>, config_files: &[String]) -> Vec<String> {
    config_files
        .iter()
        .map(|path| {
            if is_absolute_remote_path(path) {
                path.clone()
            } else {
                working_dir
                    .map(|base| join_remote_file_path(base, path))
                    .unwrap_or_else(|| path.clone())
            }
        })
        .collect()
}

fn is_absolute_remote_path(path: &str) -> bool {
    let path = path.trim();
    if path.starts_with('/') || path.starts_with("\\\\") {
        return true;
    }

    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[1] == b':'
        && bytes[0].is_ascii_alphabetic()
        && matches!(bytes[2], b'/' | b'\\')
}

fn join_remote_file_path(base: &str, child: &str) -> String {
    let base = base.trim().trim_end_matches(['/', '\\']);
    if base.is_empty() {
        return child.to_owned();
    }
    let separator = if base.contains('\\') && !base.contains('/') {
        "\\"
    } else {
        "/"
    };
    let child = child.trim_start_matches(['/', '\\']);
    format!("{base}{separator}{child}")
}

fn parse_compose_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y"
    )
}

fn json_label_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

pub fn parse_ls_entries(base_path: &str, output: &str) -> AppResult<Vec<SftpEntry>> {
    let mut entries = Vec::new();
    for raw in output.lines().map(str::trim_end) {
        if raw.is_empty() || raw.starts_with("total ") {
            continue;
        }
        let Some(entry) = parse_ls_entry(base_path, raw) else {
            continue;
        };
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| {
        sftp_entry_kind_rank(&left.kind)
            .cmp(&sftp_entry_kind_rank(&right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

pub(super) fn sftp_entry_kind_rank(kind: &SftpEntryKind) -> u8 {
    match kind {
        SftpEntryKind::Directory => 0,
        SftpEntryKind::File => 1,
        SftpEntryKind::Symlink => 2,
        SftpEntryKind::Other => 3,
    }
}

pub(super) fn parse_ls_entry(base_path: &str, raw: &str) -> Option<SftpEntry> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let permissions = parts[0].to_owned();
    let kind = match permissions.chars().next() {
        Some('d') => SftpEntryKind::Directory,
        Some('l') => SftpEntryKind::Symlink,
        Some('-') => SftpEntryKind::File,
        _ => SftpEntryKind::Other,
    };
    let size = parts.get(4).and_then(|value| value.parse::<u64>().ok());
    let modified = parts.get(5..8).map(|values| values.join(" "));
    let name = parts[8..].join(" ");
    let display_name = if matches!(kind, SftpEntryKind::Symlink) {
        name.split(" -> ").next().unwrap_or(&name).to_owned()
    } else {
        name
    };
    let path = join_remote_path(base_path, &display_name);

    Some(SftpEntry {
        name: display_name,
        path,
        kind,
        size,
        permissions: Some(permissions),
        modified,
        raw: raw.to_owned(),
    })
}

pub(super) fn join_remote_path(base_path: &str, child: &str) -> String {
    let child = child.trim_start_matches('/');
    if base_path == "/" {
        format!("/{child}")
    } else {
        format!("{}/{}", base_path.trim_end_matches('/'), child)
    }
}

pub fn split_preview_output(output: &str) -> AppResult<(String, usize, Option<usize>)> {
    let marker_prefix = "__KERMINAL_BYTES:";
    let Some((marker, content)) = output.split_once('\n') else {
        return Err(AppError::Docker("容器文件预览输出缺少大小标记".to_owned()));
    };
    let total_bytes = marker
        .strip_prefix(marker_prefix)
        .and_then(|value| value.strip_suffix("__"))
        .and_then(|value| value.parse::<usize>().ok());
    let bytes_read = content.len();
    Ok((content.to_owned(), bytes_read, total_bytes))
}
