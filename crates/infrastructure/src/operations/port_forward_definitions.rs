//! 当前版本 SSH 转发定义的原子文件仓储。
//!
//! @author kongweiguang

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    operations::{PortForwardDefinition, PortForwardDefinitionRepository},
};
use serde::{Deserialize, Serialize};

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const PORT_FORWARD_DEFINITIONS_FILE_NAME: &str = "definitions.json";
const PORT_FORWARD_DEFINITIONS_SCHEMA_VERSION: u32 = 1;
const MAX_PORT_FORWARD_DEFINITIONS_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PORT_FORWARD_DEFINITIONS: usize = 2048;

/// 磁盘文档只含当前定义格式；旧 sessions.json 必须先由一次性迁移转换。
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PortForwardDefinitionsDocument {
    #[serde(rename = "schema_version")]
    schema_version: u32,
    definitions: Vec<PortForwardDefinition>,
}

/// 使用正式数据根和共享 storage lock，使配置替换与其它应用文件写入互斥。
#[derive(Clone)]
pub struct PortForwardDefinitionStore {
    storage: StorageCoordinator,
}

impl PortForwardDefinitionStore {
    /// 注入唯一配置根的共享协调器，禁止仓储自行推断 home 或开发目录。
    pub fn new(storage: StorageCoordinator) -> Self {
        Self { storage }
    }

    /// 把同步文件 I/O 投递到阻塞池，并在锁内读取单一当前 schema 文档。
    async fn read_definitions(&self) -> AppResult<Vec<PortForwardDefinition>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = store.storage.lock().map_err(storage_lock_error)?;
            let Some(directory) = store.ensure_directory(false)? else {
                return Ok(Vec::new());
            };
            let path = directory.join(PORT_FORWARD_DEFINITIONS_FILE_NAME);
            let document = read_document(&path)?;
            Ok(document.definitions)
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("读取端口转发定义任务异常结束", error)
        })?
    }

    /// 新增和编辑分离为显式 CAS；锁内重读基线后才生成新文档，不把 upsert 当作冲突恢复。
    async fn write_definition(
        &self,
        expected: Option<PortForwardDefinition>,
        definition: PortForwardDefinition,
    ) -> AppResult<()> {
        definition.validate()?;
        if let Some(expected) = expected.as_ref() {
            expected.validate()?;
            if expected.id != definition.id {
                return Err(AppError::invalid("编辑转发不能改变定义标识"));
            }
        }
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = store.storage.lock().map_err(storage_lock_error)?;
            let directory = store
                .ensure_directory(true)?
                .ok_or_else(|| AppError::infrastructure("端口转发定义目录不可用"))?;
            let path = directory.join(PORT_FORWARD_DEFINITIONS_FILE_NAME);
            let original = read_document_bytes(&path)?;
            let mut document = parse_document(original.as_deref())?;
            validate_document(&document)?;
            let existing = document
                .definitions
                .iter_mut()
                .find(|existing| existing.id == definition.id);
            match (expected.as_ref(), existing) {
                (None, Some(_)) => {
                    return Err(AppError::conflict("端口转发定义已存在，请重新读取"));
                }
                (Some(_), None) => {
                    return Err(AppError::conflict("端口转发定义已被删除，请重新读取"));
                }
                (Some(expected), Some(existing)) if &*existing == expected => {
                    *existing = definition;
                }
                (Some(_), Some(_)) => {
                    return Err(AppError::conflict("端口转发定义已变化，请重新读取后编辑"));
                }
                (None, None) => {
                    if document.definitions.len() >= MAX_PORT_FORWARD_DEFINITIONS {
                        return Err(AppError::invalid("端口转发定义数量达到上限"));
                    }
                    document.definitions.push(definition);
                }
            }
            document
                .definitions
                .sort_by(|left, right| left.id.cmp(&right.id));
            let updated = encode_document(&document)?;
            commit_document(&path, original.as_deref(), &updated)
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("保存端口转发定义任务异常结束", error)
        })?
    }

    /// 删除只改写定义数组；若目标期间被外部进程修改则拒绝覆盖并保留其新内容。
    async fn delete_definition(&self, expected: PortForwardDefinition) -> AppResult<bool> {
        expected.validate()?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = store.storage.lock().map_err(storage_lock_error)?;
            let Some(directory) = store.ensure_directory(false)? else {
                return Ok(false);
            };
            let path = directory.join(PORT_FORWARD_DEFINITIONS_FILE_NAME);
            let original = read_document_bytes(&path)?;
            let mut document = parse_document(original.as_deref())?;
            validate_document(&document)?;
            let Some(index) = document
                .definitions
                .iter()
                .position(|definition| definition.id == expected.id)
            else {
                return Ok(false);
            };
            if document.definitions[index] != expected {
                return Err(AppError::conflict("端口转发定义已变化，请重新确认删除目标"));
            }
            document.definitions.remove(index);
            let updated = encode_document(&document)?;
            commit_document(&path, original.as_deref(), &updated)?;
            Ok(true)
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("删除端口转发定义任务异常结束", error)
        })?
    }

    /// 仅创建固定目录层级并逐级拒绝符号链接，避免数据根下的转发文件被重定向。
    fn ensure_directory(&self, create: bool) -> AppResult<Option<PathBuf>> {
        let root = self.storage.root();
        let mut current = root.to_path_buf();
        for component in ["data", "port-forwards"] {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(AppError::configuration_corrupt("端口转发定义目录")
                        .with_source(UnsafePortForwardPath));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|source| {
                        AppError::infrastructure_with_source("创建端口转发定义目录失败", source)
                    })?;
                    let metadata = fs::symlink_metadata(&current).map_err(|source| {
                        AppError::infrastructure_with_source("检查端口转发定义目录失败", source)
                    })?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        return Err(AppError::configuration_corrupt("端口转发定义目录")
                            .with_source(UnsafePortForwardPath));
                    }
                }
                Err(source) => {
                    return Err(AppError::infrastructure_with_source(
                        "读取端口转发定义目录失败",
                        source,
                    ));
                }
            }
        }
        Ok(Some(current))
    }
}

