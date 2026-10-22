//! 已冻结 schema v3 主机与分组配置文件的基础设施适配器。
//!
//! @author kongweiguang

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        HostConnectionRepository,
        management::{
            HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroup,
            HostGroupDeletionReceipt, HostGroupDraft, HostGroupSnapshot, HostManagementRepository,
            HostSftpOptions, HostTerminalOptions, ManagedHost, ManagedJumpHost, RdpDisplayOptions,
            host_note_is_valid, is_reserved_serial_host_tag, normalize_host_note,
        },
    },
    platform::external_launch::{ExternalTargetIdentity, SavedExternalTargetLookup},
    preferences::ConfigurationRepository,
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, ProxyProfile, ProxyProtocol,
        RemoteProtocol, RemoteShellDialect, SshAuthentication, SshRoute,
        credentials::{CredentialMaterial, CredentialReference},
    },
    identity::HostId,
    preferences::{ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode},
    sessions::{
        SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
    },
};
use serde::Deserialize;
use tokio::task;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::configuration::ConfigurationStore;

const HOST_SCHEMA_VERSION: u32 = 3;
const GROUP_SCHEMA_VERSION: u32 = 1;
const FAVORITE_TAG: &str = "favorite";

/// 通过同一 ConfigurationStore 提供列表 CRUD 和 SSH runtime 的 HostConnectionRepository。
#[derive(Clone)]
pub struct HostConfigurationRepository {
    configuration: Arc<dyn ConfigurationRepository>,
    store: ConfigurationStore,
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct HostFile {
    schema_version: u32,
    protocol: String,
    id: String,
    group_id: Option<String>,
    name: String,
    #[serde(default)]
    note: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    serial: Option<SerialOptionsFile>,
    auth_type: String,
    credential_ref: Option<String>,
    secret_ref: Option<String>,
    key_passphrase_ref: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    ssh_options: SshOptionsFile,
    #[serde(default)]
    rdp_options: RdpOptionsFile,
    sort_order: i64,
    created_at: String,
    updated_at: String,
}

/// 串口只在专用表中持久化，避免把设备路径伪装成网络 endpoint。
#[derive(Debug, Deserialize)]
struct SerialOptionsFile {
    device: String,
    baud_rate: u32,
    data_bits: u8,
    stop_bits: u8,
    parity: String,
    flow_control: String,
}

#[derive(Debug, Default, Deserialize)]
struct SshOptionsFile {
    #[serde(default)]
    proxy: ProxyFile,
    #[serde(default)]
    jump_hosts: Vec<JumpHostFile>,
    #[serde(default)]
    terminal: TerminalOptionsFile,
    #[serde(default)]
    sftp: SftpOptionsFile,
}

#[derive(Debug, Deserialize)]
struct ProxyFile {
    #[serde(default = "default_proxy_protocol")]
    protocol: String,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    credential_ref: Option<String>,
}

impl Default for ProxyFile {
    /// 缺省整个代理表时也使用与显式 `protocol = "none"` 相同的无代理语义。
    fn default() -> Self {
        Self {
            protocol: default_proxy_protocol(),
            host: None,
            port: None,
            username: None,
            credential_ref: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct JumpHostFile {
    #[serde(default)]
    name: String,
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default)]
    username: String,
    #[serde(default = "default_auth_type")]
    auth_type: String,
    credential_ref: Option<String>,
    secret_ref: Option<String>,
    key_passphrase_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TerminalOptionsFile {
    #[serde(default = "default_connect_timeout_seconds")]
    connect_timeout_seconds: u16,
    #[serde(default = "default_keepalive_seconds")]
    keepalive_seconds: u16,
    term: Option<String>,
    working_directory: Option<String>,
}

impl Default for TerminalOptionsFile {
    /// 让当前主机在未配置可选 terminal 表时使用稳定的连接超时默认值。
    fn default() -> Self {
        Self {
            connect_timeout_seconds: default_connect_timeout_seconds(),
            keepalive_seconds: default_keepalive_seconds(),
            term: None,
            working_directory: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SftpOptionsFile {
    #[serde(default = "default_sftp_enabled")]
    enabled: bool,
    local_start_directory: Option<String>,
    remote_start_directory: Option<String>,
    #[serde(default = "default_preserve_mtime")]
    preserve_mtime: bool,
    #[serde(default)]
    follow_source_symlinks: bool,
    concurrency: Option<u16>,
}

impl Default for SftpOptionsFile {
    /// 缺少 SFTP 子表时保留当前能力和修改时间行为，避免历史 host 被意外禁用。
    fn default() -> Self {
        Self {
            enabled: default_sftp_enabled(),
            local_start_directory: None,
            remote_start_directory: None,
            preserve_mtime: default_preserve_mtime(),
            follow_source_symlinks: false,
            concurrency: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RdpOptionsFile {
    #[serde(default = "default_rdp_fullscreen")]
    fullscreen: bool,
    width: Option<u16>,
    height: Option<u16>,
}

impl Default for RdpOptionsFile {
    /// 缺少 RDP 子表时继续采用已存在客户端的全屏行为，尺寸由客户端决定。
    fn default() -> Self {
        Self {
            fullscreen: default_rdp_fullscreen(),
            width: None,
            height: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GroupsFile {
    schema_version: u32,
    #[serde(default)]
    groups: Vec<GroupFile>,
}

#[derive(Debug, Deserialize)]
struct GroupFile {
    id: String,
    name: String,
    sort_order: i64,
    created_at: String,
    updated_at: String,
}

impl HostConfigurationRepository {
    /// 复用应用已配置的数据根、恢复门禁、锁与 CAS 实现，避免建立第二套写入路径。
    pub fn new(store: ConfigurationStore) -> Self {
        let configuration: Arc<dyn ConfigurationRepository> = Arc::new(store.clone());
        let root = store.root().to_path_buf();
        Self {
            configuration,
            store,
            root,
        }
    }

    /// 让主机管理与 SSH 运行时共用一次 schema 解析，并用原始 TOML revision 保护并发编辑。
    ///
    /// @author kongweiguang
    fn decode_host(document: ConfigDocument, requested_id: &HostId) -> AppResult<ManagedHost> {
        let file = parse_host_file(&document.source)?;
        if file.id != requested_id.as_str() {
            return Err(corrupt_host(HostDocumentError::PathMismatch));
        }
        let protocol = parse_protocol(&file.protocol)?;
        let host_id = HostId::parse(&file.id).map_err(corrupt_host)?;
        let (authentication_mode, authentication) = decode_authentication(
            protocol,
            &file.auth_type,
            file.credential_ref.as_deref(),
            file.secret_ref.as_deref(),
            file.key_passphrase_ref.as_deref(),
            CredentialReferenceContext {
                host_id: &host_id,
                kind: "ssh-host",
                scope: "target",
            },
        )?;
        let rdp_password = if protocol == RemoteProtocol::Rdp {
            file.secret_ref
                .as_deref()
                .map(|reference| {
                    decode_credential(
                        reference,
                        &host_id,
                        "rdp-host",
                        "target",
                        CredentialMaterial::Password,
                    )
                })
                .transpose()?
        } else {
            None
        };
        let route = decode_route(&file.ssh_options, &host_id)?;
        if file.tags.iter().any(|tag| is_reserved_serial_host_tag(tag)) {
            return Err(corrupt_host(HostDocumentError::InvalidSerialSettings));
        }
        let (endpoint, serial) = decode_endpoint(protocol, &file)?;
        let tags = file
            .tags
            .iter()
            .filter(|tag| !tag.eq_ignore_ascii_case(FAVORITE_TAG))
            .cloned()
            .collect::<Vec<_>>();
        let is_favorite = file
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case(FAVORITE_TAG));

        Ok(ManagedHost {
            id: host_id,
            group_id: file.group_id,
            name: file.name,
            note: normalize_host_note(file.note),
            protocol,
            endpoint,
            authentication,
            authentication_mode,
            rdp_password,
            route,
            serial,
            tags,
            is_favorite,
            sort_order: file.sort_order,
            connect_timeout_ms: u64::from(file.ssh_options.terminal.connect_timeout_seconds) * 1000,
            keepalive_interval_ms: (file.ssh_options.terminal.keepalive_seconds != 0)
                .then_some(u64::from(file.ssh_options.terminal.keepalive_seconds) * 1000),
            terminal_options: HostTerminalOptions {
                term: file.ssh_options.terminal.term.clone(),
                initial_working_directory: file.ssh_options.terminal.working_directory.clone(),
            },
            sftp_options: HostSftpOptions {
                enabled: file.ssh_options.sftp.enabled,
                local_start_directory: file
                    .ssh_options
                    .sftp
                    .local_start_directory
                    .as_deref()
                    .map(PathBuf::from),
                remote_start_directory: file.ssh_options.sftp.remote_start_directory.clone(),
                preserve_mtime: file.ssh_options.sftp.preserve_mtime,
                follow_source_symlinks: file.ssh_options.sftp.follow_source_symlinks,
                concurrency: file.ssh_options.sftp.concurrency,
            },
            rdp_display: RdpDisplayOptions {
                fullscreen: file.rdp_options.fullscreen,
                width: file.rdp_options.width,
                height: file.rdp_options.height,
            },
            created_at: file.created_at,
            updated_at: file.updated_at,
            revision: document.revision,
        })
    }

    /// 仅枚举文件名，内容必须经 ConfigurationRepository 与恢复门禁读取。
    async fn host_ids(&self) -> AppResult<Vec<HostId>> {
        let directory = self.root.join("hosts");
        let ids = task::spawn_blocking(move || enumerate_host_ids(&directory))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("主机目录扫描任务意外终止", source)
            })??;
        Ok(ids)
    }

    /// 编辑 TOML 前读取现有文档，并验证调用方提交的精确 CAS revision。
    ///
    /// @author kongweiguang
    async fn editable_document(
        &self,
        id: &HostId,
        expected_revision: Option<&ConfigRevision>,
    ) -> AppResult<(DocumentMut, Option<ConfigDocument>)> {
        let path = ConfigDocumentPath::Host(id.as_str().to_owned());
        let existing = self.configuration.read(path).await?;
        match (expected_revision, existing.as_ref()) {
            (None, None) => Ok((DocumentMut::new(), None)),
            (Some(expected), Some(document)) if expected == &document.revision => {
                let parsed = document
                    .source
                    .parse::<DocumentMut>()
                    .map_err(corrupt_host)?;
                Ok((parsed, existing))
            }
            (None, Some(_)) | (Some(_), None) | (Some(_), Some(_)) => Err(AppError::conflict(
                "主机配置已被其它操作修改，请重新读取后保存",
            )),
        }
    }
}

#[async_trait]
impl HostManagementRepository for HostConfigurationRepository {
    /// 复用共享配置仓储当前恢复态，在 vault 写入前拒绝只读恢复中的主机变更。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        if self.configuration.recovery_state().mode == ConfigurationMode::ReadOnlyRecovery {
            return Err(AppError::configuration_corrupt("主机配置需要先恢复"));
        }
        Ok(())
    }

    /// 完整返回所有普通 schema v3 主机；任一条目损坏都整体失败，避免误报为缺失。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        let mut hosts = Vec::new();
        for id in self.host_ids().await? {
            let path = ConfigDocumentPath::Host(id.as_str().to_owned());
            let document =
                self.configuration.read(path).await?.ok_or_else(|| {
                    AppError::conflict("主机目录在读取期间发生变化，请刷新后重试")
                })?;
            hosts.push(Self::decode_host(document, &id)?);
        }
        Ok(hosts)
    }

    /// 按稳定 ID 读取主机，文件路径推导留在基础设施边界。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        let document = self
            .configuration
            .read(ConfigDocumentPath::Host(host_id.as_str().to_owned()))
            .await?;
        document
            .map(|document| Self::decode_host(document, host_id))
            .transpose()
    }

    /// 仅更新主机编辑器负责的字段，并保留未知 TOML 字段和注释。
    ///
    /// @author kongweiguang
    async fn save_host(
        &self,
        host_id: HostId,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        let (mut document, existing) = self
            .editable_document(&host_id, expected_revision.as_ref())
            .await?;
        let now = timestamp_now();
        if existing.is_none() {
            set_integer(
                document.as_table_mut(),
                "schema_version",
                i64::from(HOST_SCHEMA_VERSION),
            );
            set_string(document.as_table_mut(), "created_at", &now);
            let ssh_options = ensure_table(document.as_table_mut(), "ssh_options")?;
            let terminal = ensure_table(ssh_options, "terminal")?;
            set_value(terminal, "connect_timeout_seconds", Value::from(30));
            set_value(terminal, "keepalive_seconds", Value::from(60));
        }
        apply_host_draft(&mut document, &host_id, draft, &now)?;

        let path = ConfigDocumentPath::Host(host_id.as_str().to_owned());
        let revision = self
            .configuration
            .write(path.clone(), document.to_string(), expected_revision)
            .await?;
        let saved = self
            .configuration
            .read(path)
            .await?
            .ok_or_else(|| AppError::infrastructure("保存后的主机配置无法重新读取"))?;
        if saved.revision != revision {
            return Err(AppError::conflict("主机配置在保存后再次变化，请重新读取"));
        }
        Self::decode_host(saved, &host_id)
    }

    /// 在配置存储的同步事务线程中按精确 revision 删除主机，回传删除前已校验的领域快照。
    async fn delete_host(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        self.ensure_host_writable().await?;
        let host = self
            .load_host(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("连接主机不存在"))?;
        if &host.revision != expected_revision {
            return Err(AppError::conflict("主机配置已被其它操作修改，请刷新后删除"));
        }

        let store = self.store.clone();
        let id = host_id.clone();
        let revision = expected_revision.clone();
        let removed = task::spawn_blocking(move || store.delete_host_document(&id, &revision))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("主机删除事务任务意外终止", source)
            })??;
        if !removed {
            return Err(AppError::not_found("连接主机已被其它操作删除"));
        }
        Ok(host)
    }

    /// 读取完整分组文档；文件缺失代表空目录，不为读取而合成写入。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        let Some(document) = self
            .configuration
            .read(ConfigDocumentPath::HostGroups)
            .await?
        else {
            return Ok(HostGroupSnapshot::default());
        };
        let groups = parse_groups(&document.source)?;
        Ok(HostGroupSnapshot {
            groups,
            revision: Some(document.revision),
        })
    }

    /// 以整文件 CAS 新建或重命名分组，同时保留其他分组条目。
    ///
    /// @author kongweiguang
    async fn save_group(
        &self,
        draft: HostGroupDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        let existing = self
            .configuration
            .read(ConfigDocumentPath::HostGroups)
            .await?;
        let mut document = match (expected_revision.as_ref(), existing.as_ref()) {
            (None, None) => DocumentMut::new(),
            (Some(expected), Some(current)) if expected == &current.revision => current
                .source
                .parse::<DocumentMut>()
                .map_err(corrupt_groups)?,
            _ => return Err(AppError::conflict("主机分组已被其它操作修改，请刷新后保存")),
        };
        let id = draft
            .id
            .ok_or_else(|| AppError::invalid("缺少主机分组标识"))?;
        let name = draft.name.trim();
        if name.is_empty() || name.chars().any(char::is_control) || name.chars().count() > 64 {
            return Err(AppError::invalid("分组名称必须为 1 到 64 个可见字符"));
        }
        if existing.is_none() {
            set_integer(
                document.as_table_mut(),
                "schema_version",
                i64::from(GROUP_SCHEMA_VERSION),
            );
        }
        let groups_item = document
            .as_table_mut()
            .entry("groups")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
        if groups_item.as_array_of_tables().is_none() {
            return Err(corrupt_groups(HostDocumentError::InvalidGroups));
        }
        let groups = groups_item
            .as_array_of_tables_mut()
            .ok_or_else(|| corrupt_groups(HostDocumentError::InvalidGroups))?;
        let duplicate_name = groups.iter().any(|group| {
            group.get("id").and_then(Item::as_str) != Some(id.as_str())
                && group
                    .get("name")
                    .and_then(Item::as_str)
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        });
        if duplicate_name {
            return Err(AppError::conflict("主机分组名称已存在"));
        }
        let index = groups
            .iter()
            .position(|group| group.get("id").and_then(Item::as_str) == Some(id.as_str()));
        let is_new = index.is_none();
        if is_new {
            groups.push(Table::new());
        }
        let index = index.unwrap_or(groups.len() - 1);
        let group = groups
            .get_mut(index)
            .ok_or_else(|| corrupt_groups(HostDocumentError::InvalidGroups))?;
        let created_at = group
            .get("created_at")
            .and_then(Item::as_str)
            .map(str::to_owned)
            .unwrap_or_else(timestamp_now);
        set_string(group, "id", &id);
        set_string(group, "name", name);
        set_integer(group, "sort_order", draft.sort_order);
        set_string(group, "created_at", &created_at);
        set_string(group, "updated_at", &timestamp_now());
        let revision = self
            .configuration
            .write(
                ConfigDocumentPath::HostGroups,
                document.to_string(),
                expected_revision,
            )
            .await?;
        self.load_groups().await.map(|mut snapshot| {
            snapshot.revision = Some(revision);
            snapshot
        })
    }

    /// 组文件和所有 host.group_id 由同一存储 journal 提交，避免多文件迁移中出现悬空引用。
    async fn delete_group_and_reassign(
        &self,
        group_id: String,
        destination_group_id: Option<String>,
        expected_group_revision: ConfigRevision,
        expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        self.ensure_host_writable().await?;
        let store = self.store.clone();
        let result = task::spawn_blocking(move || {
            store.delete_host_group_and_reassign(
                group_id,
                destination_group_id,
                expected_group_revision,
                expected_hosts,
            )
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("分组迁移事务任务意外终止", source)
        })??;
        let snapshot = HostGroupSnapshot {
            groups: parse_groups(&result.groups.source)?,
            revision: Some(result.groups.revision),
        };
        Ok(HostGroupDeletionReceipt {
            groups: snapshot,
            reassigned_hosts: result.reassigned_hosts,
        })
    }
}

#[async_trait]
impl HostConnectionRepository for HostConfigurationRepository {
    /// SSH 与 RDP 运行时读取器复用 UI 的 schema 解析和 vault 引用映射。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        let managed = self
            .load_host(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("连接主机不存在"))?;
        Ok(HostConnectionProfile {
            host_id: managed.id,
            protocol: managed.protocol,
            endpoint: managed.endpoint,
            authentication: managed.authentication,
            rdp_password: managed.rdp_password,
            route: SshRoute {
                proxy: managed.route.proxy,
                jump_hosts: managed
                    .route
                    .jump_hosts
                    .into_iter()
                    .map(|jump| {
                        let authentication = jump.authentication.ok_or_else(|| {
                            AppError::invalid("跳板认证配置不完整，请编辑主机并补齐凭据")
                        })?;
                        Ok(kerminal_domain::connections::SshJumpHost {
                            endpoint: jump.endpoint,
                            authentication,
                        })
                    })
                    .collect::<AppResult<Vec<_>>>()?,
            },
            shell_dialect: RemoteShellDialect::Unknown,
            connect_timeout_ms: managed.connect_timeout_ms,
            keepalive_interval_ms: managed.keepalive_interval_ms,
        })
    }
}

#[async_trait]
impl SavedExternalTargetLookup for HostConfigurationRepository {
    /// 仅按规范化 endpoint 与完整有序跳板链分类外部目标，不读取保存主机的凭据。
    async fn find_exact(&self, target: &ExternalTargetIdentity) -> AppResult<Option<HostId>> {
        let hosts = <Self as HostManagementRepository>::list_hosts(self).await?;
        Ok(hosts.into_iter().find_map(|host| {
            let endpoint_matches = same_endpoint(&host.endpoint, &target.endpoint);
            let jump_chain_matches = host.route.jump_hosts.len() == target.jump_hosts.len()
                && host
                    .route
                    .jump_hosts
                    .iter()
                    .zip(&target.jump_hosts)
                    .all(|(saved, requested)| same_endpoint(&saved.endpoint, requested));
            (host.protocol == target.protocol && endpoint_matches && jump_chain_matches)
                .then_some(host.id)
        }))
    }
}

/// 比较主机名按旧外部路由规则归一化后的完整 endpoint，用户名保持大小写敏感。
fn same_endpoint(saved: &HostEndpoint, requested: &HostEndpoint) -> bool {
    canonical_host(&saved.host) == canonical_host(&requested.host)
        && saved.port == requested.port
        && saved.username.trim() == requested.username.trim()
}

/// 只去除外层 IPv6 方括号、尾随 DNS 点并折叠 ASCII 大小写，不解析 DNS 或改写 IP。
fn canonical_host(host: &str) -> String {
    let trimmed = host.trim();
    let unwrapped = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);
    unwrapped.trim_end_matches('.').to_ascii_lowercase()
}

