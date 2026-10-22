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
}

/// 已有不同 key 时回报 Changed，trust 不能通过覆盖或自动追加来抹掉冲突。
#[tokio::test]
async fn changed_key_is_not_automatically_trusted() {
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
    assert!(repository.trust(&endpoint, &changed).await.is_err());
}

/// 轮换只替换确认过的 host:port；按原始字节和单主机解析核对其它记录，避免把追加顺序误当作保留语义。
#[tokio::test]
async fn explicit_rotation_replaces_only_the_confirmed_endpoint_record() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(PUBLIC_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let unrelated = b"other.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF keep\n";
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    let old_line = format!("example.test {} old-comment\r\n", old.public_key_openssh);
    let same_host_other_port = format!("[example.test]:2244 {} other-port\r\n", HASHED_HOST_KEY);
    let mut original = old_line.as_bytes().to_vec();
    original.extend_from_slice(same_host_other_port.as_bytes());
    original.extend_from_slice(unrelated);
    std::fs::write(&known_hosts, &original).expect("write old and unrelated records");

    repository
        .replace(&endpoint, &old.fingerprint_sha256, &new)
        .await
        .expect("replace confirmed key");

    let updated = std::fs::read(&known_hosts).expect("read rotated records");
    assert!(!String::from_utf8_lossy(&updated).contains(&old.public_key_openssh));
    assert!(String::from_utf8_lossy(&updated).contains(&new.public_key_openssh));
    assert!(String::from_utf8_lossy(&updated).contains(&same_host_other_port));
    assert!(
        updated
            .windows(unrelated.len())
            .any(|record| record == &unrelated[..]),
        "unrelated known_hosts record must remain byte-for-byte intact"
    );
    let other_port = HostEndpoint {
        host: "example.test".to_owned(),
        port: 2244,
        username: "test".to_owned(),
    };
    assert!(matches!(
        repository
            .inspect(&other_port, &identity(HASHED_HOST_KEY))
            .await
            .expect("inspect same host on another port"),
        HostKeyStatus::Known
    ));
    let unrelated_endpoint = HostEndpoint {
        host: "other.example".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    assert!(matches!(
        repository
            .inspect(&unrelated_endpoint, &identity(HASHED_HOST_KEY))
            .await
            .expect("inspect unrelated host"),
        HostKeyStatus::Known
    ));
    assert!(matches!(
        repository
            .inspect(&endpoint, &new)
            .await
            .expect("inspect rotated key"),
        HostKeyStatus::Known
    ));
}

/// 共享 host 字段轮换时只移除目标 alias，保留同一 OpenSSH 行上的其它别名信任。
#[tokio::test]
async fn explicit_rotation_preserves_aliases_from_a_shared_host_field() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let alias = HostEndpoint {
        host: "ssh-alias.example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(PUBLIC_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    std::fs::write(
        &known_hosts,
        format!(
            "example.test,ssh-alias.example.test {} shared-entry\n",
            old.public_key_openssh
        ),
    )
    .expect("write shared alias record");

    repository
        .replace(&endpoint, &old.fingerprint_sha256, &new)
        .await
        .expect("replace only requested alias");

    let updated = std::fs::read_to_string(&known_hosts).expect("read rotated records");
    assert!(updated.contains(&format!(
        "ssh-alias.example.test {} shared-entry",
        old.public_key_openssh
    )));
    assert!(matches!(
        repository
            .inspect(&alias, &old)
            .await
            .expect("inspect retained alias"),
        HostKeyStatus::Known
    ));
    assert!(matches!(
        repository
            .inspect(&endpoint, &new)
            .await
            .expect("inspect rotated endpoint"),
        HostKeyStatus::Known
    ));
}

/// 哈希 host 条目按 russh 返回的匹配行号删除，避免把别的端点的 known_hosts 记录改掉。
#[tokio::test]
async fn explicit_rotation_replaces_a_hashed_endpoint_record() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.com".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(HASHED_HOST_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    let unrelated = "unrelated.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ\n";
    let hashed = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n";
    std::fs::write(&known_hosts, format!("{hashed}{unrelated}")).expect("write hashed record");

    repository
        .replace(&endpoint, &old.fingerprint_sha256, &new)
        .await
        .expect("replace hashed key");

    let updated = std::fs::read_to_string(&known_hosts).expect("read rotated records");
    assert!(!updated.contains("|1|O33ESRMWPVkMYIwJ1Uw+n877jTo="));
    assert!(updated.contains(&new.public_key_openssh));
    assert!(updated.contains(unrelated.trim_end()));
    assert!(matches!(
        repository
            .inspect(&endpoint, &new)
            .await
            .expect("inspect rotated key"),
        HostKeyStatus::Known
    ));
}

/// 上次替换结果未知后重试同一操作应保持幂等，不能重复追加新 key 或覆盖新的并发状态。
#[tokio::test]
async fn explicit_rotation_retry_is_idempotent_after_old_record_is_gone() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(PUBLIC_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    repository
        .trust(&endpoint, &old)
        .await
        .expect("trust old key");
    repository
        .replace(&endpoint, &old.fingerprint_sha256, &new)
        .await
        .expect("first replacement");
    let committed = std::fs::read(&known_hosts).expect("read committed file");

    repository
        .replace(&endpoint, &old.fingerprint_sha256, &new)
        .await
        .expect("idempotent replacement retry");

    assert_eq!(
        std::fs::read(&known_hosts).expect("read retried file"),
        committed
    );
}

/// 旧指纹与用户所见不符或新 key 已撤销时必须拒绝，且 known_hosts 字节保持不变。
#[tokio::test]
async fn explicit_rotation_refuses_stale_old_fingerprint_and_revoked_new_key() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let old = identity(PUBLIC_KEY);
    let new = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    repository
        .trust(&endpoint, &old)
        .await
        .expect("trust original key");
    let original = std::fs::read(&known_hosts).expect("read original file");

    assert!(
        repository
            .replace(
                &endpoint,
                &identity(HASHED_HOST_KEY).fingerprint_sha256,
                &new
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(&known_hosts).expect("read after stale request"),
        original
    );

    let revoked = format!(
        "@revoked another.example {}\n{}",
        new.public_key_openssh,
        String::from_utf8_lossy(&original)
    );
    std::fs::write(&known_hosts, revoked).expect("mark candidate key revoked");
    let revoked_before = std::fs::read(&known_hosts).expect("read revoked file");
    assert!(
        repository
            .replace(&endpoint, &old.fingerprint_sha256, &new)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(&known_hosts).expect("read after revoked request"),
        revoked_before
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

/// 冻结策略把相同 revoked 公钥视为阻断状态，不能因 host pattern 或缺少正常记录而放行。
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
