//! 文件目标的短期名称投影；FileEndpoint 与 ContainerTarget 仍是唯一操作身份。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};

use kerminal_application::{
    AppError, AppResult,
    connections::management::{HostGroup, HostManagementService},
    operations::OperationsService,
};
use kerminal_domain::{
    connections::RemoteProtocol,
    files::{ContainerRuntime, FileEndpoint},
    identity::HostId,
    operations::{ContainerListQuery, ContainerTarget},
};
use tokio_util::sync::CancellationToken;

/// 只区分主机和完整容器身份，不把路径或展示名拼进业务目标键。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileTargetNameKey {
    Host(HostId),
    Container(ContainerTarget),
}

/// 名称解析只使用桌面组合根已创建的主机目录与容器用例。
#[derive(Clone, Default)]
pub(crate) struct FileTargetNameLookup {
    hosts: Option<HostManagementService>,
    operations: Option<std::sync::Arc<OperationsService>>,
}

impl FileTargetNameLookup {
    /// 构造器仅接收已有 application service，缺少服务时由视图退回到完整稳定 ID。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        hosts: Option<HostManagementService>,
        operations: Option<std::sync::Arc<OperationsService>>,
    ) -> Self {
        Self { hosts, operations }
    }

    /// 仅查询已注入的 application service；缺少服务时稳定退回 ID，不猜测其它名称来源。
    ///
    /// @author kongweiguang
    fn can_resolve(&self, key: &FileTargetNameKey) -> bool {
        match key {
            FileTargetNameKey::Host(_) => self.hosts.is_some(),
            FileTargetNameKey::Container(_) => self.operations.is_some(),
        }
    }

    /// 名称读取按完整目标精确匹配；错误或缺少条目交给 UI 使用稳定 ID 作为回退。
    ///
    /// @author kongweiguang
    pub(crate) async fn resolve(
        &self,
        key: FileTargetNameKey,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("文件目标名称查询已取消"));
        }
        let name = match key {
            FileTargetNameKey::Host(host_id) => {
                let Some(hosts) = &self.hosts else {
                    return Ok(None);
                };
                hosts
                    .load_host(&host_id)
                    .await?
                    .filter(|host| host.id == host_id)
                    .map(|host| host.name)
            }
            FileTargetNameKey::Container(target) => {
                let Some(operations) = &self.operations else {
                    return Ok(None);
                };
                operations
                    .containers
                    .list(
                        ContainerListQuery {
                            host_id: target.host_id.clone(),
                            runtime: target.runtime,
                            include_stopped: true,
                        },
                        cancellation.clone(),
                    )
                    .await?
                    .into_iter()
                    .find(|container| container.target == target)
                    .map(|container| container.name)
            }
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("文件目标名称查询已取消"));
        }
        Ok(name.and_then(nonempty_name))
    }

    /// 主机选择器只投影可用 SFTP 目标的非凭据字段，避免 UI 缓存完整认证配置。
    ///
    /// @author kongweiguang
    pub(crate) async fn list_sftp_hosts(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<SftpHostDirectory> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("SFTP 主机列表读取已取消"));
        }
        let Some(host_service) = &self.hosts else {
            return Err(AppError::unsupported("主机目录服务尚未配置"));
        };
        let hosts = host_service.list_hosts().await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("SFTP 主机列表读取已取消"));
        }
        let mut groups = host_service.load_groups().await?.groups;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("SFTP 主机列表读取已取消"));
        }
        groups.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        let choices = hosts
            .into_iter()
            .filter(|host| {
                matches!(host.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
                    && host.sftp_options.enabled
            })
            .map(|host| SftpHostChoice {
                id: host.id,
                name: host.name,
                address: host.endpoint.host,
                username: host.endpoint.username,
                port: host.endpoint.port,
                protocol: host.protocol,
                group_id: host.group_id,
            })
            .collect();
        Ok(SftpHostDirectory {
            hosts: choices,
            groups,
        })
    }
}

/// SFTP picker 同时需要受支持主机和真实分组标题，避免把 group_id 当作用户可读标签。
///
/// @author kongweiguang
pub(crate) struct SftpHostDirectory {
    pub(crate) hosts: Vec<SftpHostChoice>,
    pub(crate) groups: Vec<HostGroup>,
}

/// 主机选择器仅保留搜索与展示所需字段，不保留凭据引用或完整 ManagedHost 快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SftpHostChoice {
    pub(crate) id: HostId,
    pub(crate) name: String,
    pub(crate) address: String,
    pub(crate) username: String,
    pub(crate) port: u16,
    pub(crate) protocol: RemoteProtocol,
    pub(crate) group_id: Option<String>,
}

