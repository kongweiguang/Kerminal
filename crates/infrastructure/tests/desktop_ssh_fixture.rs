//! 为 GPUI 桌面验收生成全新、隔离的 SSH 配置根；默认不执行，也不连接网络。
//!
//! 显式环境参数：`KERMINAL_DESKTOP_SSH_FIXTURE_RUN_ID`、
//! `KERMINAL_DESKTOP_SSH_FIXTURE_SOURCE_ROOT`、
//! 可选 `KERMINAL_DESKTOP_SSH_FIXTURE_SEED_ROOT`（当前格式的公开展示 seed）、
//! `KERMINAL_DESKTOP_SSH_FIXTURE_PRIVATE_KEY_PATH`、
//! `KERMINAL_DESKTOP_SSH_FIXTURE_KNOWN_HOSTS_PATH`。
//!
//! @author kongweiguang

use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use kerminal_application::connections::{
    credentials::{
        CredentialPresence, CredentialService, SecretValue, VaultKeyAvailability,
        VaultKeyManagementService,
    },
    management::{
        HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft,
        HostManagementService,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol, SshAuthentication},
    identity::HostId,
    preferences::ConfigurationMode,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
    vault::VaultStore,
};
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, Zeroizing};

const RUN_ID_ENV: &str = "KERMINAL_DESKTOP_SSH_FIXTURE_RUN_ID";
const SSH_SOURCE_ROOT_ENV: &str = "KERMINAL_DESKTOP_SSH_FIXTURE_SOURCE_ROOT";
const SEED_ROOT_ENV: &str = "KERMINAL_DESKTOP_SSH_FIXTURE_SEED_ROOT";
const PRIVATE_KEY_PATH_ENV: &str = "KERMINAL_DESKTOP_SSH_FIXTURE_PRIVATE_KEY_PATH";
const KNOWN_HOSTS_PATH_ENV: &str = "KERMINAL_DESKTOP_SSH_FIXTURE_KNOWN_HOSTS_PATH";
const SSH_FIXTURE_HOST: &str = "127.0.0.1";
const SSH_FIXTURE_PORT: u16 = 22661;
const SSH_FIXTURE_USERNAME: &str = "kong";
const SSH_FIXTURE_HOST_ID: &str = "ui-live-ssh-r24";
const SSH_FIXTURE_GROUP_ID: &str = "ui-demo-core";
const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

const R20_SEED_FILES: [&str; 8] = [
    "settings.toml",
    "hosts/groups.toml",
    "hosts/ui-demo-ssh-build.toml",
    "hosts/ui-demo-ssh-observe.toml",
    "hosts/ui-demo-ssh-stage.toml",
    "hosts/ui-demo-telnet-console.toml",
    "profiles/ui-demo-local-pwsh.toml",
    "snippets/r20-public-path-inspection.toml",
];

type FixtureResult<T> = Result<T, &'static str>;

struct FixtureLayout {
    target_root: PathBuf,
    output_parent: PathBuf,
    seed_root: PathBuf,
    ssh_source_root: PathBuf,
}

struct FixtureOutput {
    config_root: PathBuf,
    seed_file_count: usize,
    vault_key_pair_present: bool,
}

/// 该测试只在显式调用时写入新建的仓库 target 子目录，且不建立终端或发起网络连接。
#[tokio::test]
#[ignore = "仅由独立 GPUI 桌面验收显式生成隔离 SSH fixture"]
async fn generate_desktop_ssh_fixture() {
    let output = generate_fixture()
        .await
        .unwrap_or_else(|reason| panic!("{reason}"));
    println!(
        "host={SSH_FIXTURE_HOST} port={SSH_FIXTURE_PORT} vault_key_pair_present={} seed_file_count={} config_root={}",
        output.vault_key_pair_present,
        output.seed_file_count,
        output.config_root.display()
    );
}

