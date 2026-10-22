//! 可取消的异步更新包传输与 cargo-packager 兼容验签。
//!
//! @author kongweiguang

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use cargo_packager_updater::{Update, reqwest};
use kerminal_application::{AppError, AppResult, UpdateProgress};
use minisign_verify::{PublicKey, Signature};
use reqwest::{
    Certificate, Client, Url,
    header::{ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, PROXY_AUTHORIZATION, USER_AGENT},
    redirect::Policy,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// SDK 未声明请求超时时，仍以有限时长保护更新下载。
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// 限制重定向跳数，避免受控下载端点引入循环或过长链路。
const MAX_REDIRECTS: usize = 10;

/// SDK 下载元数据和本地传输限制作为一个请求规范，避免私有下载入口错位传参。
///
/// @author kongweiguang
struct VerifiedDownloadRequest<'a> {
    url: Url,
    public_key: String,
    signature: String,
    headers: HeaderMap,
    timeout: Option<Duration>,
    max_bytes: u64,
    additional_trust_roots: &'a [Certificate],
}

/// 取消令牌与进度接收端共同描述一次下载的运行控制，不改变任何传输策略。
///
/// @author kongweiguang
struct DownloadControl {
    cancellation: CancellationToken,
    progress: watch::Sender<UpdateProgress>,
}

/// 从 SDK 候选构造下载请求；额外根证书仅经 crate 私有测试装配注入。
/// 执行控制单独分组，降低高元数参数错位风险。
///
/// @author kongweiguang
pub(super) async fn download_update(
    update: &Update,
    cancellation: CancellationToken,
    progress: watch::Sender<UpdateProgress>,
    max_bytes: u64,
    additional_trust_roots: &[Certificate],
) -> AppResult<Vec<u8>> {
    let request = VerifiedDownloadRequest {
        url: update.download_url.clone(),
        public_key: update.config.pubkey.clone(),
        signature: update.signature.clone(),
        headers: update.headers.clone(),
        timeout: update.timeout,
        max_bytes,
        additional_trust_roots,
    };
    let control = DownloadControl {
        cancellation,
        progress,
    };
    download_verified(request, control).await
}

