//! 凭据保存与 vault key 管理用例端口。
//!
//! @author kongweiguang

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::{
    connections::{
        CredentialRef,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
};
use zeroize::Zeroizing;

use crate::{AppError, AppResult};

/// 一次凭据保存请求；Debug 有意隐藏秘密正文。
pub struct SaveCredentialRequest {
    pub kind: CredentialKind,
    pub host_id: HostId,
    pub scope: CredentialScope,
    pub material: CredentialMaterial,
    pub secret: SecretValue,
}

/// 保存后供 MCP 返回兼容元数据；reference 与诊断输出保持脱敏。
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialSaveReceipt {
    pub reference: CredentialRef,
    pub entry_id: String,
    pub kind: String,
    pub key_id: String,
    pub updated_at: String,
}

/// Vault adapter 写入后的稳定元数据，不包含明文、nonce 或密文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialVaultSaveMetadata {
    pub entry_id: String,
    pub kind: String,
    pub key_id: String,
    pub updated_at: String,
}

/// 暂存一条 vault 引用供 UI 显示已保存/缺失，不能据此回显秘密。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPresence {
    Saved,
    Missing,
}

/// 明文只在当前用例内短暂存在，进入 repository 时转换为零化字符串。
pub struct SecretValue(Zeroizing<String>);

/// 明确标注一次性导入或导出的 key TOML，且默认调试输出不含 key 正文。
pub struct VaultKeyDocument(Zeroizing<String>);

/// key 保存、导入和轮换的脱敏回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultKeyOperationReceipt {
    pub key_id: String,
    pub dry_run: bool,
    pub entry_count: usize,
    pub backup_created: bool,
}

/// 文件内容错误时只返回有限原因码，不把路径、TOML 或凭据内容传到 UI。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VaultRecoveryReason {
    InvalidKey,
    InvalidVault,
    KeyMismatch,
    PairTransaction,
    UnsafePath,
    StorageUnavailable,
}

/// Vault readiness 同时区分空工作区、可用、缺少独立 key 和需显式恢复的文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VaultKeyAvailability {
    NotInitialized,
    Ready { key_id: String },
    Created { key_id: String },
    MissingForExistingVault,
    RecoveryRequired { reason: VaultRecoveryReason },
}

/// 文件导出只允许调用方明确选择新建或覆盖，避免隐式覆盖安全备份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VaultKeyExportMode {
    CreateNew,
    ReplaceExisting,
}

/// 凭据 vault 的持久化端口；明文用零化类型跨越应用与基础设施边界。
#[async_trait]
pub trait CredentialVaultRepository: Send + Sync + 'static {
    /// 加密并 upsert 单条凭据；更新 entry 不改变其首次创建时间。
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata>;

    /// 检查引用是否存在但不解密或回传秘密。
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool>;

    /// 删除指定引用；缺失项返回 false，调用方可在 host CAS 成功后安全清理。
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool>;
}