/// 在全部路径经过隔离校验后创建空配置根，再通过正式设置、vault 与主机用例生成数据。
/// @author kongweiguang
async fn generate_fixture() -> FixtureResult<FixtureOutput> {
    let layout = fixture_layout()?;
    let run_id = required_env(RUN_ID_ENV)?;
    if !is_safe_run_id(&run_id) {
        return Err("run-id 格式无效");
    }

    let private_key_path = validated_fixture_input(PRIVATE_KEY_PATH_ENV, &layout)?;
    let known_hosts_path = validated_fixture_input(KNOWN_HOSTS_PATH_ENV, &layout)?;
    validate_fixture_size(
        &private_key_path,
        MAX_PRIVATE_KEY_BYTES,
        "私钥 fixture 大小无效",
    )?;
    validate_fixture_size(
        &known_hosts_path,
        MAX_KNOWN_HOSTS_BYTES,
        "known_hosts fixture 大小无效",
    )?;

    let output_root = layout.output_parent.join(run_id);
    let config_root = output_root.join("config");
    validate_output_path(&output_root, &layout.target_root)?;
    validate_output_path(&config_root, &layout.target_root)?;
    for relative in R20_SEED_FILES {
        validate_output_path(&config_root.join(relative), &layout.target_root)?;
    }
    validate_output_path(&config_root.join("known_hosts"), &layout.target_root)?;
    validate_output_path(
        &config_root
            .join("hosts")
            .join(format!("{SSH_FIXTURE_HOST_ID}.toml")),
        &layout.target_root,
    )?;
    validate_output_path(&config_root.join("secrets/vault.toml"), &layout.target_root)?;
    validate_output_path(
        &config_root.join("secrets/vault-key.toml"),
        &layout.target_root,
    )?;

    fs::create_dir(&output_root).map_err(|_| "创建新的验收输出目录失败")?;
    fs::create_dir(&config_root).map_err(|_| "创建新的配置根失败")?;
    let seed_file_count = copy_r20_seed(&layout.seed_root, &config_root, &layout.target_root)?;
    copy_known_hosts(
        &known_hosts_path,
        &layout.ssh_source_root,
        &config_root.join("known_hosts"),
        &layout.target_root,
    )?;

    let configuration =
        ConfigurationStore::initialize(&config_root).map_err(|_| "初始化隔离配置仓储失败")?;
    if configuration.recovery_state().mode != ConfigurationMode::ReadWrite {
        return Err("新建隔离配置根未处于可写状态");
    }
    if config_root.join("data/command.sqlite").exists()
        || config_root.join("workspace/session.json").exists()
    {
        return Err("验收配置根意外包含运行态数据库或旧工作区快照");
    }

    let storage = configuration.storage_coordinator();
    let vault = VaultStore::open(storage);
    let key_management = VaultKeyManagementService::new(Arc::new(vault.clone()));
    match key_management
        .ensure()
        .await
        .map_err(|_| "初始化隔离 vault key 失败")?
    {
        VaultKeyAvailability::Ready { .. } | VaultKeyAvailability::Created { .. } => {}
        _ => return Err("隔离 vault key 状态不完整"),
    }

    let credentials = CredentialService::new(Arc::new(vault));
    let host_repository = HostConfigurationRepository::new(configuration.clone());
    let hosts =
        HostManagementService::with_credentials(Arc::new(host_repository), credentials.clone());
    let groups = hosts
        .load_groups()
        .await
        .map_err(|_| "读取 r20 主机分组 fixture 失败")?;
    if !groups
        .groups
        .iter()
        .any(|group| group.id == SSH_FIXTURE_GROUP_ID)
    {
        return Err("r20 主机分组 fixture 缺少目标分组");
    }

    let private_key_path = validate_source_file(&private_key_path, &layout.ssh_source_root)?;
    let mut key_bytes =
        Zeroizing::new(fs::read(&private_key_path).map_err(|_| "读取私钥 fixture 失败")?);
    if key_bytes.is_empty() || key_bytes.len() as u64 > MAX_PRIVATE_KEY_BYTES {
        return Err("私钥 fixture 大小无效");
    }
    let key_text = std::str::from_utf8(&key_bytes).map_err(|_| "私钥 fixture 不是 UTF-8 文本")?;
    let secret = SecretValue::new(key_text.to_owned());
    key_bytes.zeroize();

    let host_id = HostId::parse(SSH_FIXTURE_HOST_ID).map_err(|_| "验收主机 ID 无效")?;
    let receipt = hosts
        .save_host_with_credentials(
            HostDraft {
                id: Some(host_id.clone()),
                group_id: Some(SSH_FIXTURE_GROUP_ID.to_owned()),
                name: "原生 SSH 验收".to_owned(),
                note: None,
                protocol: RemoteProtocol::Ssh,
                endpoint: HostEndpoint {
                    host: SSH_FIXTURE_HOST.to_owned(),
                    port: SSH_FIXTURE_PORT,
                    username: SSH_FIXTURE_USERNAME.to_owned(),
                },
                authentication: Some(SshAuthentication::PrivateKey {
                    path: None,
                    secret: None,
                    passphrase: None,
                }),
                authentication_mode: HostAuthenticationMode::PrivateKey,
                rdp_password: None,
                route: Default::default(),
                serial: None,
                tags: vec!["desktop-acceptance".to_owned()],
                is_favorite: false,
                sort_order: 90,
                connect_timeout_ms: 30_000,
                keepalive_interval_ms: Some(60_000),
                terminal_options: Default::default(),
                sftp_options: Default::default(),
                rdp_display: Default::default(),
            },
            None,
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPrivateKey,
                secret,
            }],
            CancellationToken::new(),
        )
        .await
        .map_err(|_| "通过主机管理用例保存验收主机失败")?;

    if !matches!(
        &receipt.host.authentication,
        Some(SshAuthentication::PrivateKey {
            path: None,
            secret: Some(_),
            passphrase: None
        })
    ) {
        return Err("主机未保存为 vault 私钥引用");
    }
    let key_reference = receipt
        .credentials
        .iter()
        .find(|credential| credential.slot == HostCredentialSlot::TargetPrivateKey)
        .map(|credential| &credential.reference)
        .ok_or("主机保存回执缺少私钥引用")?;
    if credentials
        .inspect(key_reference)
        .await
        .map_err(|_| "检查隔离 vault 私钥引用失败")?
        != CredentialPresence::Saved
    {
        return Err("隔离 vault 中未找到主机私钥");
    }

    let host_document = fs::read_to_string(
        config_root
            .join("hosts")
            .join(format!("{SSH_FIXTURE_HOST_ID}.toml")),
    )
    .map_err(|_| "读取生成的主机配置失败")?;
    if !host_document
        .lines()
        .any(|line| line.trim_start().starts_with("secret_ref ="))
        || host_document.contains("-----BEGIN ")
    {
        return Err("主机配置未满足仅保存 vault 引用的约束");
    }

    let vault_key_pair_present = config_root.join("secrets/vault.toml").is_file()
        && config_root.join("secrets/vault-key.toml").is_file();
    if !vault_key_pair_present {
        return Err("隔离 vault 与 key 文件对不完整");
    }

    Ok(FixtureOutput {
        config_root,
        seed_file_count,
        vault_key_pair_present,
    })
}

