//! 外部插件的稳定身份与持久状态，不依赖进程或文件系统。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// 无效身份不携带文件内容，避免把不可信输入带入持久化路径。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidPluginIdentity;

impl fmt::Display for InvalidPluginIdentity {
    /// 使用固定说明，具体输入留给调用方在受控上下文中诊断。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid external plugin identity")
    }
}

impl std::error::Error for InvalidPluginIdentity {}

/// 身份同时用于配置键和目录名，因此始终在构造和反序列化时验证。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(String);

impl PluginId {
    /// 固定 publisher.name 的 ASCII 语法，阻止路径分隔符与大小写别名进入存储；
    /// 同时拒绝会与状态文件或 Windows 设备名冲突的段，因为身份直接用作目录名。
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidPluginIdentity> {
        let value = value.into();
        let Some((publisher, name)) = value.split_once('.') else {
            return Err(InvalidPluginIdentity);
        };
        if !valid_part(publisher, 39) || !valid_part(name, 63) {
            return Err(InvalidPluginIdentity);
        }
        // Windows 判定保留名只看第一个点前的 stem（如 `con.txt`），所以 `acme.con`
        // 这类合法 publisher.name 仍会落成设备名，必须在身份层统一拒绝。
        let first_segment = value.split('.').next().unwrap_or(&value);
        let reserved = matches!(
            first_segment,
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
                | "conin$"
                | "conout$"
        ) || value == "state.json"
            || (value.starts_with("state-recovery-") && value.ends_with(".json"));
        if reserved {
            return Err(InvalidPluginIdentity);
        }
        Ok(Self(value))
    }

    /// 返回验证后的原始身份，供持久键和协议消息复用。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 身份在构造时已完成保留名与语法校验，可直接作为 `plugins/<id>` 的单级目录名；
    /// 暴露该入口让路径构造与身份校验共用同一规则，避免各模块自行拼接。
    pub fn storage_segment(&self) -> &str {
        self.as_str()
    }
}