/// 请求数据与运行控制分组传递；网络、取消、限额和 Minisign 验证流程保持原顺序。
///
/// @author kongweiguang
async fn download_verified(
    request: VerifiedDownloadRequest<'_>,
    control: DownloadControl,
) -> AppResult<Vec<u8>> {
    let VerifiedDownloadRequest {
        url,
        public_key,
        signature,
        mut headers,
        timeout,
        max_bytes,
        additional_trust_roots,
    } = request;
    let DownloadControl {
        cancellation,
        progress,
    } = control;
    ensure_not_cancelled(&cancellation)?;
    if max_bytes == 0 || !is_safe_url(&url) {
        return Err(AppError::invalid("更新下载地址或大小限制无效"));
    }
    let (public_key, signature) = decode_signature_material(&public_key, &signature)?;
    let has_sensitive_headers = [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION]
        .into_iter()
        .any(|header| headers.contains_key(header));
    if !headers.contains_key(ACCEPT) {
        headers.insert(
            ACCEPT,
            "application/octet-stream"
                .parse()
                .map_err(|_| AppError::infrastructure("更新下载请求头无效"))?,
        );
    }
    if !headers.contains_key(USER_AGENT) {
        headers.insert(
            USER_AGENT,
            "cargo-packager-updater"
                .parse()
                .map_err(|_| AppError::infrastructure("更新下载请求头无效"))?,
        );
    }

    let redirect_policy = Policy::custom(move |attempt| {
        if redirect_allowed(attempt.previous(), attempt.url(), has_sensitive_headers) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let mut builder = Client::builder();
    for certificate in additional_trust_roots {
        builder = builder.add_root_certificate(certificate.clone());
    }
    let client = builder
        .redirect(redirect_policy)
        .build()
        .map_err(|error| AppError::infrastructure_with_source("无法初始化更新下载连接", error))?;
    let request_timeout = timeout.unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT);
    let response = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AppError::cancelled("更新下载已取消")),
        response = client
            .get(url)
            .headers(headers)
            .timeout(request_timeout)
            .send() => response.map_err(|error| {
                AppError::infrastructure_with_source("更新包下载失败", error)
            })?,
    };
    ensure_not_cancelled(&cancellation)?;
    if !response.status().is_success() {
        return Err(AppError::infrastructure("更新服务器未能提供安装包"));
    }
    if !is_safe_url(response.url()) {
        return Err(AppError::infrastructure("更新下载跳转地址不安全"));
    }

    let total_bytes = response.content_length();
    if total_bytes.is_some_and(|total| total > max_bytes) {
        return Err(AppError::invalid("更新包超过允许的大小限制"));
    }
    progress.send_replace(UpdateProgress {
        received_bytes: 0,
        total_bytes,
    });

    let mut stream_verifier = match public_key.verify_stream(&signature) {
        Ok(verifier) => Some(verifier),
        Err(minisign_verify::Error::UnsupportedLegacyMode) => None,
        Err(error) => return Err(signature_error(error)),
    };
    let mut response = response;
    let mut bytes = Vec::new();
    let mut received_bytes = 0_u64;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("更新下载已取消")),
            chunk = response.chunk() => chunk
                .map_err(|error| {
                    AppError::infrastructure_with_source("更新包下载中断", error)
                })?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk_bytes = u64::try_from(chunk.len())
            .map_err(|_| AppError::invalid("更新包超过允许的大小限制"))?;
        received_bytes = received_bytes
            .checked_add(chunk_bytes)
            .ok_or_else(|| AppError::invalid("更新包超过允许的大小限制"))?;
        if received_bytes > max_bytes {
            return Err(AppError::invalid("更新包超过允许的大小限制"));
        }
        bytes
            .try_reserve(chunk.len())
            .map_err(|_| AppError::infrastructure("无法为更新包分配缓存"))?;
        if let Some(verifier) = stream_verifier.as_mut() {
            verifier.update(&chunk);
        }
        bytes.extend_from_slice(&chunk);
        progress.send_replace(UpdateProgress {
            received_bytes,
            total_bytes,
        });
    }
    ensure_not_cancelled(&cancellation)?;
    if bytes.is_empty() {
        return Err(AppError::infrastructure("更新服务器返回了空安装包"));
    }
    if total_bytes.is_some_and(|total| total != received_bytes) {
        return Err(AppError::infrastructure("更新包长度与服务器声明不一致"));
    }

    if let Some(mut verifier) = stream_verifier {
        verifier.finalize().map_err(signature_error)?;
        ensure_not_cancelled(&cancellation)?;
        return Ok(bytes);
    }

    let verification = tokio::task::spawn_blocking(move || {
        public_key
            .verify(&bytes, &signature, true)
            .map(|()| bytes)
            .map_err(signature_error)
    })
    .await
    .map_err(|_| AppError::infrastructure("更新签名验证任务异常退出"))?;
    ensure_not_cancelled(&cancellation)?;
    verification
}

/// 与 SDK 相同地解开外层 Base64 后交给 minisign-verify 解析，不实现密码算法或格式兼容层。
///
/// @author kongweiguang
fn decode_signature_material(
    public_key: &str,
    signature: &str,
) -> AppResult<(PublicKey, Signature)> {
    let public_key_text = decode_base64_text(public_key)?;
    let public_key = PublicKey::decode(&public_key_text).map_err(signature_error)?;
    let signature_text = decode_base64_text(signature)?;
    let signature = Signature::decode(&signature_text).map_err(signature_error)?;
    Ok((public_key, signature))
}

/// 将 SDK 的 Base64 信任配置恢复为 Minisign 文本，错误文案保持固定以免泄漏元数据。
///
/// @author kongweiguang
fn decode_base64_text(value: &str) -> AppResult<String> {
    let decoded = STANDARD.decode(value).map_err(signature_error)?;
    String::from_utf8(decoded).map_err(signature_error)
}

/// 映射第三方验签错误为固定公开文案，诊断 source 留在应用错误链内部。
fn signature_error<E>(error: E) -> AppError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AppError::infrastructure("更新包签名校验失败").with_source(error)
}

/// 只接受无凭据、无片段且具有主机名的 HTTPS 地址，测试信任根不会改变此约束。
fn is_safe_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// 拦截降级、凭据 URL、循环和携带敏感请求头的跨源跳转；无凭据的 HTTPS CDN 跳转可用。
fn redirect_allowed(previous: &[Url], target: &Url, has_sensitive_headers: bool) -> bool {
    if previous.len() >= MAX_REDIRECTS || !is_safe_url(target) {
        return false;
    }
    if previous.iter().any(|url| !is_safe_url(url)) {
        return false;
    }
    !has_sensitive_headers || previous.iter().all(|url| same_origin(url, target))
}

/// 用完整 scheme、host 和规范端口判断源站，防止跨源重定向携带认证头。
fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// 在每个网络边界前优先检查协作取消，保证无需请求时不创建客户端或打开连接。
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("更新下载已取消"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/support/updater_download_unit.rs"]
mod tests;
