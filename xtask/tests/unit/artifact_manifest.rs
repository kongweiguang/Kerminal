// @author kongweiguang

use super::{
    SOURCE_FINGERPRINT_ALGORITHM, SOURCE_FINGERPRINT_FORMAT, source_fingerprint,
    source_fingerprint_json, write,
};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// 每项用例都使用独立 Git 仓库，验证真实 Git 路径枚举和工作树内容，而不是复制摘要算法。
struct TemporaryGitRepository {
    root: PathBuf,
}

impl TemporaryGitRepository {
    /// 初始化带基线提交的临时仓库，并固定忽略规则以覆盖生产常用路径。
    fn create() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kerminal-source-fingerprint-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temporary repository");
        let repository = Self { root };
        repository.git(&["init", "--quiet"]);
        repository.git(&["config", "user.name", "Kerminal Tests"]);
        repository.git(&["config", "user.email", "kerminal-tests@example.invalid"]);
        fs::write(
            repository.root.join(".gitignore"),
            "/target/\n.env\n.updeng/\n",
        )
        .expect("write ignore fixture");
        fs::write(repository.root.join("Cargo.lock"), "# baseline lock\n")
            .expect("write lock fixture");
        fs::create_dir_all(repository.root.join("src")).expect("create source directory");
        fs::write(repository.root.join("src/main.rs"), "fn main() {}\n")
            .expect("write tracked source fixture");
        repository.git(&["add", ".gitignore", "Cargo.lock", "src/main.rs"]);
        repository.git(&["commit", "--quiet", "-m", "fixture baseline"]);
        repository
    }

    /// 在临时仓库执行 Git 命令并让失败直接定位到具体参数。
    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(arguments)
            .output()
            .expect("start git fixture command");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for TemporaryGitRepository {
    /// 只清理由当前测试创建的唯一临时仓库，避免触碰工作区或其它测试目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// 指纹应随 tracked 内容、Cargo.lock 和非忽略 untracked 内容分别变化。
#[test]
fn source_fingerprint_tracks_source_lockfile_and_untracked_files() {
    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let baseline = source_fingerprint(root).expect("fingerprint baseline tree");

    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"changed\"); }\n",
    )
    .expect("modify tracked source");
    let tracked_changed = source_fingerprint(root).expect("fingerprint changed source");
    assert_ne!(baseline, tracked_changed);

    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("restore source fixture");
    fs::write(root.join("Cargo.lock"), "# changed lock\n").expect("modify lockfile");
    let lock_changed = source_fingerprint(root).expect("fingerprint changed lockfile");
    assert_ne!(baseline, lock_changed);

    fs::write(root.join("Cargo.lock"), "# baseline lock\n").expect("restore lock fixture");
    fs::write(root.join("new-source.rs"), "pub fn new_source() {}\n")
        .expect("write untracked source");
    let untracked_changed = source_fingerprint(root).expect("fingerprint untracked source");
    assert_ne!(baseline, untracked_changed);
}

/// index 中仅暂存、尚未进入 HEAD 的新增路径也属于当前待构建源码快照。
#[test]
fn source_fingerprint_includes_staged_additions_and_missing_index_paths() {
    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let baseline = source_fingerprint(root).expect("fingerprint baseline tree");
    let staged_path = root.join("staged-only.rs");
    fs::write(&staged_path, "pub fn staged_only() {}\n").expect("write staged source");
    repository.git(&["add", "staged-only.rs"]);

    let staged = source_fingerprint(root).expect("fingerprint staged addition");
    assert_ne!(baseline, staged);

    fs::remove_file(staged_path).expect("remove staged source from worktree");
    let missing = source_fingerprint(root).expect("fingerprint missing index path");
    assert_ne!(staged, missing);
}

/// Git 忽略规则和 target 防线应排除 ignored、缓存及被强制跟踪的 target 内容。
#[test]
fn ignored_private_files_and_target_contents_are_not_fingerprinted() {
    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let baseline = source_fingerprint(root).expect("fingerprint baseline tree");

    fs::write(root.join(".env"), "private=first\n").expect("write ignored environment file");
    fs::create_dir_all(root.join(".updeng/private")).expect("create ignored private directory");
    fs::write(root.join(".updeng/private/data"), "private payload\n")
        .expect("write ignored private file");
    fs::create_dir_all(root.join("target/debug")).expect("create target directory");
    fs::write(root.join("target/debug/cache"), "cache\n").expect("write target cache");
    assert_eq!(
        baseline,
        source_fingerprint(root).expect("fingerprint ignored files")
    );

    fs::write(root.join("target/forced-source.rs"), "must stay excluded\n")
        .expect("write target file");
    repository.git(&["add", "--force", "target/forced-source.rs"]);
    assert_eq!(
        baseline,
        source_fingerprint(root).expect("fingerprint forced target path")
    );
}