/// 首字符不能为连字符，其余字符只允许小写 ASCII，保持不同平台的目录身份一致。
fn valid_part(value: &str, maximum: usize) -> bool {
    let bytes = value.as_bytes();
    (2..=maximum).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

impl fmt::Display for PluginId {
    /// 显示稳定身份而非本地化名称，避免运行时映射依赖界面语言。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PluginId {
    type Err = InvalidPluginIdentity;

    /// 字符串入口与存储入口使用同一身份规则。
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PluginId {
    /// JSON 对象键保持人类可读的原始稳定身份。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginId {
    /// 不信任磁盘文件中的键，反序列化也必须经过身份门禁。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// 版本必须遵守 SemVer，不能让任意文本参与版本目录拼接。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginVersion(semver::Version);

impl PluginVersion {
    /// 仅接受规范形式，避免同一版本出现多个目录或比较身份。
    pub fn parse(value: &str) -> Result<Self, InvalidPluginIdentity> {
        let version = semver::Version::parse(value).map_err(|_| InvalidPluginIdentity)?;
        if version.to_string() != value {
            return Err(InvalidPluginIdentity);
        }
        Ok(Self(version))
    }

    /// 暴露 SemVer 规则供兼容性决策使用，不在领域层猜测宿主 API 版本。
    pub fn as_semver(&self) -> &semver::Version {
        &self.0
    }
}

impl fmt::Display for PluginVersion {
    /// 输出与解析一致的目录身份，保留预发布与构建元数据。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PluginVersion {
    type Err = InvalidPluginIdentity;

    /// 所有字符串构造路径都接受相同的 SemVer 规范。
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PluginVersion {
    /// 持久状态使用标准 SemVer 文本，便于人工审查与协议互通。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PluginVersion {
    /// 损坏版本不能成为文件路径，也不能在启动时被静默归一化。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// 目录树指纹把“见到的是哪一份内容”变成可持久比较的身份；算法版本写在值里，
/// 后续升级摘要规则时旧记录不会被误判为一致。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginFingerprint(String);

impl PluginFingerprint {
    /// 只接受固定版本前缀与小写十六进制正文，防止任意插件文本冒充已核对结论。
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidPluginIdentity> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("v1:") else {
            return Err(InvalidPluginIdentity);
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(InvalidPluginIdentity);
        }
        Ok(Self(value))
    }

    /// 暴露完整值，供持久化与跨启动比较共用同一表示。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginFingerprint {
    /// 状态文件与诊断都使用带算法版本的完整摘要。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for PluginFingerprint {
    /// 持久格式保持单字符串，避免为结构体引入第二套字段约定。
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginFingerprint {
    /// 磁盘内容不可信，反序列化也必须经过同一摘要格式门禁。
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// 安装来源与运行能力分离；drop-in 表示用户直接放入目录、尚未经过安装确认。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginSource {
    Official,
    Local,
    DropIn,
}

/// 安装状态只描述磁盘证据，不代替运行时进程状态。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginInstallStatus {
    #[default]
    Ready,
    Missing,
    Corrupt,
    /// 旧 script 包仍保留现场以便卸载，但当前宿主不再执行其视图或运行时。
    Unsupported {
        reason: PluginUnsupportedReason,
    },
}

/// 不可支持原因使用固定枚举，避免把插件文本写入持久状态。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUnsupportedReason {
    ScriptRemoved,
    /// 旧记录占用了内置单元身份；保留记录与目录是为了让用户仍能卸载，
    /// 但不能把这份外部内容当作可加载插件。
    ReservedIdentity,
}

/// 单版本目录：活动版本就是 `plugins/<id>/`，`previous` 仅为 UI 编译兼容保留，
/// schema 2 的写入与校验都要求它为 None，回退能力已移除。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledPlugin {
    pub active: PluginVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PluginVersion>,
    pub enabled: bool,
    pub source: PluginSource,
    pub installed_at: String,
    /// 每次启动都复验该摘要；只有目录已缺失的 `Missing` 记录允许缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<PluginFingerprint>,
    #[serde(default)]
    pub status: PluginInstallStatus,
    /// drop-in 默认未确认；启用动作会把它推进为 true，官方与本地安装始终为 true。
    #[serde(default)]
    pub confirmed: bool,
}

/// 待处理操作只描述下一次启动的意图，运行中的快照不会被修改。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingPluginChange {
    Install {
        id: PluginId,
        to: PluginVersion,
        /// 固定暂存路径会丢失同 ID 多次残留证据，因此只持久化 UUID，
        /// 真实位置由布局模块按 `plugins/.staging/<id>/<uuid>` 推导。
        staged: String,
        #[serde(default)]
        fingerprint: Option<PluginFingerprint>,
        source: PluginSource,
        installed_at: String,
    },
    Update {
        id: PluginId,
        to: PluginVersion,
        staged: String,
        #[serde(default)]
        fingerprint: Option<PluginFingerprint>,
    },
    Enable {
        id: PluginId,
    },
    Disable {
        id: PluginId,
    },
    /// 数据删除仍走独立的设置服务；operation_id 只定位本次备份事务。
    Uninstall {
        id: PluginId,
        #[serde(default)]
        operation_id: String,
    },
    /// UI 兼容外形：服务端拒绝并返回类型化错误，P4 删除界面入口后再移除变体。
    Rollback {
        id: PluginId,
        to: PluginVersion,
    },
}

impl PendingPluginChange {
    /// 提取操作所属身份，使顺序应用与错误定位都不依赖字符串解析。
    pub fn plugin_id(&self) -> &PluginId {
        match self {
            Self::Install { id, .. }
            | Self::Update { id, .. }
            | Self::Enable { id }
            | Self::Disable { id }
            | Self::Uninstall { id, .. }
            | Self::Rollback { id, .. } => id,
        }
    }
}

/// 启动窗口只记录已冻结到首帧之前的崩溃，避免普通退出触发安全模式。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginBootPhase {
    #[default]
    Starting,
    RegistryFrozen,
    FirstFrame,
}

/// 启动身份防止迟到首帧回执清除较新进程留下的崩溃证据。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBootMarker {
    pub id: String,
    pub phase: PluginBootPhase,
    pub consecutive_early_crashes: u8,
}

/// 唯一权威文件的内存投影；未知 schema 必须由存储层拒绝而非覆盖。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStoreState {
    pub schema: u32,
    pub revision: u64,
    pub plugins: BTreeMap<PluginId, InstalledPlugin>,
    /// 只记录被用户改过的内置插件启停；缺省表示启用，P3 才消费该表。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtins: BTreeMap<PluginId, bool>,
    pub pending: Vec<PendingPluginChange>,
    pub failures: BTreeMap<PluginId, String>,
    /// 只有启动应用 pending 失败产生的 failure 才属于可撤销意图；包读取失败不能被
    /// 后续排队的新 pending 借用，因此用独立集合持久化绑定，旧状态缺省为空。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pending_failures: BTreeSet<PluginId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<PluginBootMarker>,
}

impl Default for PluginStoreState {
    /// 缺失状态从空版本开始；损坏文件由存储层单独报告，不能套用此默认值。
    fn default() -> Self {
        Self {
            schema: 2,
            revision: 0,
            plugins: BTreeMap::new(),
            builtins: BTreeMap::new(),
            pending: Vec::new(),
            failures: BTreeMap::new(),
            pending_failures: BTreeSet::new(),
            boot: None,
        }
    }
}

/// 进程状态与安装状态分开，运行失败不会暗中改写用户的启用选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginRuntimeState {
    Idle,
    Starting,
    Ready { generation: u64 },
    Crashed { generation: u64 },
    Failed,
    Stopping,
}
