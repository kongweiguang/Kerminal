//! 有界、可取消的 updater metadata 适配；安装仍由 cargo-packager-updater 执行。
//!
//! endpoint 占位符与安装路径遵循固定依赖 `cargo-packager-updater 0.2.3` 的公开行为，
//! 不复制其实现；该依赖声明 Apache-2.0 OR MIT。metadata 使用 SDK 的公开 RemoteRelease 类型。
//!
//! @author kongweiguang

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cargo_packager_updater::{RemoteRelease, Update, reqwest};
use kerminal_application::{AppError, AppResult};
use reqwest::{
    Certificate, Client, ClientBuilder, Response, Url,
    header::{ACCEPT, HeaderValue},
    redirect::Policy,
};
use semver::Version;
use tokio_util::sync::CancellationToken;

use super::settings::UpdaterSettings;

/// 单个 release manifest 足以描述全部受支持平台，不接受无界的远端 JSON 响应。
const MAX_METADATA_BODY_BYTES: u64 = 1024 * 1024;

/// 单次检查所有 endpoint 共用绝对期限，为应用关闭清理留下时间。
const METADATA_CHECK_DEADLINE: Duration = Duration::from_secs(3);

/// 限制重定向次数，并拒绝从 HTTPS feed 跳转到不安全地址。
const MAX_METADATA_REDIRECTS: usize = 10;

/// 按 SDK 当前 target 规则逐个读取 endpoint，并生成后续由 SDK 安装的 Update。
pub(super) async fn check(
    settings: &UpdaterSettings,
    cancellation: CancellationToken,
    additional_trust_roots: &[Certificate],
) -> AppResult<Option<Update>> {
    let deadline = Instant::now() + METADATA_CHECK_DEADLINE;
    let (target, json_target, arch) = current_target()?;
    let client = build_client(additional_trust_roots)?;
    let current_version = settings.current_version().clone();
    let mut last_error = None;

    for endpoint in settings.endpoints() {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("检查更新已取消"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let endpoint = resolve_endpoint(endpoint, &current_version, &target, &arch)?;
        let response = fetch_release(
            &client,
            endpoint,
            remaining,
            cancellation.clone(),
            MAX_METADATA_BODY_BYTES,
        )
        .await;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("检查更新已取消"));
        }
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(AppError::infrastructure("检查更新超过总时限"));
        }

        let release = match response {
            Ok(Some(release)) => release,
            Ok(None) => return Ok(None),
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        if !release_is_newer(&current_version, &release) {
            return Ok(None);
        }

        return update_from_release(settings, release, current_version, &target, &json_target)
            .map(Some);
    }

    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("检查更新已取消"));
    }
    match last_error {
        Some(error) => Err(error),
        None => Err(AppError::infrastructure("检查更新超过总时限")),
    }
}

/// 以复用客户端访问 metadata，并在状态码、声明长度和每个响应块处落实边界。
async fn fetch_release(
    client: &Client,
    url: Url,
    timeout: Duration,
    cancellation: CancellationToken,
    max_bytes: u64,
) -> AppResult<Option<RemoteRelease>> {
    if !is_safe_metadata_url(&url) || max_bytes == 0 {
        return Err(AppError::invalid("更新 metadata 地址或大小限制无效"));
    }
    let response = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AppError::cancelled("检查更新已取消")),
        response = client
            .get(url)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .timeout(timeout)
            .send() => response.map_err(|error| {
                AppError::infrastructure_with_source("更新源暂时无法访问", error)
            })?,
    };
    ensure_not_cancelled(&cancellation)?;
    read_release_response(response, cancellation, max_bytes).await
}

/// 读取 SDK manifest 前检查响应体上限，避免 reqwest JSON helper 无界分配。
async fn read_release_response(
    response: Response,
    cancellation: CancellationToken,
    max_bytes: u64,
) -> AppResult<Option<RemoteRelease>> {
    if !is_safe_metadata_url(response.url()) {
        return Err(AppError::infrastructure("更新源重定向地址不安全"));
    }
    read_release_body(response, cancellation, max_bytes).await
}

/// 读取响应体时按 chunk 累加限制；由调用边界负责校验生产网络 URL。
async fn read_release_body(
    mut response: Response,
    cancellation: CancellationToken,
    max_bytes: u64,
) -> AppResult<Option<RemoteRelease>> {
    if max_bytes == 0 {
        return Err(AppError::invalid("更新 metadata 大小限制无效"));
    }
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(AppError::infrastructure("更新源未能提供版本信息"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(AppError::invalid("更新 metadata 超过允许的大小限制"));
    }

    let mut body = Vec::new();
    let mut received_bytes = 0_u64;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("检查更新已取消")),
            chunk = response.chunk() => chunk
                .map_err(|error| {
                    AppError::infrastructure_with_source("更新 metadata 读取中断", error)
                })?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk_bytes = u64::try_from(chunk.len())
            .map_err(|_| AppError::invalid("更新 metadata 超过允许的大小限制"))?;
        received_bytes = received_bytes
            .checked_add(chunk_bytes)
            .ok_or_else(|| AppError::invalid("更新 metadata 超过允许的大小限制"))?;
        if received_bytes > max_bytes {
            return Err(AppError::invalid("更新 metadata 超过允许的大小限制"));
        }
        body.try_reserve(chunk.len())
            .map_err(|_| AppError::infrastructure("无法为更新 metadata 分配缓存"))?;
        body.extend_from_slice(&chunk);
    }
    ensure_not_cancelled(&cancellation)?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| AppError::infrastructure_with_source("更新源版本信息无效", error))
}

