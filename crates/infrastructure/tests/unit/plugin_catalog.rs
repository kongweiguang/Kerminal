//! 签名目录及缓存防回退的隔离回归。
//! @author kongweiguang

use super::*;
use serde_json::Value;

impl CatalogClient {
    /// 单元测试使用一次性密钥和本地服务，不修改环境变量或正式信任根。
    /// 可见范围限于插件模块，以供安装流程及恢复等兄弟测试复用。
    ///
    /// @author kongweiguang
    pub(in crate::plugins) fn testing(
        storage: StorageCoordinator,
        url: String,
        key: String,
    ) -> Self {
        Self {
            storage,
            url,
            key,
            allow_loopback: true,
        }
    }
}

/// 固定测试密钥只覆盖离线签名样本，与正式信任根完全隔离。
fn fixture() -> (tempfile::TempDir, CatalogClient, Vec<Cache>) {
    let root = tempfile::tempdir().expect("temporary root");
    let document: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/plugin-catalog-p5.json"
    ))
    .expect("fixture JSON");
    let client = CatalogClient {
        storage: StorageCoordinator::new(root.path()),
        url: "http://127.0.0.1/catalog.json".into(),
        key: document["publicKey"].as_str().expect("key").into(),
        allow_loopback: true,
    };
    let caches = document["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .map(|entry| Cache {
            raw: entry["raw"].as_str().expect("raw").into(),
            signature: entry["signature"].as_str().expect("signature").into(),
            fetched_at: 1,
        })
        .collect();
    (root, client, caches)
}

/// 连续提交覆盖 Windows 暂存句柄生命周期，签名错误、回退和同序列异文不能覆盖缓存。
#[test]
fn signed_cache_rejects_tampering_and_sequence_rollback() {
    let (_root, client, caches) = fixture();
    assert_eq!(client.persist(&caches[0]).expect("first").sequence, 1);
    assert_eq!(client.persist(&caches[1]).expect("second").sequence, 2);
    assert!(client.persist(&caches[0]).is_err());
    assert!(client.persist(&caches[2]).is_err());
    let tampered = Cache {
        raw: caches[1].raw.replace('2', "3"),
        signature: caches[1].signature.clone(),
        fetched_at: 2,
    };
    assert!(client.persist(&tampered).is_err());
    let path = client
        .storage
        .root()
        .join("plugins/catalog-cache/official.json");
    let cached = client
        .read_cache_at(&path)
        .expect("cache")
        .expect("present");
    assert_eq!(client.verify(&cached).expect("verified").sequence, 2);
}

/// Windows 替换被外部句柄阻塞时保留签名暂存与原缓存，调用方可先回读再决定后续动作。
#[cfg(windows)]
#[test]
fn blocked_replacement_preserves_staging_for_unknown_commit() {
    use std::os::windows::fs::OpenOptionsExt;

    let (_root, client, caches) = fixture();
    client.persist(&caches[0]).expect("first");
    let directory = client.storage.root().join("plugins/catalog-cache");
    let path = directory.join("official.json");
    let _external_reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&path)
        .expect("reader without delete sharing");

    let error = client.persist(&caches[1]).expect_err("blocked replacement");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let source = std::error::Error::source(&error)
        .and_then(std::error::Error::source)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("platform error remains available");
    assert_eq!(source.raw_os_error(), Some(32));
    let current = client
        .read_cache_at(&path)
        .expect("readback")
        .expect("cache");
    assert_eq!(
        client.verify(&current).expect("current signature").sequence,
        1
    );

    let staged: Vec<_> = fs::read_dir(&directory)
        .expect("cache directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|entry| entry != &path)
        .collect();
    assert_eq!(
        staged.len(),
        1,
        "unknown commit must retain one staging file"
    );
    let pending = client
        .read_cache_at(&staged[0])
        .expect("staged readback")
        .expect("staged cache");
    assert_eq!(
        client.verify(&pending).expect("staged signature").sequence,
        2
    );
}

/// 资产域名、端口和用户信息均逐项校验，README 的 Raw 例外不作用于资产。
#[test]
fn asset_and_document_url_boundaries_are_distinct() {
    assert!(allowed_url(
        "https://github.com/vendor/project/releases/download/v1/pkg",
        false
    ));
    assert!(allowed_url(
        "https://release-assets.githubusercontent.com/pkg",
        false
    ));
    assert!(!allowed_url(
        "https://release-assets.githubusercontent.com.evil.test/pkg",
        false
    ));
    assert!(!allowed_url(
        "http://release-assets.githubusercontent.com/pkg",
        false
    ));
    assert!(!allowed_url("https://github.com.evil.test/pkg", false));
    assert!(!allowed_url("https://github.com@evil.test/pkg", false));
    assert!(!allowed_url("https://github.com:444/pkg", false));
    assert!(!allowed_url("http://127.0.0.1/pkg", false));
    assert!(allowed_url("http://127.0.0.1/pkg", true));
    assert!(!allowed_url(
        "https://raw.githubusercontent.com/a/b/main/readme.md",
        false
    ));
    assert!(allowed_fetch_url(
        "https://raw.githubusercontent.com/a/b/main/readme.md",
        false,
        true
    ));
}
