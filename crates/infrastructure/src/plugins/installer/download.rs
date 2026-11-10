//! 可中断下载只写私有暂存文件；最终信任仍由签名目录的大小和摘要决定。
//! @author kongweiguang

use kerminal_application::{
    AppError, AppResult,
    extensions::{CatalogAsset, InstallProgress},
};
use reqwest::{
    Client,
    header::{CONTENT_RANGE, ETAG, IF_RANGE, RANGE},
    redirect,
};
use std::{path::Path, time::Duration};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::super::catalog::allowed_url;

/// 中断后只续接服务器确认的相同字节范围；不支持 Range 时从零重新写入。
pub(super) async fn download(
    asset: &CatalogAsset,
    path: &Path,
    allow_loopback: bool,
    progress: &tokio::sync::watch::Sender<InstallProgress>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    if !allowed_url(&asset.url, allow_loopback) {
        return Err(AppError::invalid("插件资产地址不受信任"));
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 3 || !allowed_url(attempt.url().as_str(), allow_loopback)
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| AppError::infrastructure_with_source("创建插件下载客户端失败", e))?;
    let mut offset = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => return Err(AppError::invalid("插件下载暂存路径不安全")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            return Err(AppError::infrastructure_with_source(
                "检查插件下载暂存失败",
                e,
            ));
        }
    };
    if offset > asset.size {
        return Err(AppError::invalid("插件暂存超过目录大小"));
    }
    let mut strong_etag = None;
    for _ in 0..4 {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件下载已取消"));
        }
        progress.send_replace(InstallProgress::Downloading {
            received: offset,
            total: asset.size,
        });
        if offset == asset.size {
            return Ok(());
        }
        let mut request = client.get(&asset.url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
            if let Some(etag) = &strong_etag {
                request = request.header(IF_RANGE, etag);
            }
        }
        let mut response = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("插件下载已取消")),
            result = request.send() => match result { Ok(response) => response, Err(_) => continue },
        };
        let response_etag = response
            .headers()
            .get(ETAG)
            .filter(|value| {
                value
                    .to_str()
                    .is_ok_and(|text| text.starts_with('"') && text.ends_with('"'))
            })
            .cloned();
        let mut response_end = asset.size;
        let append = if offset == 0 {
            if response.status() != reqwest::StatusCode::OK {
                return Err(AppError::invalid("插件下载响应无效"));
            }
            false
        } else if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let expected_prefix = format!("bytes {offset}-");
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| AppError::invalid("插件续传范围缺失"))?;
            let tail = range
                .strip_prefix(&expected_prefix)
                .and_then(|value| value.split_once('/'))
                .ok_or_else(|| AppError::invalid("插件续传范围无效"))?;
            let end = tail
                .0
                .parse::<u64>()
                .map_err(|_| AppError::invalid("插件续传范围无效"))?;
            let total = tail
                .1
                .parse::<u64>()
                .map_err(|_| AppError::invalid("插件续传范围无效"))?;
            if total != asset.size || end < offset || end >= total {
                return Err(AppError::invalid("插件续传范围不匹配"));
            }
            if strong_etag.is_some() && response_etag != strong_etag {
                return Err(AppError::invalid("插件续传版本标识不匹配"));
            }
            response_end = end + 1;
            true
        } else if response.status() == reqwest::StatusCode::OK {
            offset = 0;
            false
        } else {
            return Err(AppError::invalid("插件续传响应无效"));
        };
        if !append {
            strong_etag = response_etag;
        }
        let mut output = OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .await
            .map_err(|e| AppError::infrastructure_with_source("打开插件下载暂存失败", e))?;
        let mut interrupted = false;
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件下载已取消")),
                result = response.chunk() => match result { Ok(chunk) => chunk, Err(_) => { interrupted = true; None } },
            };
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.len() as u64 > response_end.saturating_sub(offset) {
                return Err(AppError::invalid("插件下载超过目录大小"));
            }
            output
                .write_all(&chunk)
                .await
                .map_err(|e| AppError::infrastructure_with_source("写入插件下载失败", e))?;
            offset += chunk.len() as u64;
            progress.send_replace(InstallProgress::Downloading {
                received: offset,
                total: asset.size,
            });
        }
        output
            .flush()
            .await
            .map_err(|e| AppError::infrastructure_with_source("写入插件下载失败", e))?;
        if !interrupted && offset != response_end {
            return Err(AppError::invalid("插件下载响应长度不匹配"));
        }
        progress.send_replace(InstallProgress::Downloading {
            received: offset,
            total: asset.size,
        });
        if offset == asset.size {
            output
                .sync_all()
                .await
                .map_err(|e| AppError::infrastructure_with_source("同步插件下载失败", e))?;
            return Ok(());
        }
    }
    Err(AppError::infrastructure("插件下载中断，重试次数已用尽"))
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_download.rs"]
mod tests;
