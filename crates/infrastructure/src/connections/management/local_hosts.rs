//! 复用 host 文件仓储的本地启动快照适配器。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::management::{
        LocalHostDraft, ManagedLocalHost, validate_local_environment_reference,
    },
};
use kerminal_domain::{
    connections::CredentialRef,
    identity::HostId,
    preferences::{ConfigDocument, ConfigDocumentPath, ConfigRevision},
};
use serde::Deserialize;
use tokio::task;

use super::{
    HOST_SCHEMA_VERSION, HostConfigurationRepository, HostDocumentError, corrupt_host,
    ensure_table, set_integer, set_optional_string, set_string, set_string_array, timestamp_now,
};
use crate::configuration::ConfigurationStore;

#[derive(Deserialize)]
struct LocalHostFile {
    id: String,
    group_id: Option<String>,
    name: String,
    sort_order: i64,
    created_at: String,
    updated_at: String,
    migration_origin: Option<String>,
    local: LocalHostFileTarget,
}

#[derive(Deserialize)]
struct LocalHostFileTarget {
    shell: String,
    args: Vec<String>,
    cwd: Option<String>,
    environment_ref: Option<String>,
}

impl HostConfigurationRepository {
    /// 协议只用于选择类型化解码器，未知值仍交给远程解码器拒绝。
    ///
    /// @author kongweiguang
    pub(super) fn document_is_local(document: &ConfigDocument) -> AppResult<bool> {
        let value = toml::from_str::<toml::Value>(&document.source).map_err(corrupt_host)?;
        Ok(value.get("protocol").and_then(toml::Value::as_str) == Some("local"))
    }

    /// 复用配置 validator 并在领域边界复核 ID，不能把秘密或无效引用带入目录投影。
    ///
    /// @author kongweiguang
    pub(super) fn decode_local_host(
        document: ConfigDocument,
        requested_id: &HostId,
    ) -> AppResult<ManagedLocalHost> {
        ConfigurationStore::validate_host_source(requested_id.as_str(), &document.source)?;
        let file = toml::from_str::<LocalHostFile>(&document.source).map_err(corrupt_host)?;
        if file.id != requested_id.as_str() {
            return Err(corrupt_host(HostDocumentError::PathMismatch));
        }
        let environment_ref = file
            .local
            .environment_ref
            .map(CredentialRef::new)
            .transpose()
            .map_err(|_| corrupt_host(HostDocumentError::InvalidCredentialReference))?;
        if let Some(reference) = environment_ref.as_ref() {
            validate_local_environment_reference(requested_id, reference)?;
        }
        Ok(ManagedLocalHost {
            id: requested_id.clone(),
            group_id: file.group_id,
            name: file.name,
            shell: file.local.shell,
            args: file.local.args,
            cwd: file.local.cwd,
            environment_ref,
            sort_order: file.sort_order,
            created_at: file.created_at,
            updated_at: file.updated_at,
            revision: document.revision,
            migration_origin: file.migration_origin,
        })
    }

    /// 同一目录中任何损坏条目都必须显式失败，不能把损坏远程主机伪装成空列表。
    ///
    /// @author kongweiguang
    pub(super) async fn list_local_host_records(&self) -> AppResult<Vec<ManagedLocalHost>> {
        let mut hosts = Vec::new();
        for id in self.host_ids().await? {
            let document = self
                .configuration
                .read(ConfigDocumentPath::Host(id.as_str().to_owned()))
                .await?
                .ok_or_else(|| AppError::conflict("主机目录在读取期间发生变化，请刷新后重试"))?;
            if Self::document_is_local(&document)? {
                hosts.push(Self::decode_local_host(document, &id)?);
            } else {
                Self::decode_host(document, &id)?;
            }
        }
        Ok(hosts)
    }

