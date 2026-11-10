//! known_hosts 私有单元测试。
//
// @author kongweiguang

use kerminal_application::connections::{HostKeyIdentity, HostKeyStatus, KnownHostsRepository};
use kerminal_application::{AppResult, PublicErrorCode};
use kerminal_domain::connections::HostEndpoint;
use russh::keys::{HashAlg, ssh_key::PublicKey};
use tempfile::tempdir;

use super::{
    KNOWN_HOSTS_FILE, KnownHostsStore, classify_key, commit_staged_known_hosts_replacement,
    replace_known_host_contents, write_staging_file,
};
use crate::storage::StorageCoordinator;

const PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const OTHER_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
const HASHED_HOST_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

/// 从有效 OpenSSH 公钥构造 adapter 入参，测试同时覆盖格式和指纹校验。
fn identity(public_key: &str) -> HostKeyIdentity {
    let key = PublicKey::from_openssh(public_key).expect("valid test public key");
    HostKeyIdentity {
        algorithm: key.algorithm().to_string(),
        fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        public_key_openssh: public_key.to_owned(),
    }
}

/// 创建每个测试独享的用户数据根，避免读取系统或开发者的真实 known_hosts。
fn store(root: &std::path::Path) -> KnownHostsStore {
    KnownHostsStore::new(StorageCoordinator::new(root), true)
}

/// 将内存中的 known_hosts 行写入隔离临时文件，供 russh 的真实文件 parser 分类。
fn classify_fixture(
    endpoint: &HostEndpoint,
    identity: &HostKeyIdentity,
    content: &[u8],
) -> AppResult<HostKeyStatus> {
    let directory = tempdir().expect("temp directory");
    let path = directory.path().join(KNOWN_HOSTS_FILE);
    std::fs::write(&path, content).expect("write fixture");
    classify_key(endpoint, identity, &path, content)
}

/// 验证首次信任写入完整 OpenSSH 公钥行，并让后续读取命中 Known。
#[tokio::test]
async fn trust_writes_parseable_openssh_record() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22461,
        username: "test".to_owned(),
    };
    let key_identity = identity(PUBLIC_KEY);

    assert!(matches!(
        repository
            .inspect(&endpoint, &key_identity)
            .await
            .expect("inspect"),
        HostKeyStatus::Unknown
    ));
    repository
        .trust(&endpoint, &key_identity)
        .await
        .expect("trust");
    assert!(matches!(
        repository
            .inspect(&endpoint, &key_identity)
            .await
            .expect("inspect known"),
        HostKeyStatus::Known
    ));
    let line = std::fs::read_to_string(directory.path().join(KNOWN_HOSTS_FILE))
        .expect("known_hosts content");
    assert!(line.starts_with("[example.test]:22461 ssh-ed25519 "));
    assert!(!line.contains(&key_identity.fingerprint_sha256));

    repository
        .trust(&endpoint, &key_identity)
        .await
        .expect("repeated trust is idempotent");
    assert_eq!(
        std::fs::read_to_string(directory.path().join(KNOWN_HOSTS_FILE))
            .expect("known_hosts content after retry"),
        line,
        "a known key must not be appended twice"
    );
}

/// 已有不同 key 时回报 Changed；自动信任路径由 trust 原子替换旧记录，连接不再被阻断。
#[tokio::test]
async fn changed_key_is_atomically_replaced_on_trust() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let first = identity(PUBLIC_KEY);
    let changed = identity(OTHER_PUBLIC_KEY);
    repository
        .trust(&endpoint, &first)
        .await
        .expect("trust first");
    assert!(matches!(
        repository
            .inspect(&endpoint, &changed)
            .await
            .expect("inspect changed"),
        HostKeyStatus::Changed { .. }
    ));

    repository
        .trust(&endpoint, &changed)
        .await
        .expect("changed key is upserted instead of blocked");
    assert!(matches!(
        repository
            .inspect(&endpoint, &changed)
            .await
            .expect("inspect replaced key"),
        HostKeyStatus::Known
    ));
    let content = std::fs::read_to_string(directory.path().join(KNOWN_HOSTS_FILE))
        .expect("read replaced known_hosts");
    assert!(!content.contains(&first.public_key_openssh));
    assert!(content.contains(&changed.public_key_openssh));
}

