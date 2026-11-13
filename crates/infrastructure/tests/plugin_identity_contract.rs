//! 对照宿主 domain 身份门禁与 SDK 清单校验，保证两侧保留名与语法判定完全一致。
//! 该测试只依赖既有 workspace 依赖，不需要 SDK 借用 domain crate。
//! @author kongweiguang

use serde_json::{Value, json};

use kerminal_domain::plugins::PluginId;
use kerminal_plugin_protocol::{ManifestPolicy, ProtocolError, parse_manifest};

/// 以仓库合法样本为底稿改写身份，避免无效样本因其他字段提前失败掩盖身份差异。
fn manifest_with_id(id: &str) -> Value {
    let mut manifest: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/plugins/manifests/valid.json"
    ))
    .unwrap();
    manifest["id"] = json!(id);
    // publisher 必须与 id 的首段一致，否则样本会因 publisher 不匹配被拒，无法观察保留名规则。
    let publisher = id.split('.').next().unwrap_or(id);
    manifest["publisher"] = json!(publisher);
    manifest
}

/// SDK 清单解析入口与安装器共用，agent 用它构建市场条目前必须与宿主同判。
fn sdk_accepts(id: &str) -> bool {
    parse_manifest(
        &serde_json::to_vec(&manifest_with_id(id)).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .is_ok()
}

/// 保留名与语法属于身份层，与来源信任无关；用 Official 策略单独观察，
/// 避免 `kerminal.*` 被官方命名空间策略挡住而误判为身份规则差异。
fn sdk_identity_accepts(id: &str) -> bool {
    parse_manifest(
        &serde_json::to_vec(&manifest_with_id(id)).unwrap(),
        ManifestPolicy::Official,
    )
    .is_ok()
}

/// 官方命名空间拒绝必须来自策略分支本身；按错误位置与原因区分身份规则拒绝，
/// 否则测试会在别的字段或保留名先失败时误报通过。
fn sdk_third_party_policy_rejection(id: &str) -> bool {
    matches!(
        parse_manifest(
            &serde_json::to_vec(&manifest_with_id(id)).unwrap(),
            ManifestPolicy::ThirdParty,
        ),
        Err(ProtocolError::Invalid { location, reason })
            if location == "manifest.id" && reason.contains("官方命名空间")
    )
}

/// 宿主侧存储身份直接复用 domain 的构造门禁，是对照的基准。
fn host_accepts(id: &str) -> bool {
    PluginId::parse(id).is_ok()
}

#[test]
/// 官方命名空间是来源信任判断，与保留名/语法这层身份规则互不覆盖：`kerminal`
/// 不是设备名，domain 与 SDK 身份层都接受 `kerminal.con`，Official 策略下清单
/// 也通过；只有 ThirdParty 会因策略拒绝，且拒绝必须来自 `manifest.id` 的官方
/// 命名空间分支，而不是保留名或别的字段先失败。
fn official_namespace_ids_pass_identity_on_both_sides() {
    for id in ["kerminal.con", "kerminal.notes"] {
        assert!(host_accepts(id), "host official namespace: {id}");
        assert!(sdk_identity_accepts(id), "sdk identity accepts: {id}");
        assert!(
            sdk_third_party_policy_rejection(id),
            "sdk third-party policy rejects: {id}"
        );
    }
}

#[test]
/// 宿主与 SDK 对 Windows 保留设备名只看第一个点前的段，判定必须逐样本一致。
fn reserved_device_names_are_rejected_identically_on_both_sides() {
    let reserved = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "conin$",
        "conout$",
    ];
    for publisher in reserved {
        let id = format!("{publisher}.notes");
        assert!(!host_accepts(&id), "host reserved: {id}");
        assert!(!sdk_accepts(&id), "sdk reserved: {id}");
    }
}

#[test]
/// 宿主状态文件命名会与 `plugins/<id>` 目录内容冲突，两侧都必须拒绝。
fn host_state_file_names_are_rejected_identically_on_both_sides() {
    for id in [
        "state.json",
        "state-recovery-1.json",
        "state-recovery-abort.json",
        "state-recovery-.json",
    ] {
        assert!(!host_accepts(id), "host state file: {id}");
        assert!(!sdk_accepts(id), "sdk state file: {id}");
    }
}

#[test]
/// 保留名只看第一个点前的段：设备名落在 name 位或只作前缀时仍是合法身份；
/// 不在任一侧保留列表里的相近名字（如 com0）不能被任何一侧额外收紧。
fn legal_ids_near_reserved_names_are_accepted_identically_on_both_sides() {
    for id in [
        "acme.notes",
        "acme.con",
        "acme.aux",
        "acme.com1",
        "acme.lpt9",
        "console.notes",
        "auxiliary.notes",
        "null-safe.notes",
        "companion.notes",
        "com0.notes",
        "lpt0.notes",
        "stateless.json",
        "state-recovery.json",
    ] {
        assert!(host_accepts(id), "host legal: {id}");
        assert!(sdk_accepts(id), "sdk legal: {id}");
    }
}

#[test]
/// 语法边界（长度、字符集、段数）在两侧共用同一结果，差异只允许来自保留名规则。
fn syntax_boundaries_are_judged_identically_on_both_sides() {
    let cases = [
        ("a.b", false),                                            // 段长度小于 2
        ("ab.cd", true),                                           // 两段都取最小长度
        ("acme.notes.extra", false),                               // 超过两段
        ("notes", false),                                          // 缺少点分隔
        ("Acme.notes", false),                                     // 大写进入身份
        ("acme.Notes", false),                                     // name 位大写
        ("-cme.notes", false),                                     // publisher 首字符不能是连字符
        ("acme.-otes", false),                                     // name 首字符不能是连字符
        ("acm3.n0tes", true),                                      // 数字开头合法
        ("acme_n.notes", false),                                   // 下划线不在身份字符集
        ("con$$.notes", false),                                    // $ 不在身份字符集
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.notes", true),   // publisher 取 39 字符上限
        ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.notes", false), // publisher 超过 39 字符
        (
            "acme.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            true,
        ), // name 取 63 字符上限
        (
            "acme.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            false,
        ), // name 超过 63 字符
    ];
    for (id, expected) in cases {
        assert_eq!(host_accepts(id), expected, "host: {id}");
        assert_eq!(sdk_accepts(id), expected, "sdk: {id}");
    }
}