/// 枚举有效的普通主机文件，并排除分组文件、符号链接与不安全文件名。
fn enumerate_host_ids(directory: &Path) -> AppResult<Vec<HostId>> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取主机目录失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::conflict("主机目录不是安全的普通目录"));
    }
    let entries = fs::read_dir(directory)
        .map_err(|source| AppError::infrastructure_with_source("读取主机文件列表失败", source))?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            AppError::infrastructure_with_source("读取主机文件列表失败", source)
        })?;
        let file_type = entry
            .file_type()
            .map_err(|source| AppError::infrastructure_with_source("检查主机文件失败", source))?;
        if !file_type.is_file() || entry.path().extension() != Some(OsStr::new("toml")) {
            continue;
        }
        let path = entry.path();
        let Some(id) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        if ConfigDocumentPath::Host(id.to_owned())
            .relative_path()
            .is_err()
        {
            continue;
        }
        ids.push(HostId::parse(id).map_err(corrupt_host)?);
    }
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(ids)
}

/// 在类型解码忽略扩展字段前，递归拒绝任意层级内嵌的秘密材料。
fn parse_host_file(source: &str) -> AppResult<HostFile> {
    let value = toml::from_str::<toml::Value>(source).map_err(corrupt_host)?;
    if contains_inline_secret(&value) {
        return Err(corrupt_host(HostDocumentError::InlineSecret));
    }
    let file = toml::from_str::<HostFile>(source).map_err(corrupt_host)?;
    if file.schema_version != HOST_SCHEMA_VERSION {
        return Err(corrupt_host(HostDocumentError::UnsupportedSchema));
    }
    if file.protocol.trim().is_empty() {
        return Err(corrupt_host(HostDocumentError::MissingProtocol));
    }
    Ok(file)
}