/// 根 target 是构建输出，但嵌套 src/target 仍是合法源码并必须影响快照。
#[test]
fn nested_target_source_is_fingerprinted_but_root_target_is_excluded() {
    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let baseline = source_fingerprint(root).expect("fingerprint baseline tree");

    fs::create_dir_all(root.join("src/target")).expect("create nested source target directory");
    fs::write(
        root.join("src/target/policy.rs"),
        "pub fn policy() -> u8 { 1 }\n",
    )
    .expect("write nested target source");
    let nested_added = source_fingerprint(root).expect("fingerprint nested target source");
    assert_ne!(baseline, nested_added);

    fs::write(
        root.join("src/target/policy.rs"),
        "pub fn policy() -> u8 { 2 }\n",
    )
    .expect("modify nested target source");
    let nested_modified = source_fingerprint(root).expect("fingerprint modified nested source");
    assert_ne!(nested_added, nested_modified);

    fs::create_dir_all(root.join("target/debug")).expect("create workspace build output");
    fs::write(root.join("target/debug/cache"), "ignored build output\n")
        .expect("write workspace build output");
    assert_eq!(
        nested_modified,
        source_fingerprint(root).expect("root target output is excluded")
    );
}

/// 即使删除已 stage，HEAD 中仍存在的路径也必须以 tombstone 区分于原文件内容。
#[test]
fn missing_tracked_file_is_a_stable_tombstone_after_staged_delete() {
    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let baseline = source_fingerprint(root).expect("fingerprint baseline tree");
    repository.git(&["rm", "--cached", "--force", "src/main.rs"]);
    fs::remove_file(root.join("src/main.rs")).expect("remove tracked source file");

    let deleted = source_fingerprint(root).expect("fingerprint tracked deletion");
    assert_ne!(baseline, deleted);
    assert_eq!(
        deleted,
        source_fingerprint(root).expect("repeat fingerprint of tombstone")
    );
}

/// 快照只读取普通文件；符号链接必须被拒绝，不能跟随到其目标内容。
#[cfg(unix)]
#[test]
fn source_fingerprint_rejects_symlink_paths() {
    use std::os::unix::fs::symlink;

    let repository = TemporaryGitRepository::create();
    let root = &repository.root;
    let linked_source = root.join("src/main.rs");
    fs::remove_file(&linked_source).expect("remove tracked source fixture");
    fs::create_dir_all(root.join("target")).expect("create target fixture");
    fs::write(root.join("target/private-data"), "must not be read\n")
        .expect("write ignored link target");
    symlink(root.join("target/private-data"), &linked_source).expect("create source symlink");

    let error = source_fingerprint(root).expect_err("reject source symlink");
    assert!(error.to_string().contains("symbolic link"));
}

/// 非 UTF-8 Git 路径无法跨平台规范化时应失败关闭，而非用有损路径摘要。
#[cfg(unix)]
#[test]
fn source_fingerprint_rejects_non_utf8_paths() {
    use std::os::unix::ffi::OsStringExt;

    let repository = TemporaryGitRepository::create();
    let name = std::ffi::OsString::from_vec(vec![b'n', b'o', b'n', b'u', b't', b'f', b'8', 0xff]);
    fs::write(repository.root.join(name), "non UTF-8 path\n")
        .expect("write non-UTF-8 fixture path");

    let error = source_fingerprint(&repository.root).expect_err("reject non-UTF-8 Git path");
    assert!(error.to_string().contains("non-UTF-8 source path"));
}

/// 同一文件集合在不同创建顺序下必须生成相同摘要，证明路径集合按稳定顺序编码。
#[test]
fn source_fingerprint_is_independent_of_untracked_creation_order() {
    let first = TemporaryGitRepository::create();
    let second = TemporaryGitRepository::create();
    for name in ["zeta.rs", "alpha.rs", "middle.rs"] {
        fs::write(first.root.join(name), name).expect("write first repository file");
    }
    for name in ["middle.rs", "alpha.rs", "zeta.rs"] {
        fs::write(second.root.join(name), name).expect("write second repository file");
    }

    assert_eq!(
        source_fingerprint(&first.root).expect("fingerprint first tree"),
        source_fingerprint(&second.root).expect("fingerprint second tree")
    );
}