/// 从 crate manifest 定位工作树，并单独解析任务提供的只读 SSH 源目录和展示 seed。
/// 源目录可以位于工作树之外，但不能与新建验收输出目录重叠。
/// 显式 seed 仍须位于隔离输出父目录，避免修改历史 seed 来迁就当前 schema。
/// @author kongweiguang
fn fixture_layout() -> FixtureResult<FixtureLayout> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("无法定位工作区根目录")?
        .canonicalize()
        .map_err(|_| "无法校验工作区根目录")?;
    let target_path = workspace_root.join("target");
    reject_reparse_components(&target_path)?;
    let target_root = target_path
        .canonicalize()
        .map_err(|_| "target 目录不存在")?;
    let acceptance_path = target_root.join("acceptance");
    reject_reparse_components(&acceptance_path)?;
    let acceptance_root = acceptance_path
        .canonicalize()
        .map_err(|_| "验收 fixture 根目录不存在")?;
    let output_path = acceptance_root.join("ui-refresh-fixture");
    reject_reparse_components(&output_path)?;
    let output_parent = output_path
        .canonicalize()
        .map_err(|_| "验收输出父目录不存在")?;
    let seed_path = env::var_os(SEED_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| output_parent.join("r20-seed-20261017T170523Z"));
    reject_reparse_components(&seed_path)?;
    let seed_root = seed_path
        .canonicalize()
        .map_err(|_| "公开展示 seed 不存在")?;
    let source_value = required_env(SSH_SOURCE_ROOT_ENV)?;
    let source_path = PathBuf::from(source_value);
    if !source_path.is_absolute() {
        return Err("SSH fixture 源目录必须为绝对路径");
    }
    reject_reparse_components(&source_path)?;
    let source_metadata =
        fs::symlink_metadata(&source_path).map_err(|_| "任务私有 SSH fixture 源目录不可用")?;
    if !source_metadata.is_dir() || is_link_or_reparse_point(&source_metadata) {
        return Err("SSH fixture 源必须是普通目录");
    }
    let ssh_source_root = source_path
        .canonicalize()
        .map_err(|_| "任务私有 SSH fixture 源目录不可用")?;
    if !acceptance_root.starts_with(&target_root)
        || !output_parent.starts_with(&target_root)
        || !seed_root.starts_with(&output_parent)
        || paths_overlap(&ssh_source_root, &output_parent)
    {
        return Err("fixture 源或目标越过隔离边界或相互重叠");
    }
    Ok(FixtureLayout {
        target_root,
        output_parent,
        seed_root,
        ssh_source_root,
    })
}

