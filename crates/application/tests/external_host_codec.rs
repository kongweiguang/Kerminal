//! 静态主机 codec 使用冻结 schema，不需要插件进程参与配置校验。
//! @author kongweiguang

use kerminal_application::connections::host_types::{ExternalHostCodec, HostConfigurationCodec};
use kerminal_plugin_protocol::ConfigurationSchema;

/// 读取和写入均遵守同一 schema 与版本，防止非法配置绕过启动扫描。
#[test]
fn host_configuration_uses_frozen_schema() {
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"url":{"type":"string","pattern":"^https://"}},"required":["url"],"additionalProperties":false}"#).unwrap();
    let codec = ExternalHostCodec::new(schema, "acme.example").unwrap();
    let good: toml::Value = toml::from_str("url = 'https://example.com'\n").unwrap();
    let bad: toml::Value = toml::from_str("url = 'http://example.com'\n").unwrap();
    assert_eq!(codec.decode(1, &good).unwrap(), good);
    assert_eq!(codec.encode(1, &good).unwrap(), good);
    assert_eq!(codec.credential_references(1, &good).unwrap(), Some(vec![]));
    assert!(codec.decode(1, &bad).is_err());
    assert!(codec.encode(2, &good).is_err());
}

/// 即使秘密注解藏在引用目标中，也不能在 Vault 接线完成前把明文持久化。
#[test]
fn secret_schema_cannot_be_persisted_without_vault() {
    let schema = ConfigurationSchema::parse(br##"{"type":"object","properties":{"token":{"$ref":"#/$defs/secret"}},"$defs":{"secret":{"type":"string","x-secret":true}}}"##).unwrap();
    let codec = ExternalHostCodec::new(schema, "acme.example").unwrap();
    let value: toml::Value = toml::from_str("token = 'must-not-be-written'\n").unwrap();
    assert!(codec.encode(1, &value).is_err());
}

/// 静态扫描只接受当前插件的 Vault 引用，同时保留普通字段的 schema 校验。
#[test]
fn secret_reference_is_validated_without_decrypting() {
    let schema = ConfigurationSchema::parse(br#"{"type":"object","properties":{"token":{"type":"string","minLength":4,"x-secret":true},"url":{"type":"string","pattern":"^https://"}},"required":["token","url"]}"#).unwrap();
    let codec = ExternalHostCodec::new(schema, "acme.example").unwrap();
    let reference = format!(
        "credential:kerminal-plugin:acme.example:settings:{}:00000000-0000-4000-8000-000000000000:v1",
        "a".repeat(64)
    );
    let good: toml::Value = toml::from_str(&format!(
        "token = '{reference}'\nurl = 'https://example.com'\n"
    ))
    .unwrap();
    assert_eq!(codec.decode(1, &good).unwrap(), good);
    assert_eq!(
        codec
            .credential_references(1, &good)
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    let other: toml::Value = toml::from_str(&format!(
        "token = '{}'\nurl = 'https://example.com'\n",
        reference.replace("acme.example", "other.example")
    ))
    .unwrap();
    assert!(codec.decode(1, &other).is_err());
}
