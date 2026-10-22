//! Host schema v3 串口格式的配置仓储与迁移闭环。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf, sync::Arc};

use kerminal_application::{
    connections::management::{
        HostAuthenticationMode, HostDraft, HostManagementRepository, HostManagementService,
        HostSftpOptions,
    },
    data_migration::{DataMigrationOutcome, DataMigrationRepository},
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
    preferences::ConfigurationMode,
    sessions::{
        SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
    },
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
    data_migration::FileDataMigrationRepository,
};
use tempfile::tempdir;

const LEGACY_SERIAL_HOST: &str =
    include_str!("../../../tests/fixtures/configuration/host-schema-v2-serial-migration.toml");
const CURRENT_SERIAL_HOST: &str =
    include_str!("../../../tests/fixtures/configuration/host-schema-v3-serial.toml");

/// 全部高级字段走真实配置 CAS 及 reader，再次编辑继续携带原值，不把回显当作实际持久化。
///
/// @author kongweiguang
#[tokio::test]
async fn host_advanced_options_round_trip_through_current_configuration() {
    let root = tempdir().expect("隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化当前配置");
    let repository = HostConfigurationRepository::new(store);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let id = HostId::parse("host-options").expect("测试 ID 合法");
    let mut draft = serial_draft(id.clone(), Vec::new());
    draft.protocol = RemoteProtocol::Ssh;
    draft.serial = None;
    draft.endpoint = HostEndpoint {
        host: "example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    draft.connect_timeout_ms = 45_000;
    draft.keepalive_interval_ms = None;
    draft.terminal_options.term = Some("screen-256color".to_owned());
    draft.terminal_options.initial_working_directory = Some("/srv/work".to_owned());
    draft.note = Some("构建节点用途\n维护联系人：发布团队".to_owned());
    draft.sftp_options.local_start_directory = Some(root.path().to_path_buf());
    draft.sftp_options.remote_start_directory = Some("/srv/files".to_owned());
    draft.sftp_options.enabled = false;
    draft.sftp_options.preserve_mtime = false;
    draft.sftp_options.follow_source_symlinks = true;
    draft.sftp_options.concurrency = Some(3);
    draft.rdp_display.fullscreen = false;
    draft.rdp_display.width = Some(1440);
    draft.rdp_display.height = Some(900);
    let saved = service
        .save_host(draft.clone(), None)
        .await
        .expect("保存所有高级项");
    assert_eq!(saved.connect_timeout_ms, 45_000);
    assert_eq!(saved.keepalive_interval_ms, None);
    assert_eq!(saved.terminal_options, draft.terminal_options);
    assert_eq!(saved.sftp_options, draft.sftp_options);
    assert_eq!(saved.rdp_display, draft.rdp_display);
    assert_eq!(saved.note, draft.note);
    let path = root.path().join("hosts/host-options.toml");
    let raw = fs::read_to_string(&path).expect("读取真实 TOML");
    let parsed: toml::Value = toml::from_str(&raw).expect("有效 TOML");
    assert_eq!(
        parsed["ssh_options"]["terminal"]["connect_timeout_seconds"].as_integer(),
        Some(45)
    );
    assert_eq!(
        parsed["ssh_options"]["terminal"]["keepalive_seconds"].as_integer(),
        Some(0)
    );
    assert_eq!(
        parsed["ssh_options"]["sftp"]["concurrency"].as_integer(),
        Some(3)
    );
    assert_eq!(parsed["rdp_options"]["width"].as_integer(), Some(1440));
    assert_eq!(parsed["note"].as_str(), draft.note.as_deref());
    draft.name = "Renamed without losing options".to_owned();
    let updated = service
        .save_host(draft, Some(saved.revision.clone()))
        .await
        .expect("CAS 更新");
    let readback = repository
        .load_host(&id)
        .await
        .expect("当前 reader 可读")
        .expect("主机存在");
    assert_eq!(readback, updated);

    for broken in [
        raw.replace("concurrency = 3", "concurrency = 0"),
        raw.replace("width = 1440", "width = 1"),
        raw.replace("keepalive_seconds = 0", "keepalive_seconds = 601"),
    ] {
        fs::write(&path, &broken).expect("写隔离非法样本");
        let invalid = ConfigurationStore::initialize(root.path()).expect("恢复态可打开");
        assert_eq!(
            invalid.recovery_state().mode,
            ConfigurationMode::ReadOnlyRecovery
        );
        assert_eq!(fs::read_to_string(&path).expect("原值保留"), broken);
    }
}

/// 验证当前 TOML 缺少新增 SFTP 字段时按安全默认值读取，且测试主机不含凭据材料。
///
/// @author kongweiguang
#[tokio::test]
async fn omitted_sftp_options_read_as_current_defaults() {
    let root = tempdir().expect("隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化当前配置");
    let repository = HostConfigurationRepository::new(store);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let host_id = HostId::parse("sftp-defaults").expect("合成主机 ID 合法");
    let mut draft = serial_draft(host_id.clone(), Vec::new());
    draft.name = "Synthetic SSH host".to_owned();
    draft.protocol = RemoteProtocol::Ssh;
    draft.serial = None;
    draft.endpoint = HostEndpoint {
        host: "example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    draft.sftp_options = HostSftpOptions {
        local_start_directory: Some(root.path().join("initial-local")),
        remote_start_directory: Some("/srv/initial-remote".to_owned()),
        preserve_mtime: false,
        follow_source_symlinks: true,
        concurrency: Some(3),
        ..HostSftpOptions::default()
    };
    service
        .save_host(draft, None)
        .await
        .expect("写入无凭据的合成主机");

    let path = root.path().join("hosts/sftp-defaults.toml");
    let source = fs::read_to_string(&path).expect("读取刚写出的 host 文档");
    let mut parsed: toml::Value = toml::from_str(&source).expect("writer 输出有效 TOML");
    let sftp = parsed
        .get_mut("ssh_options")
        .and_then(toml::Value::as_table_mut)
        .and_then(|ssh| ssh.get_mut("sftp"))
        .and_then(toml::Value::as_table_mut)
        .expect("当前 host 包含 SFTP 表");
    for field in [
        "local_start_directory",
        "remote_start_directory",
        "preserve_mtime",
        "follow_source_symlinks",
        "concurrency",
    ] {
        assert!(
            sftp.remove(field).is_some(),
            "writer fixture must contain {field} before omission"
        );
    }
    let source_without_options = toml::to_string(&parsed).expect("移除可选项后仍是 TOML");
    assert!(source_without_options.contains("auth_type = \"agent\""));
    assert!(!source_without_options.contains("password ="));
    assert!(!source_without_options.contains("secret_ref"));
    assert!(!source_without_options.contains("inline_private_key"));
    fs::write(&path, source_without_options).expect("写入字段省略的隔离 fixture");

    let reloaded = ConfigurationStore::initialize(root.path()).expect("当前格式可正常加载");
    assert_eq!(reloaded.recovery_state().mode, ConfigurationMode::ReadWrite);
    let reloaded_repository = HostConfigurationRepository::new(reloaded);
    let host = reloaded_repository
        .load_host(&host_id)
        .await
        .expect("当前 reader 可读取")
        .expect("合成主机存在");
    assert_eq!(host.sftp_options, HostSftpOptions::default());
    assert_eq!(host.authentication_mode, HostAuthenticationMode::Agent);
    assert_eq!(host.note, None);
}

/// 验证可选备注经创建、编辑与回读保留多行，并拒绝非法配置而保留源文件。
///
/// @author kongweiguang
#[tokio::test]
async fn host_note_round_trips_and_invalid_notes_enter_read_only_recovery() {
    let root = tempdir().expect("隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化当前配置");
    let repository = HostConfigurationRepository::new(store);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let host_id = HostId::parse("host-note").expect("合成主机 ID 合法");
    let mut draft = serial_draft(host_id.clone(), Vec::new());
    draft.protocol = RemoteProtocol::Ssh;
    draft.serial = None;
    draft.endpoint = HostEndpoint {
        host: "example.invalid".to_owned(),
        port: 22,
        username: "user".to_owned(),
    };
    let expected_note = "构建节点\n维护联系人：运维\t值班";
    draft.note = Some(expected_note.to_owned());

    let saved = service
        .save_host(draft.clone(), None)
        .await
        .expect("创建含多行备注的主机");
    assert_eq!(saved.note.as_deref(), Some(expected_note));
    let path = root.path().join("hosts/host-note.toml");
    let raw = fs::read_to_string(&path).expect("读取真实 host TOML");
    let parsed: toml::Value = toml::from_str(&raw).expect("writer 输出有效 TOML");
    assert_eq!(parsed["note"].as_str(), Some(expected_note));
    assert_eq!(
        repository
            .load_host(&host_id)
            .await
            .expect("当前 reader 可读取")
            .expect("主机存在")
            .note
            .as_deref(),
        Some(expected_note)
    );

    let mut blank_note = draft;
    blank_note.name = "renamed host".to_owned();
    blank_note.note = Some(" \n\t ".to_owned());
    let updated = service
        .save_host(blank_note, Some(saved.revision.clone()))
        .await
        .expect("编辑时空白备注归一化");
    assert_eq!(updated.note, None);
    let blank_raw = fs::read_to_string(&path).expect("读取更新后 TOML");
    let blank_parsed: toml::Value = toml::from_str(&blank_raw).expect("更新文档仍是有效 TOML");
    assert!(blank_parsed.get("note").is_none());

    for invalid_note in [
        "中".repeat(kerminal_application::connections::management::HOST_NOTE_MAX_CHARS + 1),
        "非法\0控制字符".to_owned(),
    ] {
        let mut document: toml::Value = toml::from_str(&blank_raw).expect("解析有效 host 源文");
        document
            .as_table_mut()
            .expect("host 文档为根表")
            .insert("note".to_owned(), toml::Value::String(invalid_note));
        let invalid_source = toml::to_string(&document).expect("编码非法备注样本");
        fs::write(&path, &invalid_source).expect("写入隔离非法备注");
        let invalid = ConfigurationStore::initialize(root.path()).expect("恢复态可打开");
        assert_eq!(
            invalid.recovery_state().mode,
            ConfigurationMode::ReadOnlyRecovery
        );
        assert_eq!(
            fs::read_to_string(&path).expect("validator 保留原文"),
            invalid_source
        );
    }
}

/// 让测试草稿使用设备路径作为领域 endpoint 投影，但只通过 serial 值对象持久化配置。
///
/// @author kongweiguang
fn serial_draft(id: HostId, tags: Vec<String>) -> HostDraft {
    HostDraft {
        id: Some(id),
        group_id: None,
        name: "Synthetic serial console".to_owned(),
        note: None,
        protocol: RemoteProtocol::Serial,
        endpoint: HostEndpoint {
            host: "COM8".to_owned(),
            port: 0,
            username: String::new(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: Default::default(),
        serial: Some(SerialSessionTarget {
            device: PathBuf::from("COM8"),
            baud_rate: 57_600,
            data_bits: SerialDataBits::Seven,
            stop_bits: SerialStopBits::Two,
            parity: SerialParity::Even,
            flow_control: SerialFlowControl::Software,
        }),
        tags,
        is_favorite: false,
        sort_order: 3,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    }
}

/// 真实主机用例拒绝串口保留 tag，并在成功写入后通过配置仓储读回 typed 表。
///
/// @author kongweiguang
#[tokio::test]
async fn current_writer_and_reader_keep_serial_data_in_the_typed_table() {
    let root = tempdir().expect("创建隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化隔离配置根");
    let repository = HostConfigurationRepository::new(store.clone());
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let host_id = HostId::parse("serial-writer").expect("合成主机 ID 合法");

    let mut mismatched_endpoint = serial_draft(host_id.clone(), Vec::new());
    mismatched_endpoint.endpoint.host = "COM99".to_owned();
    assert!(
        service.save_host(mismatched_endpoint, None).await.is_err(),
        "串口 endpoint 只能投影 serial.device"
    );

    assert!(
        service
            .save_host(
                serial_draft(host_id.clone(), vec!["serial-baud:9600".to_owned()]),
                None,
            )
            .await
            .is_err()
    );
    let host_path = root.path().join("hosts/serial-writer.toml");
    assert!(!host_path.exists(), "无效 tag 不得留下部分 host 文件");

    let saved = service
        .save_host(serial_draft(host_id.clone(), vec!["lab".to_owned()]), None)
        .await
        .expect("保存当前 schema-v3 串口主机");
    assert_eq!(saved.protocol, RemoteProtocol::Serial);
    assert_eq!(saved.endpoint.host, "COM8");
    assert_eq!(saved.endpoint.port, 0);
    assert!(saved.endpoint.username.is_empty());
    assert_eq!(
        saved.serial.as_ref().map(|serial| serial.baud_rate),
        Some(57_600)
    );

    let source = fs::read_to_string(&host_path).expect("读取刚写出的 host 文件");
    let parsed: toml::Value = toml::from_str(&source).expect("writer 输出有效 TOML");
    assert_eq!(
        parsed
            .get("schema_version")
            .and_then(toml::Value::as_integer),
        Some(3)
    );
    assert!(parsed.get("host").is_none());
    assert!(parsed.get("port").is_none());
    assert!(parsed.get("username").is_none());
    assert_eq!(
        parsed
            .get("tags")
            .and_then(toml::Value::as_array)
            .expect("普通用户标签存在")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>(),
        ["lab"]
    );
    let readback = repository
        .load_host(&host_id)
        .await
        .expect("通过当前 reader 读取")
        .expect("host 文件存在");
    assert_eq!(readback.serial, saved.serial);
}

/// 当前 reader 接受专用 serial 表，并把设备值投影到既有领域 endpoint。
///
/// @author kongweiguang
#[tokio::test]
async fn current_serial_fixture_reads_without_endpoint_duplication() {
    let root = tempdir().expect("创建隔离配置根");
    fs::create_dir_all(root.path().join("hosts")).expect("创建 hosts 目录");
    let path = root.path().join("hosts/serial-current.toml");
    fs::write(&path, CURRENT_SERIAL_HOST).expect("写入当前 schema-v3 fixture");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化当前配置");
    assert_eq!(store.recovery_state().mode, ConfigurationMode::ReadWrite);

    let repository = HostConfigurationRepository::new(store);
    let host = repository
        .load_host(&HostId::parse("serial-current").expect("主机 ID 合法"))
        .await
        .expect("当前 serial fixture 可读取")
        .expect("host 存在");

    assert_eq!(host.endpoint.host, "COM7");
    assert_eq!(host.endpoint.port, 0);
    assert!(host.endpoint.username.is_empty());
    assert_eq!(
        host.serial.as_ref().map(|serial| serial.device.clone()),
        Some(PathBuf::from("COM7"))
    );
    assert_eq!(host.tags, ["lab"]);
}

/// 一次性数据迁移真实提交旧 tags；随后启动 current reader 并验证投影和源字段清理。
///
/// @author kongweiguang
#[tokio::test]
async fn legacy_serial_migration_applies_and_current_reader_reads_back() {
    let root = tempdir().expect("创建隔离配置根");
    fs::create_dir_all(root.path().join("hosts")).expect("创建 hosts 目录");
    let path = root.path().join("hosts/serial-legacy.toml");
    fs::write(&path, LEGACY_SERIAL_HOST).expect("写入冻结的 schema-v2 迁移 fixture");

    let report = FileDataMigrationRepository::new(root.path().to_path_buf())
        .migrate()
        .expect("应用格式迁移");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    let migrated = fs::read_to_string(&path).expect("读取迁移后的 host");
    let parsed: toml::Value = toml::from_str(&migrated).expect("迁移结果为有效 TOML");
    assert_eq!(
        parsed
            .get("schema_version")
            .and_then(toml::Value::as_integer),
        Some(3)
    );
    assert!(parsed.get("host").is_none());
    assert!(parsed.get("port").is_none());
    assert!(parsed.get("username").is_none());
    assert!(!migrated.contains("serial-flow:"));

    let store = ConfigurationStore::initialize(root.path()).expect("当前格式可正常启动");
    assert_eq!(store.recovery_state().mode, ConfigurationMode::ReadWrite);
    let repository = HostConfigurationRepository::new(store);
    let host = repository
        .load_host(&HostId::parse("serial-legacy").expect("主机 ID 合法"))
        .await
        .expect("迁移后的 current reader 可读取")
        .expect("迁移主机存在");
    assert_eq!(host.protocol, RemoteProtocol::Serial);
    assert_eq!(host.endpoint.host, "COM11");
    assert_eq!(
        host.serial.as_ref().map(|serial| serial.baud_rate),
        Some(115_200)
    );
    assert_eq!(
        host.serial.as_ref().map(|serial| serial.parity),
        Some(SerialParity::Even)
    );
    assert_eq!(host.tags, ["lab"]);
}

/// schema-v2 输入在普通启动时只读拒绝，原字节留给显式迁移入口处理。
///
/// @author kongweiguang
#[tokio::test]
async fn current_reader_rejects_legacy_serial_without_rewriting_it() {
    let root = tempdir().expect("创建隔离配置根");
    fs::create_dir_all(root.path().join("hosts")).expect("创建 hosts 目录");
    let path = root.path().join("hosts/serial-legacy.toml");
    fs::write(&path, LEGACY_SERIAL_HOST).expect("写入旧格式输入");
    let original = fs::read(&path).expect("读取旧格式原字节");

    let store = ConfigurationStore::initialize(root.path()).expect("损坏配置以恢复态打开");

    assert_eq!(
        store.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery
    );
    assert_eq!(fs::read(&path).expect("再次读取 host 原字节"), original);
    let repository = HostConfigurationRepository::new(store);
    assert!(
        repository.list_hosts().await.is_err(),
        "当前 host reader 必须拒绝 schema-v2 和旧串口 tags"
    );
    assert_eq!(fs::read(&path).expect("拒绝后 host 仍保持原字节"), original);
}

/// 当前 schema 拒绝短别名与缺失串口字段，验证器和 reader 都保留原始文档。
///
/// @author kongweiguang
#[tokio::test]
async fn current_reader_rejects_serial_aliases_and_missing_fields() {
    let cases = [
        (
            "alias",
            CURRENT_SERIAL_HOST.replace("parity = \"none\"", "parity = \"N\""),
        ),
        (
            "missing",
            CURRENT_SERIAL_HOST.replace("flow_control = \"none\"", ""),
        ),
        (
            "reserved_tag",
            CURRENT_SERIAL_HOST.replace("tags = [\"lab\"]", "tags = [\"serial-parity:E\"]"),
        ),
        (
            "duplicate_endpoint",
            CURRENT_SERIAL_HOST.replace(
                "protocol = \"serial\"",
                "protocol = \"serial\"\nhost = \"COM7\"",
            ),
        ),
    ];

    for (name, source) in cases {
        let root = tempdir().expect("创建隔离配置根");
        fs::create_dir_all(root.path().join("hosts")).expect("创建 hosts 目录");
        let path = root.path().join("hosts/serial-current.toml");
        fs::write(&path, &source).expect("写入非法当前格式 host");

        let store = ConfigurationStore::initialize(root.path()).expect("损坏配置以恢复态打开");
        assert_eq!(
            store.recovery_state().mode,
            ConfigurationMode::ReadOnlyRecovery,
            "{name} current serial must block configuration writes"
        );
        let repository = HostConfigurationRepository::new(store);
        assert!(
            repository
                .load_host(&HostId::parse("serial-current").expect("主机 ID 合法"))
                .await
                .is_err(),
            "{name} current serial must fail closed"
        );
        assert_eq!(fs::read(&path).expect("读取原始文档"), source.as_bytes());
    }
}

/// 当前非串口主机不得携带 serial 表，网络 endpoint 不覆盖或忽略该冲突字段。
///
/// @author kongweiguang
#[tokio::test]
async fn current_nonserial_host_rejects_serial_table() {
    let root = tempdir().expect("创建隔离配置根");
    fs::create_dir_all(root.path().join("hosts")).expect("创建 hosts 目录");
    let path = root.path().join("hosts/ssh-invalid.toml");
    let source = concat!(
        "schema_version = 3\nprotocol = \"ssh\"\nid = \"ssh-invalid\"\n",
        "name = \"Invalid SSH\"\nhost = \"example.test\"\nport = 22\n",
        "username = \"user\"\nauth_type = \"agent\"\ntags = []\n",
        "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n",
        "[serial]\ndevice = \"COM7\"\nbaud_rate = 9600\n",
        "data_bits = 8\nstop_bits = 1\nparity = \"none\"\nflow_control = \"none\"\n",
    );
    fs::write(&path, source).expect("写入非法非串口格式 host");

    let store = ConfigurationStore::initialize(root.path()).expect("损坏配置以恢复态打开");
    assert_eq!(
        store.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery
    );
    let repository = HostConfigurationRepository::new(store);
    assert!(
        repository
            .load_host(&HostId::parse("ssh-invalid").expect("主机 ID 合法"))
            .await
            .is_err()
    );
    assert_eq!(fs::read(&path).expect("读取原始文档"), source.as_bytes());
}
