//! 已加密连接凭据的引用格式与值对象。
//!
//! @author kongweiguang

use std::{error::Error, fmt};

use crate::{connections::CredentialRef, identity::HostId};

const REF_PREFIX: &str = "credential:kerminal";
const REF_VERSION: &str = "v1";
const MAX_COMPONENT_CHARS: usize = 128;

/// 凭据所属协议上下文；使用受校验的新类型保留旧版扩展 kind。
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CredentialKind(String);

/// target 或 jump-N 作用域，避免把跳板秘密绑定到错误连接层。
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CredentialScope(String);

/// 密钥材料类别用于验证引用与选择零化解密结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CredentialMaterial {
    Password,
    PrivateKey,
    KeyPassphrase,
}

/// 对 legacy secret-ref 结构的已验证拆解；原引用自身仍保持 Debug 脱敏。
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialReference {
    reference: CredentialRef,
    kind: CredentialKind,
    host_id: HostId,
    scope: CredentialScope,
    material: CredentialMaterial,
}

/// 引用格式错误保持静态描述，不回显 host id、scope 或 reference 内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialReferenceError {
    InvalidComponent,
    InvalidFormat,
    InvalidHostId,
    InvalidMaterial,
}

impl CredentialKind {
    /// 接受 legacy 扩展协议名，同时拒绝引用分隔符和可能进入诊断的控制字符。
    pub fn parse(value: &str) -> Result<Self, CredentialReferenceError> {
        validate_component(value)?;
        Ok(Self(value.to_owned()))
    }

    /// 提供稳定的 legacy reference 组件，不对协议名作大小写归一。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl CredentialScope {
    /// 构造 target 级凭据作用域。
    pub fn target() -> Self {
        Self("target".to_owned())
    }

    /// 构造 legacy jump-N 作用域，索引按原始跳板链顺序保存。
    pub fn jump(index: u32) -> Self {
        Self(format!("jump-{index}"))
    }

    /// 解析已持久化的扩展 scope 并保留其原始拼写。
    pub fn parse(value: &str) -> Result<Self, CredentialReferenceError> {
        validate_component(value)?;
        Ok(Self(value.to_owned()))
    }

    /// 返回 legacy reference 中的稳定 scope 组件。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl CredentialMaterial {
    /// 返回 legacy reference 中使用的材料名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::PrivateKey => "private-key",
            Self::KeyPassphrase => "key-passphrase",
        }
    }

    /// 只解析旧版已使用的材料值，避免误把未知材料当作密码传给 SSH/RDP。
    pub fn parse(value: &str) -> Result<Self, CredentialReferenceError> {
        match value {
            "password" => Ok(Self::Password),
            "private-key" => Ok(Self::PrivateKey),
            "key-passphrase" => Ok(Self::KeyPassphrase),
            _ => Err(CredentialReferenceError::InvalidMaterial),
        }
    }
}

impl CredentialReference {
    /// 从有类型的身份组件生成仍兼容 v0.3.45 的 opaque 引用。
    pub fn new(
        kind: CredentialKind,
        host_id: HostId,
        scope: CredentialScope,
        material: CredentialMaterial,
    ) -> Result<Self, CredentialReferenceError> {
        validate_component(host_id.as_str())?;
        let value = format!(
            "{REF_PREFIX}:{}:{}:{}:{}:{REF_VERSION}",
            kind.as_str(),
            host_id.as_str(),
            scope.as_str(),
            material.as_str()
        );
        Self::parse(CredentialRef::new(value).map_err(|_| CredentialReferenceError::InvalidFormat)?)
    }

    /// 解析七段式旧引用，确保未知或损坏的材料不会被静默降级。
    pub fn parse(reference: CredentialRef) -> Result<Self, CredentialReferenceError> {
        let parts = reference.as_str().split(':').collect::<Vec<_>>();
        if parts.len() != 7
            || parts[0] != "credential"
            || parts[1] != "kerminal"
            || parts[6] != REF_VERSION
        {
            return Err(CredentialReferenceError::InvalidFormat);
        }
        let kind = CredentialKind::parse(parts[2])?;
        let host_id =
            HostId::parse(parts[3]).map_err(|_| CredentialReferenceError::InvalidHostId)?;
        validate_component(host_id.as_str())?;
        let scope = CredentialScope::parse(parts[4])?;
        let material = CredentialMaterial::parse(parts[5])?;
        Ok(Self {
            reference,
            kind,
            host_id,
            scope,
            material,
        })
    }

    /// 返回 opaque vault key；其 Debug 实现会隐藏完整引用。
    pub fn reference(&self) -> &CredentialRef {
        &self.reference
    }

    /// 返回协议 kind，供 vault entry 和 MCP 回执使用。
    pub fn kind(&self) -> &CredentialKind {
        &self.kind
    }

    /// 返回稳定主机身份，供应用层检查引用与当前 host 配置是否匹配。
    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 返回 target 或 jump-N 作用域。
    pub fn scope(&self) -> &CredentialScope {
        &self.scope
    }

    /// 返回已验证材料类型，决定解密后的 Zeroizing 包装类型。
    pub const fn material(&self) -> CredentialMaterial {
        self.material
    }
}

impl fmt::Debug for CredentialReference {
    /// Host ID、scope 与 opaque ref 都可能暴露用户连接布局，因此仅输出固定占位符。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialReference([redacted])")
    }
}

impl fmt::Display for CredentialReferenceError {
    /// 只输出静态错误，不将输入内容拼入应用日志或公开错误。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidComponent => "credential reference component is invalid",
            Self::InvalidFormat => "credential reference format is invalid",
            Self::InvalidHostId => "credential host identity is invalid",
            Self::InvalidMaterial => "credential material is unsupported",
        })
    }
}

impl Error for CredentialReferenceError {}

/// 限定 reference 组件边界，避免冒号改变字段分段并限制异常持久化输入。
fn validate_component(value: &str) -> Result<(), CredentialReferenceError> {
    if value.is_empty()
        || value.chars().count() > MAX_COMPONENT_CHARS
        || value.contains(':')
        || value.chars().any(char::is_control)
    {
        return Err(CredentialReferenceError::InvalidComponent);
    }
    Ok(())
}
