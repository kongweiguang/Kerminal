//! Vault 迁移只补 AAD；应用自身的服务引用不能被七段 host 凭据规则误判为损坏。
//!
//! @author kongweiguang

use super::*;

use std::{fs, path::Path};

const HOST_ID: &str =
    "credential:kerminal:ssh-host:11111111-1111-4111-8111-111111111111:target:password:v1";
const SERVICE_ID: &str = "service:kerminal:mcp-token:v1";

/// 合成 vault 不包含可解密材料，只验证迁移元数据边界。
///
/// @author kongweiguang
fn write_vault(root: &Path, service_id: &str) -> std::path::PathBuf {
    let secrets = root.join("secrets");
    fs::create_dir_all(&secrets).expect("创建合成凭据目录");
    let source = format!(
        "schema_version = 1\n\n[[entries]]\nid = \"{HOST_ID}\"\nkind = \"ssh-password\"\nalgorithm = \"xchacha20poly1305\"\nkey_id = \"fixture-key\"\nnonce = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nciphertext = \"AAAAAAAAAAAAAAAAAAAAAA==\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nupdated_at = \"2026-01-01T00:00:00Z\"\n\n[[entries]]\nid = \"{service_id}\"\nkind = \"mcp-token\"\nalgorithm = \"xchacha20poly1305\"\nkey_id = \"fixture-key\"\nnonce = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nciphertext = \"AAAAAAAAAAAAAAAAAAAAAA==\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nupdated_at = \"2026-01-01T00:00:00Z\"\n"
    );
    let path = secrets.join("vault.toml");
    fs::write(&path, source).expect("写入合成 vault");
    path
}

/// 服务引用的 id 既是条目身份也是 AAD；迁移必须保留它并补齐 AAD，而不是阻断整库。
///
/// @author kongweiguang
#[test]
fn service_reference_receives_aad_without_rewriting_identity() {
    let temporary = tempfile::tempdir().expect("isolated vault root");
    let path = write_vault(temporary.path(), SERVICE_ID);
    let before = fs::read(&path).expect("读取原始 vault");

    let edits = plan_vault_migration(temporary.path()).expect("服务引用不应阻断迁移");
    assert_eq!(edits.len(), 1);
    assert_eq!(
        edits[0].relative_path,
        std::path::Path::new(VAULT_RELATIVE_PATH)
    );
    assert_eq!(edits[0].before.as_deref(), Some(before.as_slice()));

    let after = std::str::from_utf8(&edits[0].after).expect("合成 vault 是 UTF-8");
    let document: toml::Value = toml::from_str(after).expect("迁移结果是有效 TOML");
    let entries = document["entries"].as_array().expect("entries 数组");
    let expected_host = BASE64.encode(HOST_ID.as_bytes());
    let expected_service = BASE64.encode(SERVICE_ID.as_bytes());
    assert_eq!(entries[0]["id"].as_str(), Some(HOST_ID));
    assert_eq!(
        entries[0]["associated_data"].as_str(),
        Some(expected_host.as_str())
    );
    assert_eq!(entries[1]["id"].as_str(), Some(SERVICE_ID));
    assert_eq!(
        entries[1]["associated_data"].as_str(),
        Some(expected_service.as_str())
    );
}

/// 只接受应用定义的 service:kerminal:<name>:v1，其他前缀仍视为损坏并保留原文件。
///
/// @author kongweiguang
#[test]
fn non_application_service_reference_still_blocks_migration() {
    let temporary = tempfile::tempdir().expect("isolated vault root");
    write_vault(temporary.path(), "service:other:mcp-token:v1");

    let error = plan_vault_migration(temporary.path()).expect_err("未知服务引用必须阻断");
    assert_eq!(error.public_code().as_str(), "invalid");
}