/// 解码完整 schema v1 分组文档，不静默丢弃或修复损坏条目。
fn parse_groups(source: &str) -> AppResult<Vec<HostGroup>> {
    let file = toml::from_str::<GroupsFile>(source).map_err(corrupt_groups)?;
    if file.schema_version != GROUP_SCHEMA_VERSION {
        return Err(corrupt_groups(HostDocumentError::UnsupportedSchema));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut groups = Vec::with_capacity(file.groups.len());
    for group in file.groups {
        if group.id.is_empty() || !is_safe_group_id(&group.id) || !ids.insert(group.id.clone()) {
            return Err(corrupt_groups(HostDocumentError::InvalidGroups));
        }
        groups.push(HostGroup {
            id: group.id,
            name: group.name,
            sort_order: group.sort_order,
            created_at: group.created_at,
            updated_at: group.updated_at,
        });
    }
    groups.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(groups)
}

/// 将唯一解析出的 schema 模型转换为运行时 profile，不维护第二套主机加载器。
///
/// @author kongweiguang
fn decode_route(
    options: &SshOptionsFile,
    host_id: &HostId,
) -> AppResult<kerminal_application::connections::management::HostRouteSettings> {
    let proxy = match options.proxy.protocol.as_str() {
        "none" => None,
        "http" => Some(ProxyProtocol::HttpConnect),
        "socks5" => Some(ProxyProtocol::Socks5),
        _ => return Err(corrupt_host(HostDocumentError::InvalidProxy)),
    };
    let proxy = proxy
        .map(|protocol| {
            let host = options.proxy.host.as_deref().unwrap_or_default();
            let port = options.proxy.port.unwrap_or_default();
            if host.trim().is_empty() || port == 0 {
                return Err(corrupt_host(HostDocumentError::InvalidProxy));
            }
            Ok(ProxyProfile {
                protocol,
                endpoint: HostEndpoint {
                    host: host.to_owned(),
                    port,
                    username: options.proxy.username.clone().unwrap_or_default(),
                },
                password: options
                    .proxy
                    .credential_ref
                    .as_deref()
                    .map(CredentialRef::new)
                    .transpose()
                    .map_err(|_| corrupt_host(HostDocumentError::InvalidCredentialReference))?,
            })
        })
        .transpose()?;
    let jump_hosts = options
        .jump_hosts
        .iter()
        .enumerate()
        .map(|(index, jump)| {
            let scope = format!("jump-{index}");
            let (authentication_mode, authentication) = decode_authentication(
                RemoteProtocol::Ssh,
                &jump.auth_type,
                jump.credential_ref.as_deref(),
                jump.secret_ref.as_deref(),
                jump.key_passphrase_ref.as_deref(),
                CredentialReferenceContext {
                    host_id,
                    kind: "jump-host",
                    scope: &scope,
                },
            )?;
            Ok(ManagedJumpHost {
                name: if jump.name.is_empty() {
                    format!("Jump {}", index + 1)
                } else {
                    jump.name.clone()
                },
                endpoint: HostEndpoint {
                    host: jump.host.clone(),
                    port: jump.port,
                    username: jump.username.clone(),
                },
                authentication,
                authentication_mode,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(kerminal_application::connections::management::HostRouteSettings { proxy, jump_hosts })
}

/// 凭据归属上下文将 host、kind 与 scope 作为不可拆散的一组解码条件。
///
/// @author kongweiguang
struct CredentialReferenceContext<'a> {
    host_id: &'a HostId,
    kind: &'a str,
    scope: &'a str,
}

/// 只解释历史认证元数据；缺少 vault entry 时保留可见的未配置凭据状态。
///
/// @author kongweiguang
fn decode_authentication(
    protocol: RemoteProtocol,
    auth_type: &str,
    key_path: Option<&str>,
    secret_ref: Option<&str>,
    passphrase_ref: Option<&str>,
    reference_context: CredentialReferenceContext<'_>,
) -> AppResult<(HostAuthenticationMode, Option<SshAuthentication>)> {
    let CredentialReferenceContext {
        host_id,
        kind: credential_kind,
        scope,
    } = reference_context;
    let mode = match auth_type {
        "agent" => HostAuthenticationMode::Agent,
        "password" => HostAuthenticationMode::Password,
        "key" => HostAuthenticationMode::PrivateKey,
        _ => return Err(corrupt_host(HostDocumentError::InvalidAuthentication)),
    };
    if !matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        if protocol == RemoteProtocol::Rdp && mode != HostAuthenticationMode::PrivateKey {
            return Ok((mode, None));
        }
        if matches!(protocol, RemoteProtocol::Telnet | RemoteProtocol::Serial)
            && mode == HostAuthenticationMode::Agent
        {
            return Ok((mode, None));
        }
        if protocol == RemoteProtocol::Rdp {
            return Err(corrupt_host(HostDocumentError::InvalidAuthentication));
        }
        return Ok((mode, None));
    }
    let authentication = match mode {
        HostAuthenticationMode::Agent => Some(SshAuthentication::Agent),
        HostAuthenticationMode::Password => secret_ref
            .map(|reference| {
                decode_credential(
                    reference,
                    host_id,
                    credential_kind,
                    scope,
                    CredentialMaterial::Password,
                )
            })
            .transpose()?
            .map(|secret| SshAuthentication::Password { secret }),
        HostAuthenticationMode::PrivateKey => {
            if key_path.is_some_and(|path| path.starts_with("credential:")) {
                return Err(corrupt_host(HostDocumentError::InvalidCredentialReference));
            }
            let secret = secret_ref
                .map(|reference| {
                    decode_credential(
                        reference,
                        host_id,
                        credential_kind,
                        scope,
                        CredentialMaterial::PrivateKey,
                    )
                })
                .transpose()?;
            let passphrase = passphrase_ref
                .map(|reference| {
                    decode_credential(
                        reference,
                        host_id,
                        credential_kind,
                        scope,
                        CredentialMaterial::KeyPassphrase,
                    )
                })
                .transpose()?;
            Some(SshAuthentication::PrivateKey {
                path: key_path.map(PathBuf::from),
                secret,
                passphrase,
            })
        }
    };
    Ok((mode, authentication))
}

/// 在运行时适配器接收凭据前校验 vault 引用的归属和材料类型。
fn decode_credential(
    value: &str,
    host_id: &HostId,
    kind: &str,
    scope: &str,
    material: CredentialMaterial,
) -> AppResult<CredentialRef> {
    let reference = CredentialRef::new(value)
        .map_err(|_| corrupt_host(HostDocumentError::InvalidCredentialReference))?;
    let parsed = CredentialReference::parse(reference.clone()).map_err(corrupt_host)?;
    if parsed.host_id() != host_id
        || parsed.kind().as_str() != kind
        || !scope_matches(scope, parsed.scope().as_str())
        || parsed.material() != material
    {
        return Err(corrupt_host(HostDocumentError::InvalidCredentialReference));
    }
    Ok(reference)
}

/// 从当前 schema 的专用字段投影 endpoint；串口设备只保存一次并由 serial.device 派生。
fn decode_endpoint(
    protocol: RemoteProtocol,
    file: &HostFile,
) -> AppResult<(HostEndpoint, Option<SerialSessionTarget>)> {
    if protocol == RemoteProtocol::Serial {
        if file.host.is_some() || file.port.is_some() || file.username.is_some() {
            return Err(corrupt_host(HostDocumentError::InvalidSerialSettings));
        }
        let serial = file
            .serial
            .as_ref()
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidSerialSettings))?;
        let target = decode_serial(serial)?;
        let host = target
            .device
            .to_str()
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidSerialSettings))?
            .to_owned();
        return Ok((
            HostEndpoint {
                host,
                port: 0,
                username: String::new(),
            },
            Some(target),
        ));
    }

    if file.serial.is_some() {
        return Err(corrupt_host(HostDocumentError::InvalidSerialSettings));
    }
    let endpoint = HostEndpoint {
        host: file
            .host
            .clone()
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidEndpoint))?,
        port: file
            .port
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidEndpoint))?,
        username: file
            .username
            .clone()
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidEndpoint))?,
    };
    Ok((endpoint, None))
}