/// 只接受显式绝对文件路径，并确认规范化文件仍位于任务指定的只读源目录。
/// @author kongweiguang
fn validated_fixture_input(variable: &str, layout: &FixtureLayout) -> FixtureResult<PathBuf> {
    let value = env::var_os(variable).ok_or("缺少显式 SSH fixture 输入参数")?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("SSH fixture 输入路径必须为绝对路径");
    }
    validate_source_file(&path, &layout.ssh_source_root)
}

/// 拒绝路径中的链接组件，再用规范化路径二次确认源文件归属。
/// @author kongweiguang
fn validate_source_file(path: &Path, source_root: &Path) -> FixtureResult<PathBuf> {
    reject_reparse_components(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| "SSH fixture 输入文件不可用")?;
    if !metadata.is_file() || is_link_or_reparse_point(&metadata) {
        return Err("SSH fixture 输入必须是普通文件");
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| "SSH fixture 输入文件不可用")?;
    if !canonical.starts_with(source_root) {
        return Err("SSH fixture 输入不属于显式指定的只读源目录");
    }
    Ok(canonical)
}

/// 仅从固定清单复制旧版展示 seed，避免连带搬运行态数据库、workspace 或旧 vault。
/// @author kongweiguang
fn copy_r20_seed(seed_root: &Path, config_root: &Path, target_root: &Path) -> FixtureResult<usize> {
    let mut copied = 0;
    for relative in R20_SEED_FILES {
        let source = seed_root.join(relative);
        reject_reparse_components(&source)?;
        let metadata = fs::symlink_metadata(&source).map_err(|_| "r20 seed 文件缺失")?;
        if !metadata.is_file() || is_link_or_reparse_point(&metadata) {
            return Err("r20 seed 仅允许普通配置文件");
        }
        let source = source.canonicalize().map_err(|_| "r20 seed 文件不可用")?;
        if !source.starts_with(seed_root) {
            return Err("r20 seed 文件越过隔离目录");
        }
        let destination = config_root.join(relative);
        validate_output_path(&destination, target_root)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|_| "创建 seed 配置目录失败")?;
        }
        fs::copy(source, destination).map_err(|_| "复制 r20 seed 配置失败")?;
        copied += 1;
    }
    if copied != R20_SEED_FILES.len() {
        return Err("r20 seed 文件计数不符合预期");
    }
    Ok(copied)
}

/// 仅将任务私有源目录内的 known_hosts 复制到隔离配置根，不修改源文件。
/// @author kongweiguang
fn copy_known_hosts(
    source: &Path,
    source_root: &Path,
    destination: &Path,
    target_root: &Path,
) -> FixtureResult<()> {
    validate_output_path(destination, target_root)?;
    let source = validate_source_file(source, source_root)?;
    fs::copy(source, destination).map_err(|_| "复制隔离 known_hosts fixture 失败")?;
    Ok(())
}