/// legacy key 文档管理端口；实现负责兼容 schema 1 和 pair recovery。
#[async_trait]
pub trait VaultKeyRepository: Send + Sync + 'static {
    /// 先恢复本地 pair journal，再诊断当前状态；不自动创建 key 或替换损坏文件。
    async fn availability(&self) -> AppResult<VaultKeyAvailability>;

    /// 仅在 key/vault 均缺失时创建 key；若有密文却缺 key 则不覆盖任何文件。
    async fn ensure_key(&self) -> AppResult<VaultKeyAvailability>;

    /// 显式创建新 key，只允许 key 和 vault 都不存在的空工作区。
    async fn create_key(&self) -> AppResult<VaultKeyOperationReceipt>;

    /// 读取原始 key TOML 供用户保存到离线安全位置。
    async fn read_key(&self) -> AppResult<VaultKeyDocument>;

    /// 按旧版导出语义规范化并返回 key TOML。
    async fn export_key(&self) -> AppResult<VaultKeyDocument>;

    /// 将规范化 key 导出到用户显式选择的路径，覆盖语义必须由调用方明确传入。
    async fn export_key_to_path(
        &self,
        path: PathBuf,
        mode: VaultKeyExportMode,
    ) -> AppResult<VaultKeyOperationReceipt>;

    /// 校验 key 与当前 vault 后保存，保留旧版 key 备份行为。
    async fn save_key(&self, document: VaultKeyDocument) -> AppResult<VaultKeyOperationReceipt>;

    /// dry-run 只校验现有 vault 是否可用输入 key 解密，不写入文件。
    async fn import_key(
        &self,
        document: VaultKeyDocument,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt>;

    /// 从用户显式选择的文件导入 key；读取与验证由 adapter 完成，不交给 UI 做文件 IO。
    async fn import_key_from_path(
        &self,
        path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt>;

    /// 从用户选择的一组 key/vault 文件恢复损坏 pair；只有全量校验通过才会提交。
    async fn restore_pair_from_paths(
        &self,
        key_path: PathBuf,
        vault_path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt>;

    /// 重新加密全部 entries；提交前备份旧 key 和 vault，dry-run 不改磁盘。
    async fn rotate_key(&self, dry_run: bool) -> AppResult<VaultKeyOperationReceipt>;
}

/// 只保存一次性输入并返回 opaque ref，避免 UI 或 MCP DTO 持有 vault adapter。
#[derive(Clone)]
pub struct CredentialService {
    repository: Arc<dyn CredentialVaultRepository>,
}

/// 将旧版 key 维护命令转换为可注入的 typed application use case。
#[derive(Clone)]
pub struct VaultKeyManagementService {
    repository: Arc<dyn VaultKeyRepository>,
}

impl SecretValue {
    /// 接收 UI/MCP 一次性秘密并立即置于 zeroize 容器中。
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// 将秘密所有权交给加密 adapter，防止调用方保存第二份普通 String。
    pub fn into_zeroizing(self) -> Zeroizing<String> {
        self.0
    }
}

impl VaultKeyDocument {
    /// 包装用户选定的旧格式 key TOML，退出应用用例后清除临时文本。
    pub fn new(source: String) -> Self {
        Self(Zeroizing::new(source))
    }

    /// 接收已经零化的 key 编码，避免 adapter 生成导出文本时再创建普通明文副本。
    pub fn from_zeroizing(source: Zeroizing<String>) -> Self {
        Self(source)
    }

    /// 返回只读文本以供 UI 导出或用户检查，调用方不得记录或缓存。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// 将文档交给 repository 解析并写入，避免再复制一份明文 TOML。
    pub fn into_zeroizing(self) -> Zeroizing<String> {
        self.0
    }
}

impl std::fmt::Debug for SecretValue {
    /// 脱敏调试输出，防止误将用户输入的凭据写进日志。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretValue([redacted])")
    }
}

impl std::fmt::Debug for CredentialSaveReceipt {
    /// MCP 需显式序列化兼容字段，普通 Debug 不暴露主机引用或 vault entry id。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialSaveReceipt([redacted])")
    }
}

impl std::fmt::Debug for VaultKeyDocument {
    /// key TOML 本身含 master key，调试信息只能显示固定占位符。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VaultKeyDocument([redacted])")
    }
}

impl CredentialService {
    /// 由 composition root 注入 vault repository，应用用例不认识文件路径。
    pub fn new(repository: Arc<dyn CredentialVaultRepository>) -> Self {
        Self { repository }
    }

    /// 保存到 legacy reference 对应 entry；host 配置仍由调用方以 CAS 独立提交。
    pub async fn save(&self, request: SaveCredentialRequest) -> AppResult<CredentialRef> {
        self.save_with_metadata(request)
            .await
            .map(|receipt| receipt.reference)
    }