/// 解码 schema v3 的规范串口字段，不在普通读取路径猜测别名或补旧字段。
fn decode_serial(file: &SerialOptionsFile) -> AppResult<SerialSessionTarget> {
    let device = file.device.trim();
    if device.is_empty()
        || device != file.device
        || device.chars().any(char::is_control)
        || !(300..=4_000_000).contains(&file.baud_rate)
    {
        return Err(corrupt_host(HostDocumentError::InvalidSerialSettings));
    }
    let data_bits = match file.data_bits {
        5 => SerialDataBits::Five,
        6 => SerialDataBits::Six,
        7 => SerialDataBits::Seven,
        8 => SerialDataBits::Eight,
        _ => return Err(corrupt_host(HostDocumentError::InvalidSerialSettings)),
    };
    let stop_bits = match file.stop_bits {
        1 => SerialStopBits::One,
        2 => SerialStopBits::Two,
        _ => return Err(corrupt_host(HostDocumentError::InvalidSerialSettings)),
    };
    let parity = match file.parity.as_str() {
        "none" => SerialParity::None,
        "odd" => SerialParity::Odd,
        "even" => SerialParity::Even,
        _ => return Err(corrupt_host(HostDocumentError::InvalidSerialSettings)),
    };
    let flow_control = match file.flow_control.as_str() {
        "none" => SerialFlowControl::None,
        "xonxoff" => SerialFlowControl::Software,
        "rtscts" => SerialFlowControl::Hardware,
        _ => return Err(corrupt_host(HostDocumentError::InvalidSerialSettings)),
    };
    Ok(SerialSessionTarget {
        device: PathBuf::from(device),
        baud_rate: file.baud_rate,
        data_bits,
        stop_bits,
        parity,
        flow_control,
    })
}

