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
const RSA_PUBLIC_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCpVx5I2Qqp6fuuV1kE52z7uiCTyWBeRZKMU50yKdvzcqFkdtlCiPPABxIzReRPNm4bLyif8OxDlufhgQ765BFX12x+SrmkcraBwuLxmOo7SRj0oHN3g5wSNVX9bZKxfWLmXArD88TuFLHcnSTV0wHGy3aIqX+Uo6ltCw/OLt8Yv+2V/r6F88e0dmWUHzHqq6E62huWYHW0uW09eO6fyao34IonUsl9udfZ5aW+ZuaRaL/GIbG2k150J9aCOQhjLmWMWjm6K+cNcU/W3jrIQhfZPe+2gSUkWB9EX6JKNp7TWIIMfMsffCYdMiuwsjzxYD27WB2kULXp5zR1Zc645srf";

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
    classify_key(endpoint, identity, &path)
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
        .record(&endpoint, &key_identity)
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
        .record(&endpoint, &key_identity)
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
        .record(&endpoint, &first)
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
        .record(&endpoint, &changed)
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
        .record(&endpoint, &key_identity)
        .await
        .expect("seed writable known_hosts");
    let readonly = KnownHostsStore::new(StorageCoordinator::new(directory.path()), false);

    readonly
        .record(&endpoint, &key_identity)
        .await
        .expect("known key requires no write access");
    assert!(
        readonly
            .record(&endpoint, &identity(OTHER_PUBLIC_KEY))
            .await
            .is_err(),
        "unknown keys still report the read-only storage error for the caller to downgrade"
    );
}

/// 当前 key 的 @revoked 历史标记不再阻断记录；同一 endpoint 会追加正常记录并转为 Known。
#[tokio::test]
async fn revoked_record_does_not_block_new_record() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let key_identity = identity(PUBLIC_KEY);
    let revoked = format!(
        "@revoked example.test {}\n",
        key_identity.public_key_openssh
    );
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    std::fs::write(&known_hosts, &revoked).expect("write revoked fixture");

    repository
        .record(&endpoint, &key_identity)
        .await
        .expect("revoked marker must not block recording the observed key");
    assert!(matches!(
        repository
            .inspect(&endpoint, &key_identity)
            .await
            .expect("inspect recorded key"),
        HostKeyStatus::Known
    ));
    let content = std::fs::read_to_string(&known_hosts).expect("read recorded fixture");
    assert!(
        content.contains("@revoked example.test"),
        "撤销标记原样保留"
    );
    assert!(
        content
            .lines()
            .any(|line| line.starts_with("example.test ")),
        "撤销标记之外仍应追加正常记录"
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

/// 撤销记录不再参与分类：只有 @revoked 行的 endpoint 属于 Unknown，可以追加正常记录。
#[test]
fn revoked_only_record_is_unknown() {
    let key_identity = identity(PUBLIC_KEY);
    let content = b"@revoked example.test ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ\n";
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    assert!(matches!(
        classify_fixture(&endpoint, &key_identity, content).expect("classify revoked"),
        HostKeyStatus::Unknown
    ));
}

/// 同一 endpoint 的 rsa 与 ed25519 必须独立轮换：替换 ed25519 时保留 rsa 记录。
#[tokio::test]
async fn rotation_replaces_only_same_algorithm_record() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let rsa = identity(RSA_PUBLIC_KEY);
    let ed25519 = identity(PUBLIC_KEY);
    let rotated = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    let original = format!(
        "example.test {}\nexample.test {}\nexample.test {}\nother.example {}\n",
        rsa.public_key_openssh,
        ed25519.public_key_openssh,
        ed25519.public_key_openssh,
        ed25519.public_key_openssh
    );
    std::fs::write(&known_hosts, &original).expect("write multi-algorithm fixture");

    repository
        .record(&endpoint, &rotated)
        .await
        .expect("rotate ed25519 while rsa stays valid");
    assert!(matches!(
        repository
            .inspect(&endpoint, &rotated)
            .await
            .expect("inspect rotated key"),
        HostKeyStatus::Known
    ));
    let content = std::fs::read_to_string(&known_hosts).expect("read rotated fixture");
    assert!(
        content.contains(&rsa.public_key_openssh),
        "rsa 记录必须保留"
    );
    assert!(
        !content.contains(&format!("example.test {}", ed25519.public_key_openssh)),
        "同算法的旧 ed25519 记录都必须移除"
    );
    assert!(
        content.contains(&format!("other.example {}", ed25519.public_key_openssh)),
        "其它主机的同算法记录不受影响"
    );
}