/// 对照 SDK 默认比较器判断新版本，避免引入另一套版本排序规则。
fn release_is_newer(current_version: &Version, release: &RemoteRelease) -> bool {
    &release.version > current_version
}

/// 给生产默认客户端或私有测试信任根安装同一 HTTPS redirect policy。
fn build_client(additional_trust_roots: &[Certificate]) -> AppResult<Client> {
    build_client_from_builder(Client::builder(), additional_trust_roots)
}

/// 仅注入证书信任材料；请求、跳转与最终响应仍由统一 HTTPS 策略校验。
fn build_client_from_builder(
    mut builder: ClientBuilder,
    additional_trust_roots: &[Certificate],
) -> AppResult<Client> {
    for certificate in additional_trust_roots {
        builder = builder.add_root_certificate(certificate.clone());
    }
    let redirect_policy = Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_METADATA_REDIRECTS
            || !is_safe_metadata_url(attempt.url())
            || attempt
                .previous()
                .iter()
                .any(|url| !is_safe_metadata_url(url))
        {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    builder
        .redirect(redirect_policy)
        .build()
        .map_err(|error| AppError::infrastructure_with_source("无法初始化更新检查连接", error))
}

/// 构造 SDK 同款 OS、OS-ARCH 与 SemVer 三种 endpoint 模板值。
fn current_target() -> AppResult<(String, String, String)> {
    let json_target = cargo_packager_updater::target()
        .ok_or_else(|| AppError::unsupported("当前系统无法确定更新目标"))?;
    let (target, arch) = json_target
        .split_once('-')
        .ok_or_else(|| AppError::infrastructure("更新目标标识格式无效"))?;
    let target = target.to_owned();
    let arch = arch.to_owned();
    Ok((target, json_target, arch))
}

/// 仅替换 SDK 支持的三个占位符；有效 SemVer 中只有 build metadata 的加号需转义。
fn resolve_endpoint(
    endpoint: &Url,
    current_version: &Version,
    target: &str,
    arch: &str,
) -> AppResult<Url> {
    let encoded_version = current_version.to_string().replace('+', "%2B");
    let endpoint = endpoint
        .to_string()
        .replace("%7B%7Bcurrent_version%7D%7D", &encoded_version)
        .replace("%7B%7Btarget%7D%7D", target)
        .replace("%7B%7Barch%7D%7D", arch)
        .replace("{{current_version}}", &encoded_version)
        .replace("{{target}}", target)
        .replace("{{arch}}", arch);
    Url::parse(&endpoint).map_err(|_| AppError::invalid("更新地址模板无效"))
}

/// 把公开 SDK RemoteRelease 映射成其公开 Update 契约，继续调用 SDK 平台安装器。
fn update_from_release(
    settings: &UpdaterSettings,
    release: RemoteRelease,
    current_version: Version,
    target: &str,
    json_target: &str,
) -> AppResult<Update> {
    let download_url = release
        .download_url(json_target)
        .map_err(|error| AppError::infrastructure_with_source("更新源缺少当前平台产物", error))?
        .clone();
    let signature = release
        .signature(json_target)
        .map_err(|error| AppError::infrastructure_with_source("更新源缺少当前平台签名", error))?
        .clone();
    let format = release
        .format(json_target)
        .map_err(|error| AppError::infrastructure_with_source("更新源缺少当前平台格式", error))?;
    let extract_path = current_install_path()?;

    Ok(Update {
        config: settings.config(),
        body: bounded_notes(release.notes),
        current_version: current_version.to_string(),
        version: release.version.to_string(),
        date: release.pub_date,
        target: target.to_owned(),
        extract_path,
        download_url,
        signature,
        timeout: Some(settings.download_timeout()),
        headers: reqwest::header::HeaderMap::new(),
        format,
    })
}

/// 保持当前 SDK 的平台安装目标约定；DEB 已在 settings 层拒绝应用内更新。
fn current_install_path() -> AppResult<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(appimage));
    }

    let executable = std::env::current_exe()
        .map_err(|error| AppError::infrastructure_with_source("应用安装位置不可用", error))?;
    #[cfg(target_os = "macos")]
    {
        let executable_directory = executable
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| AppError::infrastructure("应用安装位置不可用"))?;
        let has_macos_bundle_layout = executable_directory
            .file_name()
            .is_some_and(|name| name == "MacOS")
            && executable_directory
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "Contents");
        if has_macos_bundle_layout {
            return executable_directory
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or_else(|| AppError::infrastructure("应用安装位置不可用"));
        }
    }

    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| AppError::infrastructure("应用安装位置不可用"))
}

/// 用 SemVer 边界截断 notes，限制进入 GPUI 卡片的远端长文本。
fn bounded_notes(notes: Option<String>) -> Option<String> {
    notes.and_then(|mut value| {
        if value.is_empty() {
            return None;
        }
        const MAX_NOTES_BYTES: usize = 16 * 1024;
        if value.len() > MAX_NOTES_BYTES {
            let mut end = MAX_NOTES_BYTES;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        Some(value)
    })
}

/// 同步检查更新任务在网络操作前后响应取消，避免已取消检查继续创建候选。
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("检查更新已取消"))
    } else {
        Ok(())
    }
}

/// metadata endpoint 与跳转都必须使用无凭据 HTTPS，保留签名校验之外的传输保护。
fn is_safe_https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// Metadata 与重定向目标只接受无凭据 HTTPS；本地测试 CA 只影响证书信任链。
fn is_safe_metadata_url(url: &Url) -> bool {
    is_safe_https_url(url)
}

#[cfg(test)]
#[path = "../../../tests/support/updater_metadata_unit.rs"]
mod tests;