/// 已知 key 必须在 writable 判断之前返回成功，使只读数据根中的已受信主机仍可连接。
#[tokio::test]
async fn known_key_succeeds_without_write_access() {
    let directory = tempdir().expect("temp directory");
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let key_identity = identity(PUBLIC_KEY);
    store(directory.path())
        .trust(&endpoint, &key_identity)
        .await
        .expect("seed writable known_hosts");
    let readonly = KnownHostsStore::new(StorageCoordinator::new(directory.path()), false);

    readonly
        .trust(&endpoint, &key_identity)
        .await
        .expect("known key requires no write access");
    assert!(
        readonly
            .trust(&endpoint, &identity(OTHER_PUBLIC_KEY))
            .await
            .is_err(),
        "unknown keys still report the read-only storage error for the caller to downgrade"
    );
}

/// 当前 key 的 @revoked 记录在 classify 中表现为 previous==current；自动信任只保留原记录并成功返回。
#[tokio::test]
async fn revoked_key_trust_is_ignored_without_blocking() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let key_identity = identity(PUBLIC_KEY);
    let fixture = format!(
        "@revoked other.example {}\n",
        key_identity.public_key_openssh
    );
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    std::fs::write(&known_hosts, &fixture).expect("write revoked fixture");

    repository
        .trust(&endpoint, &key_identity)
        .await
        .expect("revoked key must not create a blocking error");
    assert_eq!(
        std::fs::read_to_string(&known_hosts).expect("read preserved fixture"),
        fixture
    );
}

/// russh 的 matcher 必须继续识别冻结文件中的 OpenSSH 哈希主机名。
#[test]
fn hashed_hostname_record_is_known() {
    let key_identity = identity(HASHED_HOST_KEY);
    let content = b"|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n";
    let endpoint = HostEndpoint {
        host: "example.com".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    assert!(matches!(
        classify_fixture(&endpoint, &key_identity, content).expect("classify hashed host"),
        HostKeyStatus::Known
    ));
}

/// 撤销记录仍归类为 Changed，供 trust 保留原记录并告警；分类本身不再阻断连接。
#[test]
fn revoked_key_is_never_known() {
    let key_identity = identity(PUBLIC_KEY);
    let content = b"@revoked other.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ\n";
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    assert!(matches!(
        classify_fixture(&endpoint, &key_identity, content).expect("classify revoked"),
        HostKeyStatus::Changed { .. }
    ));
}

/// staging 完成后若外部编辑写入第三值，提交边界必须拒绝覆盖并保留该值。
#[test]
fn rotation_does_not_replace_external_edit_after_staging() {
    let directory = tempdir().expect("temp directory");
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(PUBLIC_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let target = directory.path().join(KNOWN_HOSTS_FILE);
    let original = format!("example.test {} confirmed\n", old.public_key_openssh).into_bytes();
    std::fs::write(&target, &original).expect("write confirmed original");
    let updated =
        replace_known_host_contents(&endpoint, &old.fingerprint_sha256, &new, &target, &original)
            .expect("prepare replacement");
    let staging = write_staging_file(&target, &updated).expect("stage replacement");
    let external_edit = format!("other.example {} external-edit\n", HASHED_HOST_KEY).into_bytes();
    std::fs::write(&target, &external_edit).expect("simulate non-cooperating editor");

    let result = commit_staged_known_hosts_replacement(&target, &staging, &original, &updated);

    assert!(matches!(
        result,
        Err(error) if error.public_code() == PublicErrorCode::Conflict
    ));
    assert_eq!(
        std::fs::read(&target).expect("read preserved external edit"),
        external_edit
    );
    assert!(!staging.exists(), "rejected staging file must be removed");
}