/// russh 跳过注释但不递增行号；有前置注释时替换必须落在真实记录行，不能删错注释或邻居。
#[tokio::test]
async fn rotation_with_leading_comment_replaces_real_record_line() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let previous = identity(PUBLIC_KEY);
    let rotated = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    let original = format!(
        "# managed by fixture\nfirst.example {}\nexample.test {}\nlast.example {}\n",
        identity(HASHED_HOST_KEY).public_key_openssh,
        previous.public_key_openssh,
        previous.public_key_openssh
    );
    std::fs::write(&known_hosts, &original).expect("write commented fixture");

    repository
        .record(&endpoint, &rotated)
        .await
        .expect("comment must not shift the replaced line");
    let content = std::fs::read_to_string(&known_hosts).expect("read replaced fixture");
    assert_eq!(
        content.lines().next(),
        Some("# managed by fixture"),
        "注释行保持原样"
    );
    assert!(
        content.contains(&format!(
            "first.example {}",
            identity(HASHED_HOST_KEY).public_key_openssh
        )),
        "注释后的其它主机行保持不变"
    );
    assert!(
        content.contains(&format!("last.example {}", previous.public_key_openssh)),
        "注释后的邻居行保持不变"
    );
    assert!(
        content.contains(&format!("example.test {}", rotated.public_key_openssh)),
        "目标 endpoint 替换为新 key"
    );
    assert!(!content.contains(&format!("example.test {}", previous.public_key_openssh)));
}

/// 无关主机、端口和注释字节必须保持不变；替换只动当前 endpoint 的同算法记录。
#[tokio::test]
async fn rotation_preserves_unrelated_lines_and_port_identity() {
    let directory = tempdir().expect("temp directory");
    let repository = store(directory.path());
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 2222,
        username: "test".to_owned(),
    };
    let previous = identity(PUBLIC_KEY);
    let rotated = identity(OTHER_PUBLIC_KEY);
    let known_hosts = directory.path().join(KNOWN_HOSTS_FILE);
    let unrelated = format!(
        "# header\r\nexample.test {} \r\n[example.test]:2222 {} \r\nother.example {}\r\n",
        previous.public_key_openssh, previous.public_key_openssh, previous.public_key_openssh
    );
    std::fs::write(&known_hosts, &unrelated).expect("write unrelated fixture");

    repository
        .record(&endpoint, &rotated)
        .await
        .expect("replace only the non-default port record");
    let content = std::fs::read_to_string(&known_hosts).expect("read preserved fixture");
    assert!(content.starts_with("# header\r\n"), "注释和 CRLF 保留");
    assert!(
        content.contains(&format!(
            "example.test {} \r\n",
            previous.public_key_openssh
        )),
        "port 22 的同主机记录不受影响"
    );
    assert!(
        content.contains(&format!(
            "other.example {}\r\n",
            previous.public_key_openssh
        )),
        "其它主机记录不受影响"
    );
    assert!(!content.contains(&format!(
        "[example.test]:2222 {}",
        previous.public_key_openssh
    )));
    assert!(content.contains(&format!(
        "[example.test]:2222 {}",
        rotated.public_key_openssh
    )));
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