impl SftpHostChoice {
    /// 列表详情明确协议类型，避免同名主机在选择时只能依赖地址猜测。
    ///
    /// @author kongweiguang
    pub(crate) fn protocol_label(&self) -> String {
        match &self.protocol {
            RemoteProtocol::Ssh => "SSH".to_owned(),
            RemoteProtocol::Sftp => "SFTP".to_owned(),
            RemoteProtocol::Telnet => "Telnet".to_owned(),
            RemoteProtocol::Serial => "Serial".to_owned(),
            RemoteProtocol::Rdp => "RDP".to_owned(),
        }
    }
}

/// 一次精确目标名称请求；代次和取消令牌由视图持有，迟到结果不能改写新目标。
pub(crate) struct FileTargetNameRequest {
    pub(crate) key: FileTargetNameKey,
    pub(crate) generation: u64,
    pub(crate) cancellation: CancellationToken,
}

struct PendingFileTargetName {
    generation: u64,
    cancellation: CancellationToken,
}

/// 当前视图目标集合的名称快照；左右槽或多个编辑器标签共享同一条精确查询结果。
#[derive(Default)]
pub(crate) struct FileTargetNameState {
    active: HashSet<FileTargetNameKey>,
    resolved: HashMap<FileTargetNameKey, Option<String>>,
    pending: HashMap<FileTargetNameKey, PendingFileTargetName>,
    next_generation: u64,
}

impl FileTargetNameState {
    /// 仅为当前 endpoint 集合发起一次查名；移出集合的请求先取消并失效，再清理缓存。
    ///
    /// @author kongweiguang
    pub(crate) fn synchronize(
        &mut self,
        endpoints: impl IntoIterator<Item = FileEndpoint>,
        lookup: &FileTargetNameLookup,
    ) -> Vec<FileTargetNameRequest> {
        let active = endpoints
            .into_iter()
            .flat_map(|endpoint| file_target_name_keys(&endpoint))
            .collect::<HashSet<_>>();

        self.pending.retain(|key, pending| {
            if active.contains(key) {
                true
            } else {
                pending.cancellation.cancel();
                false
            }
        });
        self.resolved.retain(|key, _| active.contains(key));
        self.active = active;

        let mut requests = Vec::new();
        for key in self.active.iter().cloned() {
            if self.resolved.contains_key(&key) || self.pending.contains_key(&key) {
                continue;
            }
            if !lookup.can_resolve(&key) {
                self.resolved.insert(key, None);
                continue;
            }

            self.next_generation = self.next_generation.wrapping_add(1).max(1);
            let generation = self.next_generation;
            let cancellation = CancellationToken::new();
            self.pending.insert(
                key.clone(),
                PendingFileTargetName {
                    generation,
                    cancellation: cancellation.clone(),
                },
            );
            requests.push(FileTargetNameRequest {
                key,
                generation,
                cancellation,
            });
        }
        requests
    }

    /// 仅接受仍在当前目标集合中的同代回执；失败保持 ID 回退并等待下一次显式刷新。
    ///
    /// @author kongweiguang
    pub(crate) fn complete(
        &mut self,
        key: &FileTargetNameKey,
        generation: u64,
        result: AppResult<Option<String>>,
    ) -> bool {
        if !self.active.contains(key)
            || !self
                .pending
                .get(key)
                .is_some_and(|pending| pending.generation == generation)
        {
            return false;
        }
        self.pending.remove(key);
        self.resolved
            .insert(key.clone(), result.ok().flatten().and_then(nonempty_name));
        true
    }

    /// 显式刷新时只失效该 endpoint 的名称项；目录路径刷新不改变目标身份。
    ///
    /// @author kongweiguang
    pub(crate) fn invalidate_endpoint(&mut self, endpoint: &FileEndpoint) {
        for key in file_target_name_keys(endpoint) {
            self.resolved.remove(&key);
            if let Some(pending) = self.pending.remove(&key) {
                pending.cancellation.cancel();
            }
        }
    }

    /// 只从当前目标快照取名称，未完成、失败或取消都不会伪造可读名称。
    ///
    /// @author kongweiguang
    fn name(&self, key: &FileTargetNameKey) -> Option<&str> {
        self.resolved.get(key).and_then(Option::as_deref)
    }