/// 把类型化主机草稿写回当前 schema；备注只占一个根字段，空白值不产生冗余 TOML。
///
/// @author kongweiguang
fn apply_host_draft(
    document: &mut DocumentMut,
    host_id: &HostId,
    draft: HostDraft,
    now: &str,
) -> AppResult<()> {
    if draft
        .note
        .as_deref()
        .is_some_and(|note| !host_note_is_valid(note))
    {
        return Err(AppError::invalid(
            "主机备注不能超过 4096 个字符或包含不支持的控制字符",
        ));
    }
    let root = document.as_table_mut();
    set_string(root, "protocol", protocol_name(draft.protocol));
    set_string(root, "id", host_id.as_str());
    set_optional_string(root, "group_id", draft.group_id.as_deref());
    set_string(root, "name", draft.name.trim());
    set_optional_string(
        root,
        "note",
        draft.note.as_deref().filter(|note| !note.trim().is_empty()),
    );
    if draft.protocol == RemoteProtocol::Serial {
        let serial = draft
            .serial
            .as_ref()
            .ok_or_else(|| AppError::invalid("串口连接缺少串口参数"))?;
        validate_serial_target(serial)?;
        root.remove("host");
        root.remove("port");
        root.remove("username");
        write_serial_options(root, serial)?;
    } else {
        if draft.serial.is_some() {
            return Err(AppError::invalid("非串口连接不能包含串口参数"));
        }
        root.remove("serial");
        set_string(root, "host", draft.endpoint.host.trim());
        set_integer(root, "port", i64::from(draft.endpoint.port));
        set_string(root, "username", draft.endpoint.username.trim());
    }
    set_integer(root, "sort_order", draft.sort_order);
    set_string(root, "updated_at", now);
    if !root.contains_key("created_at") {
        set_string(root, "created_at", now);
    }
    write_host_options(root, &draft)?;
    if draft
        .tags
        .iter()
        .any(|tag| is_reserved_serial_host_tag(tag))
    {
        return Err(AppError::invalid(
            "串口参数必须保存到 serial 表，不能写入 tags",
        ));
    }
    let mut tags = draft
        .tags
        .into_iter()
        .filter(|tag| !tag.eq_ignore_ascii_case(FAVORITE_TAG))
        .collect::<Vec<_>>();
    if draft.is_favorite {
        tags.push(FAVORITE_TAG.to_owned());
    }
    set_string_array(root, "tags", &tags);
    apply_authentication(
        root,
        host_id,
        draft.protocol,
        draft.authentication_mode,
        &draft.authentication,
        draft.rdp_password.as_ref(),
    )?;
    apply_route(document, &draft.route, host_id)?;
    Ok(())
}