/// NUL 分隔的 Git 路径必须保留 Windows 也允许的空格、引号和 Unicode 文件名。
#[test]
fn source_fingerprint_accepts_nul_delimited_cross_platform_paths() {
    let repository = TemporaryGitRepository::create();
    let unusual_path = repository.root.join("space quote ' 中文.rs");
    fs::write(&unusual_path, "unusual path\n").expect("write unusual path fixture");

    let fingerprint = source_fingerprint(&repository.root).expect("fingerprint unusual path");
    assert_eq!(fingerprint.format, SOURCE_FINGERPRINT_FORMAT);
    assert_eq!(fingerprint.algorithm, SOURCE_FINGERPRINT_ALGORITHM);
    assert_eq!(fingerprint.digest.len(), 64);
}

/// Unix 文件系统允许文件名包含换行；单独覆盖 NUL 分隔解析，避免 Windows 非法路径。
#[cfg(unix)]
#[test]
fn source_fingerprint_accepts_newline_in_unix_paths() {
    let repository = TemporaryGitRepository::create();
    let unusual_path = repository.root.join("line one\nline two.rs");
    fs::write(&unusual_path, "newline path\n").expect("write Unix newline path fixture");

    let fingerprint = source_fingerprint(&repository.root).expect("fingerprint newline path");
    assert_eq!(fingerprint.format, SOURCE_FINGERPRINT_FORMAT);
    assert_eq!(fingerprint.algorithm, SOURCE_FINGERPRINT_ALGORITHM);
    assert_eq!(fingerprint.digest.len(), 64);
}

/// 输出清单保留 schema v1 的 revision/dirty，并额外写入当前源码指纹与真实包体摘要。
#[test]
fn artifact_manifest_adds_source_fingerprint_without_changing_schema_version() {
    let repository = TemporaryGitRepository::create();
    let bundle = repository.root.join("target/package-output");
    fs::create_dir_all(&bundle).expect("create package output fixture");
    fs::write(bundle.join("candidate.bin"), b"candidate bytes").expect("write package artifact");
    let output = repository.root.join("target/artifact-manifest.json");

    write(&repository.root, &bundle, &output).expect("write artifact manifest");
    let document: Value = serde_json::from_slice(&fs::read(&output).expect("read manifest"))
        .expect("parse artifact manifest");
    let source = document.get("source").expect("source metadata");
    let fingerprint = source.get("fingerprint").expect("source fingerprint");
    assert_eq!(document["schemaVersion"], 1);
    assert!(
        source["revision"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(source["dirty"], false);
    assert_eq!(fingerprint["format"], SOURCE_FINGERPRINT_FORMAT);
    assert_eq!(fingerprint["algorithm"], SOURCE_FINGERPRINT_ALGORITHM);
    assert_eq!(fingerprint["digest"].as_str().map(str::len), Some(64));
    assert_eq!(
        document["artifacts"][0]["sha256"].as_str().map(str::len),
        Some(64)
    );

    fs::write(
        repository.root.join("src/main.rs"),
        "fn main() { /* dirty */ }\n",
    )
    .expect("modify source after clean manifest");
    write(&repository.root, &bundle, &output).expect("rewrite manifest for dirty source");
    let dirty_document: Value =
        serde_json::from_slice(&fs::read(output).expect("read dirty manifest"))
            .expect("parse dirty manifest");
    assert_eq!(dirty_document["source"]["revision"], source["revision"]);
    assert_eq!(dirty_document["source"]["dirty"], true);
    assert_ne!(
        dirty_document["source"]["fingerprint"]["digest"],
        fingerprint["digest"]
    );
}

/// 独立命令输出紧凑 JSON，供 release 调用方比较前后快照而不解析人类说明。
#[test]
fn source_fingerprint_cli_payload_is_machine_readable() {
    let repository = TemporaryGitRepository::create();
    let output = source_fingerprint_json(&repository.root).expect("serialize source fingerprint");
    let document: Value = serde_json::from_str(&output).expect("parse source fingerprint JSON");

    assert_eq!(document["format"], SOURCE_FINGERPRINT_FORMAT);
    assert_eq!(document["algorithm"], SOURCE_FINGERPRINT_ALGORITHM);
    assert_eq!(document["digest"].as_str().map(str::len), Some(64));
}