    /// 按主机 ID 分流本地和远程文档，远程条目不能被本地启动路径误用。
    ///
    /// @author kongweiguang
    pub(super) async fn load_local_host_record(
        &self,
        id: &HostId,
    ) -> AppResult<Option<ManagedLocalHost>> {
        let document = self
            .configuration
            .read(ConfigDocumentPath::Host(id.as_str().to_owned()))
            .await?;
        match document {
            Some(document) if Self::document_is_local(&document)? => {
                Self::decode_local_host(document, id).map(Some)
            }
            Some(document) => {
                Self::decode_host(document, id)?;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// 原扩展字段保留；写入可能提交后丢失回执或读回失败时必须保留恢复材料。
    ///
    /// @author kongweiguang
    pub(super) async fn save_local_host_record(
        &self,
        host_id: HostId,
        draft: LocalHostDraft,
        environment_ref: Option<CredentialRef>,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedLocalHost> {
        if !draft.env.is_empty() {
            return Err(AppError::invalid("本地主机环境必须先密封到 vault"));
        }
        if let Some(reference) = environment_ref.as_ref() {
            validate_local_environment_reference(&host_id, reference)?;
        }
        let (mut document, existing) = self
            .editable_document(&host_id, expected_revision.as_ref())
            .await?;
        if let Some(current) = existing.as_ref() {
            if !Self::document_is_local(current)? {
                return Err(AppError::conflict("主机标识已被其他协议使用"));
            }
            Self::decode_local_host(current.clone(), &host_id)?;
        }
        let now = timestamp_now();
        let root = document.as_table_mut();
        if existing.is_none() {
            set_integer(root, "schema_version", i64::from(HOST_SCHEMA_VERSION));
            set_string(root, "created_at", &now);
        }
        set_string(root, "protocol", "local");
        set_string(root, "id", host_id.as_str());
        set_string(root, "name", &draft.name);
        set_optional_string(root, "group_id", draft.group_id.as_deref());
        set_integer(root, "sort_order", draft.sort_order);
        set_string(root, "updated_at", &now);
        let local = ensure_table(root, "local")?;
        set_string(local, "shell", &draft.shell);
        set_string_array(local, "args", &draft.args);
        set_optional_string(local, "cwd", draft.cwd.as_deref());
        set_optional_string(
            local,
            "environment_ref",
            environment_ref.as_ref().map(CredentialRef::as_str),
        );
        let path = ConfigDocumentPath::Host(host_id.as_str().to_owned());
        let revision = self
            .configuration
            .write(path.clone(), document.to_string(), expected_revision)
            .await
            .map_err(|error| {
                if error.public_code() == PublicErrorCode::Infrastructure {
                    AppError::outcome_unknown_with_source("本地主机保存结果未知，请重新读取", error)
                } else {
                    error
                }
            })?;
        let saved = self
            .configuration
            .read(path)
            .await
            .map_err(|error| {
                AppError::outcome_unknown_with_source("保存后的本地主机无法确认，请重新读取", error)
            })?
            .ok_or_else(|| AppError::outcome_unknown("保存后的本地主机无法确认，请重新读取"))?;
        if saved.revision != revision {
            return Err(AppError::outcome_unknown(
                "本地主机在保存后再次变化，请重新读取",
            ));
        }
        Self::decode_local_host(saved, &host_id).map_err(|error| {
            AppError::outcome_unknown_with_source("保存后的本地主机无法确认，请重新读取", error)
        })
    }

    /// 删除只接受原始 revision，存储结果未知时上层保留环境引用等待读回。
    ///
    /// @author kongweiguang
    pub(super) async fn delete_local_host_record(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedLocalHost> {
        let host = self
            .load_local_host_record(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("本地主机不存在"))?;
        if &host.revision != expected_revision {
            return Err(AppError::conflict("本地主机已变化，请重新读取后删除"));
        }
        let store = self.store.clone();
        let id = host_id.clone();
        let revision = expected_revision.clone();
        let removed = task::spawn_blocking(move || store.delete_host_document(&id, &revision))
            .await
            .map_err(|source| {
                AppError::outcome_unknown_with_source("本地主机删除结果未知，请重新读取", source)
            })??;
        if !removed {
            return Err(AppError::not_found("本地主机已被其他操作删除"));
        }
        Ok(host)
    }
}
