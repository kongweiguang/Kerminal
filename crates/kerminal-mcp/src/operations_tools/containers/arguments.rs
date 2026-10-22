//! 容器 MCP 工具各自的严格 wire 参数类型。
//!
//! 每个类型只声明该工具 schema 中允许的字段；不共用字段并集，以便在业务调用前拒绝跨工具参数。
//!
//! @author kongweiguang

use serde::{Deserialize, Deserializer};

/// 让 schema 的 optional 表示“可省略”，而不是把显式 JSON null 当成合法值。
///
/// @author kongweiguang
fn deserialize_non_null_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// 容器列表只接受列表筛选参数，不能接收其它容器工具的路径字段。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerListArgs {
    pub(super) host_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) include_stopped: Option<bool>,
}

/// 读状态和普通生命周期工具共享完全相同的目标 schema。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerTargetArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
}

/// 容器日志工具只额外接受有界 tail 参数。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerLogsArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) tail: Option<u32>,
}

/// 移除工具独有 force 参数；其它目标动作不能借此静默传入强制删除意图。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerRemoveArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) force: Option<bool>,
}

/// 容器内单路径工具的共同精确 schema。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerPathArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) path: String,
}

/// 文件预览仅允许额外限制字节数，不接受写入或传输字段。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerPreviewArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) path: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) max_bytes: Option<usize>,
}

/// 文本写入参数按冻结 schema 区分 required flags 与 optional revision。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerWriteArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) path: String,
    pub(super) content: String,
    pub(super) encoding: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) expected_revision: Option<RevisionArgs>,
    pub(super) create: bool,
    pub(super) overwrite_on_conflict: bool,
}

/// 文件 revision 是嵌套的精确对象，禁止扩展字段被忽略后误当成 CAS 输入。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RevisionArgs {
    pub(super) size: u64,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) modified: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) permissions: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) permissions_mode: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) content_sha256: Option<String>,
}

/// Rename 只接收源与目标路径，避免单路径字段并集掩盖参数拼写错误。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerRenameArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) from_path: String,
    pub(super) to_path: String,
}

/// Chmod 的 mode 作为文本接收后由 handler 做八进制校验。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerChmodArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) path: String,
    pub(super) mode: String,
}

/// 上传和下载使用一致的方向无关端点 schema。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerTransferArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) remote_path: String,
    pub(super) local_path: String,
    pub(super) kind: String,
}

/// 删除目标类型是 required，防止遗漏后意外选择默认删除范围。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ContainerDeleteArgs {
    pub(super) host_id: String,
    pub(super) container_id: String,
    #[serde(default, deserialize_with = "deserialize_non_null_option")]
    pub(super) runtime: Option<String>,
    pub(super) path: String,
    pub(super) directory: bool,
}