#[async_trait]
impl PortForwardDefinitionRepository for PortForwardDefinitionStore {
    /// 读取已保存定义；缺文件只代表首次使用，畸形内容始终阻断加载。
    async fn list(&self) -> AppResult<Vec<PortForwardDefinition>> {
        self.read_definitions().await
    }

    /// 新建只能写入缺失 ID，不允许其它调用方绕过应用门禁覆盖已有配置。
    async fn create(&self, definition: PortForwardDefinition) -> AppResult<()> {
        self.write_definition(None, definition).await
    }

    /// 精确配置基线通过 CAS 后才能替换，不保留 PID、listener 或运行 lease。
    async fn update(
        &self,
        expected: PortForwardDefinition,
        definition: PortForwardDefinition,
    ) -> AppResult<()> {
        self.write_definition(Some(expected), definition).await
    }

    /// 原子删除精确 ID；活动资源由应用层在调用此方法前阻止删除。
    async fn delete(&self, expected: PortForwardDefinition) -> AppResult<bool> {
        self.delete_definition(expected).await
    }
}

/// 缺文件初始化空文档；存在文档必须严格满足当前 schema 并通过全量定义校验。
fn read_document(path: &Path) -> AppResult<PortForwardDefinitionsDocument> {
    let bytes = read_document_bytes(path)?;
    let document = parse_document(bytes.as_deref())?;
    validate_document(&document)?;
    Ok(document)
}

/// 安全读取普通文件并限制大小；符号链接、非文件和读取中变化都不能伪装成空清单。
fn read_document_bytes(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取端口转发定义元数据失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(
            AppError::configuration_corrupt("端口转发定义文件").with_source(UnsafePortForwardPath)
        );
    }
    if metadata.len() > MAX_PORT_FORWARD_DEFINITIONS_BYTES {
        return Err(AppError::configuration_corrupt("端口转发定义文件")
            .with_source(PortForwardDocumentTooLarge));
    }
    let file = File::open(path)
        .map_err(|source| AppError::infrastructure_with_source("打开端口转发定义失败", source))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PORT_FORWARD_DEFINITIONS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AppError::infrastructure_with_source("读取端口转发定义失败", source))?;
    if bytes.len() as u64 > MAX_PORT_FORWARD_DEFINITIONS_BYTES {
        return Err(AppError::configuration_corrupt("端口转发定义文件")
            .with_source(PortForwardDocumentTooLarge));
    }
    Ok(Some(bytes))
}