/// 只更新编辑器拥有的选项键；保留第三方扩展，且秒数必须无损表达，避免保存后悄悄取整。
///
/// @author kongweiguang
fn write_host_options(root: &mut Table, draft: &HostDraft) -> AppResult<()> {
    if !draft.connect_timeout_ms.is_multiple_of(1000)
        || draft
            .keepalive_interval_ms
            .is_some_and(|value| !value.is_multiple_of(1000))
    {
        return Err(AppError::invalid("主机连接超时与心跳必须使用整秒"));
    }
    if draft
        .sftp_options
        .local_start_directory
        .as_ref()
        .is_some_and(|value| value.to_str().is_none())
    {
        return Err(AppError::invalid("SFTP 本地起始目录必须能编码为 UTF-8"));
    }
    let ssh = ensure_table(root, "ssh_options")?;
    let terminal = ensure_table(ssh, "terminal")?;
    set_integer(
        terminal,
        "connect_timeout_seconds",
        (draft.connect_timeout_ms / 1000) as i64,
    );
    set_integer(
        terminal,
        "keepalive_seconds",
        (draft.keepalive_interval_ms.unwrap_or(0) / 1000) as i64,
    );
    set_optional_string(terminal, "term", draft.terminal_options.term.as_deref());
    set_optional_string(
        terminal,
        "working_directory",
        draft.terminal_options.initial_working_directory.as_deref(),
    );
    let sftp = ensure_table(ssh, "sftp")?;
    set_value(sftp, "enabled", Value::from(draft.sftp_options.enabled));
    set_optional_string(
        sftp,
        "local_start_directory",
        draft
            .sftp_options
            .local_start_directory
            .as_ref()
            .and_then(|value| value.to_str()),
    );
    set_optional_string(
        sftp,
        "remote_start_directory",
        draft.sftp_options.remote_start_directory.as_deref(),
    );
    set_value(
        sftp,
        "preserve_mtime",
        Value::from(draft.sftp_options.preserve_mtime),
    );
    set_value(
        sftp,
        "follow_source_symlinks",
        Value::from(draft.sftp_options.follow_source_symlinks),
    );
    if let Some(concurrency) = draft.sftp_options.concurrency {
        set_integer(sftp, "concurrency", i64::from(concurrency));
    } else {
        sftp.remove("concurrency");
    }
    let rdp = ensure_table(root, "rdp_options")?;
    set_value(rdp, "fullscreen", Value::from(draft.rdp_display.fullscreen));
    for (key, value) in [
        ("width", draft.rdp_display.width),
        ("height", draft.rdp_display.height),
    ] {
        if let Some(value) = value {
            set_integer(rdp, key, i64::from(value));
        } else {
            rdp.remove(key);
        }
    }
    Ok(())
}

/// 将已校验的领域串口设置写入唯一持久化位置，并保留该表中的未知扩展字段。
fn write_serial_options(root: &mut Table, serial: &SerialSessionTarget) -> AppResult<()> {
    let device = serial
        .device
        .to_str()
        .ok_or_else(|| AppError::invalid("串口设备路径必须能编码为 UTF-8"))?;
    let table = ensure_table(root, "serial")?;
    set_string(table, "device", device);
    set_integer(table, "baud_rate", i64::from(serial.baud_rate));
    set_integer(
        table,
        "data_bits",
        i64::from(serial_data_bits(serial.data_bits)),
    );
    set_integer(
        table,
        "stop_bits",
        i64::from(serial_stop_bits(serial.stop_bits)),
    );
    set_string(table, "parity", serial_parity(serial.parity));
    set_string(
        table,
        "flow_control",
        serial_flow_control(serial.flow_control),
    );
    Ok(())
}

/// 拒绝当前格式无法无损表达的路径或波特率，避免写入后回读为另一条串口配置。
fn validate_serial_target(serial: &SerialSessionTarget) -> AppResult<()> {
    let device = serial
        .device
        .to_str()
        .ok_or_else(|| AppError::invalid("串口设备路径必须能编码为 UTF-8"))?;
    if device.trim().is_empty()
        || device.trim() != device
        || device.chars().any(char::is_control)
        || !(300..=4_000_000).contains(&serial.baud_rate)
    {
        return Err(AppError::invalid("串口设备路径或波特率无效"));
    }
    Ok(())
}

/// 将领域数据位枚举映射为配置文档使用的整数值。
fn serial_data_bits(value: SerialDataBits) -> u8 {
    match value {
        SerialDataBits::Five => 5,
        SerialDataBits::Six => 6,
        SerialDataBits::Seven => 7,
        SerialDataBits::Eight => 8,
    }
}

/// 将领域停止位枚举映射为配置文档使用的整数值。
fn serial_stop_bits(value: SerialStopBits) -> u8 {
    match value {
        SerialStopBits::One => 1,
        SerialStopBits::Two => 2,
    }
}

/// 将领域校验枚举编码为唯一的当前文档值。
fn serial_parity(value: SerialParity) -> &'static str {
    match value {
        SerialParity::None => "none",
        SerialParity::Even => "even",
        SerialParity::Odd => "odd",
    }
}

/// 将领域流控枚举编码为唯一的当前文档值。
fn serial_flow_control(value: SerialFlowControl) -> &'static str {
    match value {
        SerialFlowControl::None => "none",
        SerialFlowControl::Software => "xonxoff",
        SerialFlowControl::Hardware => "rtscts",
    }
}