/// 在创建验收文件前验证路径位于工作树 target、父级无链接且最终组件不存在。
/// @author kongweiguang
fn validate_output_path(path: &Path, target_root: &Path) -> FixtureResult<()> {
    reject_reparse_components(path)?;
    let canonical_target_root = target_root
        .canonicalize()
        .map_err(|_| "无法校验验收 target 根目录")?;
    let canonical_candidate = canonicalize_output_candidate(path)?;
    if !canonical_candidate.starts_with(&canonical_target_root) {
        return Err("验收输出路径越过工作区 target");
    }
    match fs::symlink_metadata(path) {
        Ok(_) => Err("验收 run 或目标文件已存在，拒绝覆盖"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("无法确认验收输出路径为空"),
    }
}

/// 用最近存在祖先的规范路径重建新目标，统一 Windows 普通路径与 extended canonical prefix。
/// 先检查原路径的 reparse 组件，再比较真实规范路径，避免仅靠字符串/prefix 猜测越界。
/// @author kongweiguang
fn canonicalize_output_candidate(path: &Path) -> FixtureResult<PathBuf> {
    let mut existing_ancestor = path;
    loop {
        match fs::symlink_metadata(existing_ancestor) {
            Ok(_) => {
                let canonical_ancestor = existing_ancestor
                    .canonicalize()
                    .map_err(|_| "无法规范化验收输出祖先")?;
                let suffix = path
                    .strip_prefix(existing_ancestor)
                    .map_err(|_| "无法校验验收输出路径层级")?;
                return Ok(canonical_ancestor.join(suffix));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing_ancestor = existing_ancestor
                    .parent()
                    .ok_or("无法定位验收输出的现存祖先")?;
            }
            Err(_) => return Err("无法检查验收输出路径祖先"),
        }
    }
}

/// 两个根目录有包含关系时视为重叠，避免验收输出反向覆盖输入源。
/// @author kongweiguang
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// 检查已存在路径的每个组件，避免 junction/reparse point 改变验收输入或输出的实际位置。
/// @author kongweiguang
fn reject_reparse_components(path: &Path) -> FixtureResult<()> {
    if !path.is_absolute() {
        return Err("fixture 路径必须为绝对路径");
    }

    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir | Component::ParentDir => {
                return Err("fixture 路径不能包含相对跳转组件");
            }
            _ => current.push(component.as_os_str()),
        }

        // Windows 盘符前缀本身不是文件系统节点；其后的根目录组件才可检查。
        if matches!(component, Component::Prefix(_)) {
            continue;
        }

        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_or_reparse_point(&metadata) => {
                return Err("fixture 路径不能经过符号链接或 reparse point");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err("无法检查 fixture 路径组件"),
        }
    }
    Ok(())
}

/// Windows reparse point 包含 junction/mount point，不能只依赖 Rust 的 symlink 判断。
/// @author kongweiguang
#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// 其它平台用文件类型检查符号链接；该 fixture seeder 仍可跨平台编译。
/// @author kongweiguang
#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// 避免读取异常大小的 task fixture，限制内存占用并保护验收过程不意外吞入大文件。
fn validate_fixture_size(path: &Path, maximum: u64, error: &'static str) -> FixtureResult<()> {
    let size = fs::metadata(path)
        .map_err(|_| "SSH fixture 输入文件不可用")?
        .len();
    if size == 0 || size > maximum {
        return Err(error);
    }
    Ok(())
}

/// 将 run-id 限制为单一路径组件，拒绝分隔符、盘符及特殊目录。
fn is_safe_run_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// 读取显式环境参数但不在失败信息中回显其内容，防止路径进入验收日志。
fn required_env(name: &str) -> FixtureResult<String> {
    env::var(name).map_err(|_| "缺少显式验收环境参数")
}

#[cfg(test)]
#[path = "support/desktop_ssh_fixture_unit.rs"]
mod tests;