/// 拒绝空文档、未知 schema 和未知字段，不把版本不匹配降级为默认列表。
fn parse_document(bytes: Option<&[u8]>) -> AppResult<PortForwardDefinitionsDocument> {
    let Some(bytes) = bytes else {
        return Ok(PortForwardDefinitionsDocument {
            schema_version: PORT_FORWARD_DEFINITIONS_SCHEMA_VERSION,
            definitions: Vec::new(),
        });
    };
    validate_config_shapes(bytes)?;
    let document: PortForwardDefinitionsDocument =
        serde_json::from_slice(bytes).map_err(|source| {
            AppError::configuration_corrupt("端口转发定义文件").with_source(source)
        })?;
    if document.schema_version != PORT_FORWARD_DEFINITIONS_SCHEMA_VERSION {
        return Err(AppError::configuration_corrupt("端口转发定义文件")
            .with_source(UnsupportedPortForwardSchema));
    }
    Ok(document)
}

/// 领域配置还供兼容 MCP 读取，仓储额外拒绝未知嵌套字段，避免误保存秘密或静默丢弃拼错的选项。
fn validate_config_shapes(bytes: &[u8]) -> AppResult<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|source| {
        AppError::configuration_corrupt("端口转发定义文件").with_source(source)
    })?;
    let definitions = value
        .get("definitions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AppError::configuration_corrupt("端口转发定义文件"))?;
    for definition in definitions {
        let config = definition
            .get("config")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| AppError::configuration_corrupt("端口转发定义文件"))?;
        if config.keys().any(|key| {
            ![
                "hostId",
                "name",
                "kind",
                "bind",
                "destination",
                "proxyProtocol",
                "remoteAccessScope",
                "proxyApplyScope",
            ]
            .contains(&key.as_str())
        }) {
            return Err(AppError::configuration_corrupt("端口转发定义文件"));
        }
        for field in ["bind", "destination"] {
            let Some(endpoint) = config.get(field).filter(|value| !value.is_null()) else {
                continue;
            };
            if endpoint.as_object().is_none_or(|endpoint| {
                endpoint
                    .keys()
                    .any(|key| !["host", "port"].contains(&key.as_str()))
            }) {
                return Err(AppError::configuration_corrupt("端口转发定义文件"));
            }
        }
    }
    Ok(())
}

/// 校验全量记录和 ID 唯一性，损坏数据在任何写操作前失败关闭。
fn validate_document(document: &PortForwardDefinitionsDocument) -> AppResult<()> {
    if document.definitions.len() > MAX_PORT_FORWARD_DEFINITIONS {
        return Err(AppError::configuration_corrupt("端口转发定义文件")
            .with_source(TooManyPortForwardDefinitions));
    }
    let mut ids = std::collections::HashSet::with_capacity(document.definitions.len());
    for definition in &document.definitions {
        definition
            .validate()
            .map_err(|error| AppError::configuration_corrupt("端口转发定义").with_source(error))?;
        if !ids.insert(definition.id.as_str()) {
            return Err(AppError::configuration_corrupt("端口转发定义文件")
                .with_source(DuplicatePortForwardDefinition));
        }
    }
    Ok(())
}

/// 序列化固定版本文件并限制提交大小，保持文件字段顺序可审阅。
fn encode_document(document: &PortForwardDefinitionsDocument) -> AppResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|source| AppError::infrastructure_with_source("序列化端口转发定义失败", source))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_PORT_FORWARD_DEFINITIONS_BYTES {
        return Err(AppError::invalid("端口转发定义文件超过大小上限"));
    }
    Ok(bytes)
}

