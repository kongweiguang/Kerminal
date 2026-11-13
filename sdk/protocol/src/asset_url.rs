//! 插件资产 URL 的纯语法契约：与安装器网络边界共用同一入口，市场登记侧可离线复用。
//!
//! 这里只判定"原文与解析结果一致、无歧义、无凭据泄露面"，不涉及 DNS、地址分类、
//! 重定向策略或网络访问；这些仍由 `installer/network.rs` 的传输层负责。判定基于
//! 工作区锁定的 `url` 2.5.8（含其 IDNA/ICU 适配），是宿主实际连接语义的唯一来源。
//!
//! @author kongweiguang

use url::Url;

/// URL 语法拒绝原因；文案是源码常量，不拼接输入原文，避免 query 或凭据泄露。
///
/// 判别式枚举供安装器映射既有错误语义，也供 CLI 与市场测试区分失败类别，
/// 不把所有失败混成一个通用错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetUrlRejection {
    /// 原文结构歧义或解析失败：控制字符、反斜杠、首尾空格、多余斜杠 authority、
    /// 缺少 authority 的绝对 http(s) 等。
    Malformed,
    /// 协议不是 http/https。
    UnsupportedScheme,
    /// 缺少主机，或非默认端口缺失导致无法确定连接端口。
    MissingHost,
    /// authority 中出现 userinfo（含解析后会被丢弃的空 userinfo，如 `http://@host/`）。
    UserInfo,
    /// 存在 fragment。
    Fragment,
    /// 端口为 0；解析器接受它，但没有任何可连接目标，登记和下载都应失败关闭。
    ZeroPort,
}

impl AssetUrlRejection {
    /// 固定中文文案；只说明拒绝规则，不回显 URL 任何部分。
    pub fn describe(self) -> &'static str {
        match self {
            Self::Malformed => "格式无效",
            Self::UnsupportedScheme => "协议不受支持",
            Self::MissingHost => "缺少主机",
            Self::UserInfo => "包含用户信息",
            Self::Fragment => "包含片段",
            Self::ZeroPort => "端口为 0",
        }
    }
}

impl std::fmt::Display for AssetUrlRejection {
    /// Display 同样只输出固定文案，调用方不能通过错误文本还原被拒的 URL。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.describe())
    }
}

/// 校验绝对资产 URL：原文结构与解析结果必须一致且无歧义，返回 WHATWG 解析产物。
///
/// 允许 HTTP(S)、公网/局域网/回环主机、IPv4/IPv6 字面量、非默认端口、合法
/// Unicode IDN 原文与任意 path/query；拒绝 userinfo（含空 userinfo）、fragment、
/// 控制字符、反斜杠、首尾空格、歧义 authority 和 0 端口。允许的地址类别、DNS、
/// 重定向等传输层策略不在本契约范围内。
pub fn validate_asset_url(value: &str) -> Result<Url, AssetUrlRejection> {
    check_raw_reference(value)?;
    let url = Url::parse(value).map_err(|_| AssetUrlRejection::Malformed)?;
    validate_parsed_url(url)
}

/// 校验重定向 `Location` 原文并基于当前 URL 解析，拒绝与绝对地址同一套歧义。
///
/// 相对路径、网络路径引用（`//host/path`）与同文档 query（`?x=y`）都会按
/// WHATWG join 规则解析；join 前对 Location 原文执行与绝对 URL 相同的
/// authority 检查，因为 `//@host/` 这类空 userinfo 会在 join 后被静默丢弃。
/// 解析后的目标再经统一校验，保证每一跳与初始地址走同一入口。
pub fn validate_redirect_target(current: &Url, location: &str) -> Result<Url, AssetUrlRejection> {
    check_raw_reference(location)?;
    let next = current
        .join(location)
        .map_err(|_| AssetUrlRejection::Malformed)?;
    validate_parsed_url(next)
}

/// 在 WHATWG 解析前检查原文结构，弥补解析器会静默修复或丢弃的歧义。
///
/// 锁定 url 2.5.8 会把 `https:///host` 可恢复解析成 `https://host`，并把
/// `http://@host/`、`http://:@host/` 的空 userinfo 在序列化时丢掉，仅检查解析后的
/// `username()`/`password()` 无法发现原文。这里只扫描 authority 片段，不限制
/// path/query 中合法的 `@`，也不拒绝 Unicode 路径或可规范化的 IDN 主机。
fn check_raw_reference(value: &str) -> Result<(), AssetUrlRejection> {
    // 首尾空格会被解析器裁剪；控制字符在 path/query/fragment 中可能被忽略或
    // 重新编码；反斜杠在特殊 scheme 中等同于斜杠。这些都让签名原文与连接目标
    // 不一致，按失败关闭原则保守拒绝。中间空格会被一致地百分号编码，不拒绝。
    if value.trim_matches(' ').len() != value.len()
        || value.chars().any(char::is_control)
        || value.contains('\\')
    {
        return Err(AssetUrlRejection::Malformed);
    }
    if raw_authority(value)?.is_some_and(|authority| authority.contains('@')) {
        return Err(AssetUrlRejection::UserInfo);
    }
    Ok(())
}

/// 只提取绝对 http(s) 或 `//` 网络路径引用的 authority 片段。
///
/// 其他 scheme 交由解析后的协议校验统一报 `UnsupportedScheme`；普通相对路径没有
/// authority，返回 `None`，避免把 path/query 里的 `@` 或 `:` 误判成凭据。
fn raw_authority(value: &str) -> Result<Option<&str>, AssetUrlRejection> {
    let (scheme, rest) = match split_scheme(value) {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, value),
    };
    if scheme.is_some_and(|scheme| {
        !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https")
    }) {
        return Ok(None);
    }
    let Some(after) = rest.strip_prefix("//") else {
        // 绝对 http(s) 缺少 `//`（如 `https:/host`）意味着作者意图与实际 authority
        // 不一致，拒绝；普通相对路径 `/path` 不含 authority，保持可用。
        return if scheme.is_some() {
            Err(AssetUrlRejection::Malformed)
        } else {
            Ok(None)
        };
    };
    if after.starts_with('/') {
        // `https:////host`、相对 `///host` 会被解析器折叠成单 authority，拒绝歧义写法。
        return Err(AssetUrlRejection::Malformed);
    }
    Ok(Some(
        after.split(['/', '?', '#']).next().unwrap_or_default(),
    ))
}

/// 按 RFC 3986 语法识别 scheme 前缀，避免把 path/query 中的 `:` 误当成 scheme。
fn split_scheme(value: &str) -> Option<(&str, &str)> {
    let colon = value.find(':')?;
    let scheme = &value[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .then_some((scheme, &value[colon + 1..]))
}

/// 已解析 URL 的统一校验：绝对地址与每一跳 Location 都必须经过同一入口。
///
/// 0 端口在这里拒绝而不是依赖传输层，因为市场登记侧没有传输层可兜底，
/// 两端共享同一失败关闭语义。
fn validate_parsed_url(url: Url) -> Result<Url, AssetUrlRejection> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AssetUrlRejection::UnsupportedScheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AssetUrlRejection::UserInfo);
    }
    if url.fragment().is_some() {
        return Err(AssetUrlRejection::Fragment);
    }
    if url.host().is_none() {
        return Err(AssetUrlRejection::MissingHost);
    }
    if url.port_or_known_default() == Some(0) {
        return Err(AssetUrlRejection::ZeroPort);
    }
    Ok(url)
}