    /// 显示名优先，未能读取时保留可辨认的短 ID；详情始终包含完整目标身份。
    ///
    /// @author kongweiguang
    pub(crate) fn display(&self, endpoint: &FileEndpoint) -> FileTargetDisplay {
        match endpoint {
            FileEndpoint::Local { .. } => FileTargetDisplay {
                compact: crate::locale::text("本机", "This device").to_string(),
                detail: crate::locale::text("本机", "This device").to_string(),
            },
            FileEndpoint::Remote { host_id, .. } => {
                let name = self.name(&FileTargetNameKey::Host(host_id.clone()));
                let short = name
                    .map(str::to_owned)
                    .unwrap_or_else(|| short_id(host_id.as_str()).to_owned());
                let detail = name.map_or_else(
                    || {
                        format!(
                            "{} {}",
                            crate::locale::text("SSH 主机", "SSH host"),
                            host_id
                        )
                    },
                    |name| format!("SSH · {name} · {host_id}"),
                );
                FileTargetDisplay {
                    compact: format!("SSH · {short}"),
                    detail,
                }
            }
            FileEndpoint::Container {
                host_id,
                runtime,
                container_id,
                ..
            } => {
                let target = ContainerTarget {
                    host_id: host_id.clone(),
                    runtime: *runtime,
                    container_id: container_id.clone(),
                };
                let host_name = host_id
                    .as_ref()
                    .and_then(|host| self.name(&FileTargetNameKey::Host(host.clone())));
                let container_name = self.name(&FileTargetNameKey::Container(target));
                let runtime_label = container_runtime_label(*runtime);
                let container_compact = container_name
                    .map(str::to_owned)
                    .unwrap_or_else(|| short_id(container_id).to_owned());
                let container_detail = container_name.map_or_else(
                    || {
                        format!(
                            "{} {container_id}",
                            crate::locale::text("容器", "container")
                        )
                    },
                    |name| format!("{name} ({container_id})"),
                );
                let host_compact = host_id.as_ref().map(|host| {
                    host_name
                        .map(str::to_owned)
                        .unwrap_or_else(|| short_id(host.as_str()).to_owned())
                });
                let host_detail = host_id.as_ref().map(|host| {
                    host_name.map_or_else(|| host.to_string(), |name| format!("{name} ({host})"))
                });
                let compact = match host_compact {
                    Some(host) => format!("{host} · {runtime_label} · {container_compact}"),
                    None => format!("{runtime_label} · {container_compact}"),
                };
                let detail = match host_detail {
                    Some(host) => format!("{host} · {runtime_label} · {container_detail}"),
                    None => format!(
                        "{} · {runtime_label} · {container_detail}",
                        crate::locale::text("本机", "This device")
                    ),
                };
                FileTargetDisplay { compact, detail }
            }
        }
    }

    /// 将目标详情和 endpoint 原路径组合，仅用于可见标签、tooltip 与辅助技术说明。
    ///
    /// @author kongweiguang
    pub(crate) fn endpoint_detail(&self, endpoint: &FileEndpoint) -> String {
        let target = self.display(endpoint).detail;
        match endpoint {
            FileEndpoint::Local { path } => format!("{target} · {}", path.display()),
            FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
                format!("{target} · {}", path.as_str())
            }
        }
    }
}

impl Drop for FileTargetNameState {
    /// 视图被移除时协作取消仍在途的查名任务，避免后台请求继续更新已关闭窗口。
    fn drop(&mut self) {
        for pending in self.pending.values() {
            pending.cancellation.cancel();
        }
    }
}

/// 主机与容器 endpoint 同时保留所属 HostId 与完整 ContainerTarget 作为查询键。
///
/// @author kongweiguang
fn file_target_name_keys(endpoint: &FileEndpoint) -> Vec<FileTargetNameKey> {
    match endpoint {
        FileEndpoint::Local { .. } => Vec::new(),
        FileEndpoint::Remote { host_id, .. } => {
            vec![FileTargetNameKey::Host(host_id.clone())]
        }
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            ..
        } => {
            let mut keys = Vec::with_capacity(2);
            if let Some(host_id) = host_id {
                keys.push(FileTargetNameKey::Host(host_id.clone()));
            }
            keys.push(FileTargetNameKey::Container(ContainerTarget {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
            }));
            keys
        }
    }
}

/// 空白或控制字符名称不覆盖稳定 ID 回退，避免把损坏元数据显示成有效目标名称。
///
/// @author kongweiguang
fn nonempty_name(name: String) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && !name.chars().any(char::is_control)).then(|| name.to_owned())
}

/// 窄标签保持短小；完整 opaque ID 始终由详情投影展示。
///
/// @author kongweiguang
fn short_id(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

/// 容器运行时只映射到固定用户文案，不依赖 Debug 表示。
///
/// @author kongweiguang
fn container_runtime_label(runtime: ContainerRuntime) -> String {
    match runtime {
        ContainerRuntime::Docker => crate::locale::text("Docker", "Docker").to_string(),
        ContainerRuntime::Podman => crate::locale::text("Podman", "Podman").to_string(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileTargetDisplay {
    pub(crate) compact: String,
    pub(crate) detail: String,
}

#[cfg(test)]
#[path = "../../../tests/unit/files_target_names.rs"]
mod tests;
