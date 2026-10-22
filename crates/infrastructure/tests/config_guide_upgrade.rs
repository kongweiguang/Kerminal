//! 配置指南按冻结正文摘要升级并保护用户改写内容的独立回归契约。
//!
//! @author kongweiguang

use std::fs;

use kerminal_application::preferences::ConfigurationGuideStatus;
use kerminal_infrastructure::configuration::ConfigurationStore;
use sha2::{Digest, Sha256};

const CONFIG_GUIDE_V2: &str =
    include_str!("../../../tests/fixtures/configuration/config-guide-v2.md");

/// 运行时不再迁移历史指南；旧正文、用户修改内容和未来 revision 都保持原字节。
///
/// @author kongweiguang
#[test]
fn runtime_preserves_old_custom_and_future_generated_guides() {
    let normalized_digest = |source: &str| {
        let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
        Sha256::digest(normalized.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };

    let trusted_root = tempfile::tempdir().expect("创建隔离的旧指南配置根");
    let trusted_digest = normalized_digest(CONFIG_GUIDE_V2);
    let trusted_contents = format!(
        "<!-- kerminal-generated-config-guide revision=2 sha256={trusted_digest} -->\n{CONFIG_GUIDE_V2}"
    );
    let trusted_path = trusted_root.path().join("kerminal-config.md");
    fs::write(&trusted_path, trusted_contents.as_bytes()).expect("写入冻结 v2 指南");

    let trusted_store =
        ConfigurationStore::initialize(trusted_root.path()).expect("初始化旧生成指南");
    assert_eq!(
        trusted_store.configuration_guide_status(),
        ConfigurationGuideStatus::UserModified
    );
    assert_eq!(
        fs::read(&trusted_path).expect("读取保留的旧指南"),
        trusted_contents.as_bytes()
    );

    let modified_root = tempfile::tempdir().expect("创建隔离的用户修改配置根");
    let modified_body =
        CONFIG_GUIDE_V2.replacen("长期设置和用户维护清单", "长期设置与用户维护清单", 1);
    assert_ne!(modified_body, CONFIG_GUIDE_V2, "fixture 修改点必须存在");
    let modified_digest = normalized_digest(&modified_body);
    let modified_contents = format!(
        "<!-- kerminal-generated-config-guide revision=2 sha256={modified_digest} -->\n{modified_body}"
    );
    let modified_path = modified_root.path().join("kerminal-config.md");
    fs::write(&modified_path, modified_contents.as_bytes()).expect("写入用户修改的 v2 指南");

    let modified_store =
        ConfigurationStore::initialize(modified_root.path()).expect("识别并保留用户修改指南");
    assert_eq!(
        modified_store.configuration_guide_status(),
        ConfigurationGuideStatus::UserModified
    );
    assert_eq!(
        fs::read(&modified_path).expect("复读被保护的指南"),
        modified_contents.as_bytes()
    );
}