/// 修改代理与跳板编辑字段时保留 SSH options 中编辑器未管理的子表。
fn apply_route(
    document: &mut DocumentMut,
    route: &kerminal_application::connections::management::HostRouteSettings,
    host_id: &HostId,
) -> AppResult<()> {
    let ssh_options = ensure_table(document.as_table_mut(), "ssh_options")?;
    let proxy = ensure_table(ssh_options, "proxy")?;
    match route.proxy.as_ref() {
        None => {
            set_string(proxy, "protocol", "none");
            for field in ["host", "port", "username", "credential_ref"] {
                proxy.remove(field);
            }
        }
        Some(profile) => {
            set_string(
                proxy,
                "protocol",
                match profile.protocol {
                    ProxyProtocol::HttpConnect => "http",
                    ProxyProtocol::Socks5 => "socks5",
                },
            );
            set_string(proxy, "host", &profile.endpoint.host);
            set_integer(proxy, "port", i64::from(profile.endpoint.port));
            set_optional_string(proxy, "username", nonempty(&profile.endpoint.username));
            set_optional_string(
                proxy,
                "credential_ref",
                profile.password.as_ref().map(CredentialRef::as_str),
            );
        }
    }

    let jumps_item = ssh_options
        .entry("jump_hosts")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if jumps_item.as_array_of_tables().is_none() {
        *jumps_item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let jumps = jumps_item
        .as_array_of_tables_mut()
        .ok_or_else(|| corrupt_host(HostDocumentError::InvalidJumpHosts))?;
    for (index, jump) in route.jump_hosts.iter().enumerate() {
        if index >= jumps.len() {
            jumps.push(Table::new());
        }
        let table = jumps
            .get_mut(index)
            .ok_or_else(|| corrupt_host(HostDocumentError::InvalidJumpHosts))?;
        set_string(table, "name", &jump.name);
        set_string(table, "host", &jump.endpoint.host);
        set_integer(table, "port", i64::from(jump.endpoint.port));
        set_string(table, "username", &jump.endpoint.username);
        if let Some(authentication) = &jump.authentication {
            validate_auth_reference(
                authentication,
                host_id,
                "jump-host",
                &format!("jump-{index}"),
            )?;
        }
        write_auth_fields(
            table,
            jump.authentication_mode,
            jump.authentication.as_ref(),
        )?;
    }
    while jumps.len() > route.jump_hosts.len() {
        jumps.remove(jumps.len() - 1);
    }
    Ok(())
}

/// 只序列化不透明引用和密钥路径，秘密材料始终留在 CredentialService 的 vault 边界内。
fn apply_authentication(
    root: &mut Table,
    host_id: &HostId,
    protocol: RemoteProtocol,
    authentication_mode: HostAuthenticationMode,
    authentication: &Option<SshAuthentication>,
    rdp_password: Option<&CredentialRef>,
) -> AppResult<()> {
    if protocol == RemoteProtocol::Rdp {
        if authentication_mode == HostAuthenticationMode::PrivateKey {
            return Err(AppError::invalid("RDP 不支持私钥认证"));
        }
        set_string(
            root,
            "auth_type",
            if authentication_mode == HostAuthenticationMode::Agent {
                "agent"
            } else {
                "password"
            },
        );
        if let Some(reference) = rdp_password {
            validate_credential_reference(
                reference,
                host_id,
                "rdp-host",
                "target",
                CredentialMaterial::Password,
            )?;
        }
        set_optional_string(root, "secret_ref", rdp_password.map(CredentialRef::as_str));
        root.remove("credential_ref");
        root.remove("key_passphrase_ref");
        return Ok(());
    }
    if !matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        if protocol == RemoteProtocol::Rdp {
            return Err(AppError::invalid("RDP 认证字段无效"));
        }
        set_string(root, "auth_type", "agent");
        for field in ["credential_ref", "secret_ref", "key_passphrase_ref"] {
            root.remove(field);
        }
        return Ok(());
    }
    if let Some(authentication) = authentication.as_ref() {
        validate_auth_reference(authentication, host_id, "ssh-host", "target")?;
        if !authentication_matches_mode(authentication_mode, authentication) {
            return Err(AppError::invalid("SSH 认证模式与凭据字段不匹配"));
        }
    }
    write_auth_fields(root, authentication_mode, authentication.as_ref())?;
    Ok(())
}

/// 将三种历史 SSH 认证模式映射到路径或引用字段，不写入秘密正文。
fn write_auth_fields(
    table: &mut Table,
    mode: HostAuthenticationMode,
    authentication: Option<&SshAuthentication>,
) -> AppResult<()> {
    match (mode, authentication) {
        (HostAuthenticationMode::Agent, None | Some(SshAuthentication::Agent)) => {
            set_string(table, "auth_type", "agent");
            for field in ["credential_ref", "secret_ref", "key_passphrase_ref"] {
                table.remove(field);
            }
        }
        (HostAuthenticationMode::Password, None) => {
            set_string(table, "auth_type", "password");
            table.remove("credential_ref");
            table.remove("secret_ref");
            table.remove("key_passphrase_ref");
        }
        (HostAuthenticationMode::Password, Some(SshAuthentication::Password { secret })) => {
            set_string(table, "auth_type", "password");
            table.remove("credential_ref");
            set_string(table, "secret_ref", secret.as_str());
            table.remove("key_passphrase_ref");
        }
        (HostAuthenticationMode::PrivateKey, None) => {
            set_string(table, "auth_type", "key");
            for field in ["credential_ref", "secret_ref", "key_passphrase_ref"] {
                table.remove(field);
            }
        }
        (
            HostAuthenticationMode::PrivateKey,
            Some(SshAuthentication::PrivateKey {
                path,
                secret,
                passphrase,
            }),
        ) => {
            set_string(table, "auth_type", "key");
            let path = path
                .as_deref()
                .map(|path| {
                    path.to_str()
                        .ok_or_else(|| AppError::invalid("私钥路径必须能以 UTF-8 保存"))
                })
                .transpose()?;
            set_optional_string(table, "credential_ref", path);
            set_optional_string(
                table,
                "secret_ref",
                secret.as_ref().map(CredentialRef::as_str),
            );
            set_optional_string(
                table,
                "key_passphrase_ref",
                passphrase.as_ref().map(CredentialRef::as_str),
            );
        }
        _ => return Err(AppError::invalid("认证模式与凭据字段不匹配")),
    }
    Ok(())
}

/// 保存前拒绝来自其他主机、scope 或凭据类型的 vault 引用。
fn validate_auth_reference(
    authentication: &SshAuthentication,
    host_id: &HostId,
    kind: &str,
    scope: &str,
) -> AppResult<()> {
    match authentication {
        SshAuthentication::Agent => Ok(()),
        SshAuthentication::Password { secret } => validate_credential_reference(
            secret,
            host_id,
            kind,
            scope,
            CredentialMaterial::Password,
        ),
        SshAuthentication::PrivateKey {
            secret, passphrase, ..
        } => {
            if let Some(secret) = secret {
                validate_credential_reference(
                    secret,
                    host_id,
                    kind,
                    scope,
                    CredentialMaterial::PrivateKey,
                )?;
            }
            if let Some(passphrase) = passphrase {
                validate_credential_reference(
                    passphrase,
                    host_id,
                    kind,
                    scope,
                    CredentialMaterial::KeyPassphrase,
                )?;
            }
            Ok(())
        }
    }
}

/// 持久化不透明 token 前校验 vault 引用身份的全部组成部分。
fn validate_credential_reference(
    reference: &CredentialRef,
    host_id: &HostId,
    kind: &str,
    scope: &str,
    material: CredentialMaterial,
) -> AppResult<()> {
    let parsed = CredentialReference::parse(reference.clone()).map_err(corrupt_host)?;
    if parsed.host_id() != host_id
        || parsed.kind().as_str() != kind
        || !scope_matches(scope, parsed.scope().as_str())
        || parsed.material() != material
    {
        return Err(corrupt_host(HostDocumentError::InvalidCredentialReference));
    }
    Ok(())
}