/// 复用共享原子提交保存权限和 no-clobber 语义；未知提交保留 staging，并由原事务回读核对。
fn commit_document(path: &Path, original: Option<&[u8]>, updated: &[u8]) -> AppResult<()> {
    let staging = write_staging(path, updated)?;
    let current = match read_document_bytes(path) {
        Ok(current) => current,
        Err(error) => {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
    };
    if current.as_deref() != original {
        let _ = fs::remove_file(&staging);
        return Err(AppError::conflict(
            "端口转发定义在保存期间已变化，请重新读取",
        ));
    }
    let mode = if original.is_some() {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::CreateNew
    };
    match commit_staged_file(&staging, path, mode) {
        Ok(()) => {}
        Err(AtomicCommitError::NotCommitted(source)) => {
            let _ = fs::remove_file(&staging);
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(AppError::conflict(
                    "端口转发定义在创建期间已出现，请重新读取",
                ));
            }
            return Err(AppError::infrastructure_with_source(
                "端口转发定义原子提交失败",
                source,
            ));
        }
        Err(AtomicCommitError::CommitUnknown(source)) => {
            return Err(AppError::outcome_unknown_with_source(
                "端口转发定义提交结果未知，请重新读取后处理",
                source,
            ));
        }
    }
    match read_document_bytes(path) {
        Ok(Some(current)) if current == updated => {}
        Ok(_) => return Err(AppError::outcome_unknown("端口转发定义提交后回读不一致")),
        Err(error) => {
            return Err(AppError::outcome_unknown_with_source(
                "端口转发定义提交后无法确认，请重新读取处理",
                error,
            ));
        }
    }
    Ok(())
}

/// 临时文件和目标同目录，避免跨卷移动；Unix 限制权限并使用 create_new 防止踩占位文件。
fn write_staging(destination: &Path, contents: &[u8]) -> AppResult<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::infrastructure("端口转发定义缺少父目录"))?;
    for _ in 0..8 {
        let staging = parent.join(format!(".definitions-{}.pending", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&staging) {
            Ok(mut file) => {
                if let Err(source) = file.write_all(contents).and_then(|()| file.sync_all()) {
                    let _ = fs::remove_file(&staging);
                    return Err(AppError::infrastructure_with_source(
                        "写入端口转发定义 staging 失败",
                        source,
                    ));
                }
                return Ok(staging);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "创建端口转发定义 staging 失败",
                    source,
                ));
            }
        }
    }
    Err(AppError::conflict("端口转发定义 staging 名称冲突"))
}

/// StorageCoordinator 的底层锁细节仅保留在内部错误链，不向 UI 暴露路径或系统文本。
fn storage_lock_error(source: crate::storage::StorageLockError) -> AppError {
    AppError::infrastructure_with_source("无法锁定端口转发定义存储", source)
}

#[derive(Debug)]
struct UnsafePortForwardPath;

impl std::fmt::Display for UnsafePortForwardPath {
    /// 诊断只表明目标不是普通文件或目录，不把用户数据路径写入错误消息。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("port forward storage path is not a regular file or directory")
    }
}

impl std::error::Error for UnsafePortForwardPath {}

#[derive(Debug)]
struct PortForwardDocumentTooLarge;

impl std::fmt::Display for PortForwardDocumentTooLarge {
    /// 大小限制保持为独立诊断类别，避免将异常文件误认为可读空目录。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("port forward definitions exceed the size limit")
    }
}

impl std::error::Error for PortForwardDocumentTooLarge {}

#[derive(Debug)]
struct UnsupportedPortForwardSchema;

impl std::fmt::Display for UnsupportedPortForwardSchema {
    /// 不支持版本需要迁移或升级处理，当前仓储绝不自动覆写。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("unsupported port forward definitions schema")
    }
}

impl std::error::Error for UnsupportedPortForwardSchema {}

#[derive(Debug)]
struct TooManyPortForwardDefinitions;

impl std::fmt::Display for TooManyPortForwardDefinitions {
    /// 文档记录数上限保护内存和工作台列表规模。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("too many port forward definitions")
    }
}

impl std::error::Error for TooManyPortForwardDefinitions {}

#[derive(Debug)]
struct DuplicatePortForwardDefinition;

impl std::fmt::Display for DuplicatePortForwardDefinition {
    /// 重复 ID 会让更新和删除目标产生歧义，因此整个文档必须恢复处理。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("duplicate port forward definition id")
    }
}

impl std::error::Error for DuplicatePortForwardDefinition {}