    /// 保存凭据并返回旧 MCP 所需的脱敏索引元数据，避免 adapter 被 MCP 直接调用。
    pub async fn save_with_metadata(
        &self,
        request: SaveCredentialRequest,
    ) -> AppResult<CredentialSaveReceipt> {
        let reference = CredentialReference::new(
            request.kind,
            request.host_id,
            request.scope,
            request.material,
        )
        .map_err(|_| AppError::invalid("凭据引用参数无效"))?;
        let metadata = self
            .repository
            .save(reference.clone(), request.secret.into_zeroizing())
            .await?;
        Ok(CredentialSaveReceipt {
            reference: reference.reference().clone(),
            entry_id: metadata.entry_id,
            kind: metadata.kind,
            key_id: metadata.key_id,
            updated_at: metadata.updated_at,
        })
    }

    /// 只确认已保存状态，编辑界面不得回填旧秘密。
    pub async fn inspect(&self, reference: &CredentialRef) -> AppResult<CredentialPresence> {
        CredentialReference::parse(reference.clone())
            .map_err(|_| AppError::invalid("凭据引用格式无效"))?;
        if self.repository.contains(reference).await? {
            Ok(CredentialPresence::Saved)
        } else {
            Ok(CredentialPresence::Missing)
        }
    }

    /// 删除明确指定的 vault ref；host CAS 的调用方应只在清除引用成功后调用。
    pub async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        CredentialReference::parse(reference.clone())
            .map_err(|_| AppError::invalid("凭据引用格式无效"))?;
        self.repository.remove(reference).await
    }
}

impl VaultKeyManagementService {
    /// 注入共享 vault adapter，令设置界面不直接读写 key/vault 文件。
    pub fn new(repository: Arc<dyn VaultKeyRepository>) -> Self {
        Self { repository }
    }

    /// 读取 typed readiness，不创建 key，供设置页与全局恢复提示展示当前状态。
    pub async fn availability(&self) -> AppResult<VaultKeyAvailability> {
        self.repository.availability().await
    }

    /// 读取兼容格式 key 文档；返回值调试输出脱敏并会在释放时清除。
    pub async fn read(&self) -> AppResult<VaultKeyDocument> {
        self.repository.read_key().await
    }

    /// 确保新工作区可创建凭据，但不会用随机 key 覆盖现存密文。
    pub async fn ensure(&self) -> AppResult<VaultKeyAvailability> {
        self.repository.ensure_key().await
    }

    /// 显式创建首个 key，已存在 key 或 vault 时返回冲突。
    pub async fn create(&self) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.create_key().await
    }

    /// 导出规范化 key TOML，沿用旧版离线备份能力。
    pub async fn export(&self) -> AppResult<VaultKeyDocument> {
        self.repository.export_key().await
    }

    /// 将用户明确选择的 key 写到路径，原子提交并拒绝未授权覆盖。
    pub async fn export_to_path(
        &self,
        path: PathBuf,
        mode: VaultKeyExportMode,
    ) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.export_key_to_path(path, mode).await
    }

    /// 保存 key 前验证它能够解密当前所有 vault entries。
    pub async fn save(&self, document: VaultKeyDocument) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.save_key(document).await
    }

    /// dry-run 可用于导入前检查，写入成功回执包含备份与 entry 数量。
    pub async fn import(
        &self,
        document: VaultKeyDocument,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.import_key(document, dry_run).await
    }

    /// 从用户选择的文件导入 key，避免 UI 读取或缓存明文 key TOML。
    pub async fn import_from_path(
        &self,
        path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.import_key_from_path(path, dry_run).await
    }

    /// 显式恢复选定的 key 与密文文件；adapter 完整验证 pair 后才备份并提交。
    pub async fn restore_pair_from_paths(
        &self,
        key_path: PathBuf,
        vault_path: PathBuf,
        dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        self.repository
            .restore_pair_from_paths(key_path, vault_path, dry_run)
            .await
    }

    /// 轮换时重新加密所有凭据并保留旧版回滚备份。
    pub async fn rotate(&self, dry_run: bool) -> AppResult<VaultKeyOperationReceipt> {
        self.repository.rotate_key(dry_run).await
    }
}