/// 只接受既有精确 scope 或带唯一后缀的新 scope，继续读取历史引用并支持 CAS 安全轮换。
fn scope_matches(expected: &str, actual: &str) -> bool {
    actual == expected
        || actual
            .strip_prefix(expected)
            .is_some_and(|suffix| suffix.starts_with('-') && suffix.len() > 1)
}

/// 确保当前主机格式的 auth_type 与运行时认证枚举一致，防止 silent downgrade。
fn authentication_matches_mode(
    mode: HostAuthenticationMode,
    authentication: &SshAuthentication,
) -> bool {
    matches!(
        (mode, authentication),
        (HostAuthenticationMode::Agent, SshAuthentication::Agent)
            | (
                HostAuthenticationMode::Password,
                SshAuthentication::Password { .. }
            )
            | (
                HostAuthenticationMode::PrivateKey,
                SshAuthentication::PrivateKey { .. }
            )
    )
}

/// 递归拒绝禁止的凭据字段，避免它们作为扩展字段被保留下来。
fn contains_inline_secret(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            let normalized = key
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            matches!(
                normalized.as_str(),
                "password"
                    | "credentialsecret"
                    | "inlineprivatekey"
                    | "keypassphrase"
                    | "keypassphrasesecret"
            ) || contains_inline_secret(value)
        }),
        toml::Value::Array(values) => values.iter().any(contains_inline_secret),
        _ => false,
    }
}

/// 设置标量并保留原有 TOML 格式装饰，包括字段值上的注释。
fn set_value(table: &mut Table, key: &str, mut replacement: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        *replacement.decor_mut() = existing.decor().clone();
        *existing = replacement;
    } else {
        table.insert(key, Item::Value(replacement));
    }
}

/// 写入兼容 schema 的字符串且不破坏原值上的注释。
fn set_string(table: &mut Table, key: &str, value: &str) {
    set_value(table, key, Value::from(value));
}

/// 通过保留 TOML 格式装饰的同一路径写入整数。
fn set_integer(table: &mut Table, key: &str, value: i64) {
    set_value(table, key, Value::from(value));
}

/// 写入可选字符串；值缺失时只移除这个已知字段。
fn set_optional_string(table: &mut Table, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        set_string(table, key, value);
    } else {
        table.remove(key);
    }
}

/// 替换一个已知字符串数组，同时保留相邻主机元数据。
fn set_string_array(table: &mut Table, key: &str, values: &[String]) {
    let mut array = toml_edit::Array::new();
    for item in values {
        array.push(item.as_str());
    }
    set_value(table, key, Value::Array(array));
}

/// 确保已知 TOML 子表存在；形状冲突时显式失败，不覆盖原标量。
fn ensure_table<'a>(table: &'a mut Table, key: &str) -> AppResult<&'a mut Table> {
    let item = table
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    item.as_table_mut()
        .ok_or_else(|| corrupt_host(HostDocumentError::InvalidNestedTable))
}

/// 使用显式协议白名单解码，避免把未知未来值误判为 SSH。
fn parse_protocol(value: &str) -> AppResult<RemoteProtocol> {
    match value {
        "ssh" => Ok(RemoteProtocol::Ssh),
        "sftp" => Ok(RemoteProtocol::Sftp),
        "telnet" => Ok(RemoteProtocol::Telnet),
        "serial" => Ok(RemoteProtocol::Serial),
        "rdp" => Ok(RemoteProtocol::Rdp),
        _ => Err(corrupt_host(HostDocumentError::InvalidProtocol)),
    }
}

/// 返回当前 host 文档所需的规范协议 token。
fn protocol_name(protocol: RemoteProtocol) -> &'static str {
    match protocol {
        RemoteProtocol::Ssh => "ssh",
        RemoteProtocol::Sftp => "sftp",
        RemoteProtocol::Telnet => "telnet",
        RemoteProtocol::Serial => "serial",
        RemoteProtocol::Rdp => "rdp",
    }
}

/// 不将空的可选代理凭据或用户名持久化成误导性的已配置值。
fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

/// 生成兼容现有文件元数据的稳定毫秒时间戳字符串。
fn timestamp_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
        .to_string()
}

/// 校验主机分组文档共用的安全文件名 ID 语法。
fn is_safe_group_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// schema v3 缺少代理元数据时表示主机未配置代理。
fn default_proxy_protocol() -> String {
    "none".to_owned()
}

/// 可选端口缺失时沿用现有 SSH 跳板默认端口。
fn default_ssh_port() -> u16 {
    22
}

/// 可选跳板认证缺失时采用当前 SSH agent 默认值。
fn default_auth_type() -> String {
    "agent".to_owned()
}

/// 对缺少 terminal options 的 schema v3 文件沿用当前 SSH 超时。
fn default_connect_timeout_seconds() -> u16 {
    30
}

/// 沿用历史 keepalive 间隔，并将零解释为显式关闭。
fn default_keepalive_seconds() -> u16 {
    60
}

/// 让省略的 SFTP enabled 字段沿用应用类型的当前默认，避免配置适配器复制产品语义。
fn default_sftp_enabled() -> bool {
    HostSftpOptions::default().enabled
}

/// 让省略的 SFTP 时间保留字段与新主机默认一致，保持读取和编辑表单的行为相同。
fn default_preserve_mtime() -> bool {
    HostSftpOptions::default().preserve_mtime
}

/// 让省略的 RDP fullscreen 字段复用当前 launcher 默认，避免配置读取自行漂移。
fn default_rdp_fullscreen() -> bool {
    RdpDisplayOptions::default().fullscreen
}

/// 保持错误上下文固定且脱敏，并返回类型化的配置损坏错误。
#[derive(Debug, thiserror::Error)]
enum HostDocumentError {
    #[error("host document ID does not match its file name")]
    PathMismatch,
    #[error("unsupported host or group schema version")]
    UnsupportedSchema,
    #[error("host schema requires an explicit protocol")]
    MissingProtocol,
    #[error("host protocol is not supported")]
    InvalidProtocol,
    #[error("host authentication metadata is invalid")]
    InvalidAuthentication,
    #[error("vault credential reference is invalid or belongs to another host")]
    InvalidCredentialReference,
    #[error("inline credentials are forbidden in host configuration")]
    InlineSecret,
    #[error("serial settings are invalid")]
    InvalidSerialSettings,
    #[error("network endpoint fields are incomplete")]
    InvalidEndpoint,
    #[error("proxy settings are invalid")]
    InvalidProxy,
    #[error("jump host settings are invalid")]
    InvalidJumpHosts,
    #[error("host TOML nested table has an invalid shape")]
    InvalidNestedTable,
    #[error("host group document has invalid entries")]
    InvalidGroups,
}

/// 将脱敏的类型化来源信息附加到配置恢复错误类别。
fn corrupt_host<E>(source: E) -> AppError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AppError::configuration_corrupt("主机配置").with_source(source)
}

/// 将脱敏的类型化来源信息附加到主机分组恢复错误类别。
fn corrupt_groups<E>(source: E) -> AppError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AppError::configuration_corrupt("主机分组").with_source(source)
}
