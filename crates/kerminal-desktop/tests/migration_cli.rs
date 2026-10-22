//! 维护 console 的真实子进程边界；所有数据均位于临时根，Windows 不显示控制台窗口。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::ffi::OsString;

use serde_json::Value;

/// 子进程只运行维护 bin 且显式指定临时根；有界等待失败时仅回收本测试拥有的进程。
///
/// @author kongweiguang
fn run_migration(root: &Path, mode: &str) -> Output {
    run_migration_from(root, mode, None)
}

/// 允许只改变目标子进程的工作目录，避免并发测试共享进程级 current_dir。
///
/// @author kongweiguang
fn run_migration_from(root: &Path, mode: &str, current_dir: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kerminal-migrate"));
    command.args([OsStr::new("--root"), root.as_os_str(), OsStr::new(mode)]);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    run_bounded(command)
}

/// 共用隐藏窗口和有界等待策略，超时只终止本测试创建的维护子进程。
///
/// @author kongweiguang
fn run_bounded(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().expect("spawn maintenance console");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child
            .try_wait()
            .expect("poll maintenance console")
            .is_some()
        {
            return child.wait_with_output().expect("collect maintenance JSON");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("maintenance console did not exit within its test deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// 保存复制到临时发布布局的脚本与 helper；其目录在所有调用完成前保持有效。
///
/// @author kongweiguang
#[cfg(windows)]
struct PackagedMigrationWrapper {
    _fixture: tempfile::TempDir,
    script_path: PathBuf,
}

#[cfg(windows)]
impl PackagedMigrationWrapper {
    /// 在隔离目录复用真实包装脚本和 Cargo 提供的真实 helper，验证打包相邻文件契约。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        let fixture = tempfile::tempdir().expect("isolated packaged wrapper fixture");
        let packaging_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packaging")
            .join("kerminal-migrate.ps1");
        let script_path = fixture.path().join("kerminal-migrate.ps1");
        let helper_path = fixture.path().join("kerminal-migrate.exe");
        fs::copy(&packaging_script, &script_path).expect("copy packaged migration script");
        fs::copy(env!("CARGO_BIN_EXE_kerminal-migrate"), &helper_path)
            .expect("copy real migration helper beside script");
        Self {
            _fixture: fixture,
            script_path,
        }
    }

    /// 通过 Windows PowerShell 5.1 执行脚本，根目录始终显式传入且不启动桌面 UI。
    ///
    /// @author kongweiguang
    fn run(&self, root: &OsStr, mode: &str) -> Output {
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&self.script_path)
            .arg("-Root")
            .arg(root)
            .arg(mode);
        run_bounded(command)
    }

    /// 用 PowerShell 的 `-Name:value` 形式把 flag 外观字符串明确绑定为根目录参数值。
    ///
    /// @author kongweiguang
    fn run_with_flag_like_root(&self) -> Output {
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&self.script_path)
            .arg("-Root:--dry-run")
            .arg("-DryRun");
        run_bounded(command)
    }
}

/// 在临时发布目录中保存真实 POSIX 包装脚本和迁移 helper，避免访问用户配置目录。
///
/// @author kongweiguang
#[cfg(unix)]
struct PackagedUnixMigrationWrapper {
    _fixture: tempfile::TempDir,
    script_path: PathBuf,
}

#[cfg(unix)]
impl PackagedUnixMigrationWrapper {
    /// 复制真实产物布局并补齐可执行位，验证包装脚本能启动同目录 helper。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("isolated packaged Unix wrapper fixture");
        let packaging_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packaging")
            .join("kerminal-migrate.sh");
        let script_path = fixture.path().join("kerminal-migrate.sh");
        let helper_path = fixture.path().join("kerminal-migrate");
        fs::copy(&packaging_script, &script_path).expect("copy packaged Unix migration script");
        fs::copy(env!("CARGO_BIN_EXE_kerminal-migrate"), &helper_path)
            .expect("copy real Unix migration helper beside script");

        for packaged_path in [&script_path, &helper_path] {
            let mut permissions = fs::metadata(packaged_path)
                .expect("read packaged file permissions")
                .permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            fs::set_permissions(packaged_path, permissions)
                .expect("preserve executable packaged file permissions");
        }

        Self {
            _fixture: fixture,
            script_path,
        }
    }

    /// 通过系统 sh 执行真实包装脚本，并将显式根目录与模式作为独立参数传入。
    ///
    /// @author kongweiguang
    fn run(&self, root: &OsStr, mode: &str) -> Output {
        let mut command = Command::new("sh");
        command
            .arg(&self.script_path)
            .arg("--root")
            .arg(root)
            .arg(mode);
        run_bounded(command)
    }
}

/// 只接受单条安全 JSON，防止维护入口意外启动日志服务或把源错误写入协议输出。
///
/// @author kongweiguang
fn document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "maintenance must not initialize runtime logging"
    );
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    serde_json::from_slice(&output.stdout).expect("one maintenance JSON document")
}

/// 按相对名称和原字节记录已有目录，用于证明 dry-run 不新增锁、备份、日志或 SQLite 辅助文件。
///
/// @author kongweiguang
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).expect("read isolated root") {
            let entry = entry.expect("isolated directory entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("relative test path")
                .to_path_buf();
            if entry.file_type().expect("entry type").is_dir() {
                result.insert(relative, None);
                directories.push(path);
            } else {
                result.insert(relative, Some(fs::read(path).expect("fixture bytes")));
            }
        }
    }
    result
}

/// 新根检查不能借助 mkdir 或 storage lock 推断状态，中文带空格路径也须保持为一个 OS 参数。
///
/// @author kongweiguang
#[test]
fn dry_run_does_not_create_a_missing_root() {
    let fixture = tempfile::tempdir().expect("isolated CLI fixture");
    let root = fixture.path().join("尚不存在的数据 根");
    let output = run_migration(&root, "--dry-run");
    let document = document(&output);
    assert!(output.status.success());
    assert_eq!(document["ok"], true);
    assert_eq!(document["mode"], "dry_run");
    assert_eq!(document["inspection"]["readiness"], "fresh");
    assert_eq!(document["inspection"]["recoveryPending"], false);
    assert!(
        !root.exists(),
        "dry-run must not initialize even an empty root"
    );
    assert_eq!(
        fs::read_dir(fixture.path())
            .expect("fixture listing")
            .count(),
        0
    );
}

/// 已有空根的检查也不能落盘锁或模板，读取本身必须与维护 apply 分开。
///
/// @author kongweiguang
#[test]
fn dry_run_keeps_an_existing_root_byte_for_byte() {
    let root = tempfile::tempdir().expect("isolated existing root");
    let before = snapshot(root.path());
    let output = run_migration(root.path(), "--dry-run");
    assert!(output.status.success());
    assert_eq!(document(&output)["ok"], true);
    assert_eq!(snapshot(root.path()), before);
}

/// 参数错误先于根解析与迁移执行，错误 JSON 不回显未知输入或潜在敏感路径。
///
/// @author kongweiguang
#[test]
fn invalid_mode_fails_without_creating_or_echoing_the_root() {
    let fixture = tempfile::tempdir().expect("isolated invalid argv");
    let root = fixture.path().join("private-test-root");
    let output = run_migration(&root, "--not-a-mode");
    let document = document(&output);
    assert!(!output.status.success());
    assert_eq!(document["ok"], false);
    assert_eq!(document["code"], "invalid");
    assert!(document["message"].is_string());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-test-root"));
    assert!(!root.exists());
}

/// 不明非空根必须以 Blocked 失败退出，预检不能创建恢复材料或在 JSON 中回显原始文件正文。
///
/// @author kongweiguang
#[test]
fn blocked_dry_run_reports_failure_without_modifying_unknown_data() {
    let root = tempfile::tempdir().expect("isolated unknown root");
    fs::write(
        root.path().join("unknown-data.txt"),
        b"PUBLIC_UNKNOWN_FIXTURE_CONTENT",
    )
    .expect("unknown isolated data");
    let before = snapshot(root.path());
    let output = run_migration(root.path(), "--dry-run");
    let document = document(&output);
    assert!(!output.status.success());
    assert_eq!(document["ok"], false);
    assert_eq!(document["mode"], "dry_run");
    assert_eq!(document["inspection"]["readiness"], "blocked");
    assert!(
        document["inspection"]["blockers"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PUBLIC_UNKNOWN_FIXTURE_CONTENT"));
    assert_eq!(snapshot(root.path()), before);
}

/// Fresh 初始化和后续幂等 apply 由同一真实用例完成，第二次不能改写已经提交的数据。
///
/// @author kongweiguang
#[test]
fn apply_initializes_once_then_reports_already_current() {
    let fixture = tempfile::tempdir().expect("isolated apply fixture");
    let root = fixture.path().join("中文 apply root");
    let first = run_migration(&root, "--apply");
    let first_document = document(&first);
    assert!(first.status.success());
    assert_eq!(first_document["ok"], true);
    assert_eq!(first_document["mode"], "apply");
    assert_eq!(first_document["report"]["outcome"], "initialized");
    assert!(root.is_dir());
    let committed = snapshot(&root);
    assert!(
        !committed.is_empty(),
        "successful initialization must commit its format marker"
    );

    let second = run_migration(&root, "--apply");
    let second_document = document(&second);
    assert!(second.status.success());
    assert_eq!(second_document["ok"], true);
    assert_eq!(second_document["report"]["outcome"], "alreadyCurrent");
    assert_eq!(second_document["report"]["changedFileCount"], 0);
    assert_eq!(snapshot(&root), committed);

    let inspection = run_migration(&root, "--dry-run");
    assert!(inspection.status.success());
    let inspection_document = document(&inspection);
    assert_eq!(inspection_document["ok"], true);
    assert_eq!(inspection_document["inspection"]["readiness"], "current");
    assert_eq!(snapshot(&root), committed);
}

/// 相对根按 child current_dir 解析；初始化产生的每个文件都必须留在该根目录内。
///
/// @author kongweiguang
#[test]
fn relative_root_is_resolved_from_child_working_directory() {
    let fixture = tempfile::tempdir().expect("isolated relative-root fixture");
    let relative_root = Path::new("./中文 数据根");
    let root = fixture.path().join("中文 数据根");
    let output = run_migration_from(relative_root, "--apply", Some(fixture.path()));
    let result = document(&output);

    assert!(output.status.success());
    assert_eq!(result["report"]["outcome"], "initialized");
    assert!(root.is_dir());
    let children = fs::read_dir(fixture.path())
        .expect("read isolated working directory")
        .map(|entry| entry.expect("working directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0].as_os_str(),
        OsStr::new("中文 数据根"),
        "the helper must not create siblings beside its configured root"
    );
    assert!(
        snapshot(fixture.path())
            .keys()
            .all(|path| path.starts_with(OsStr::new("中文 数据根")))
    );
}

/// Windows 包装脚本必须将含中文、空格和尾部分隔符的根作为一个参数，并保持 dry-run 零写入。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn powershell_wrapper_dry_run_preserves_chinese_spaced_trailing_separator_root() {
    let wrapper = PackagedMigrationWrapper::new();
    let fixture = tempfile::tempdir().expect("isolated wrapper dry-run fixture");
    let root = fixture.path().join("中文 dry run 根");
    let root_argument = OsString::from(format!("{}\\", root.display()));
    let before = snapshot(fixture.path());

    let output = wrapper.run(root_argument.as_os_str(), "-DryRun");
    let result = document(&output);

    assert!(output.status.success());
    assert_eq!(result["ok"], true);
    assert_eq!(result["mode"], "dry_run");
    assert_eq!(result["inspection"]["readiness"], "fresh");
    assert!(!root.exists());
    assert_eq!(snapshot(fixture.path()), before);
}

/// Windows PowerShell wrapper 必须真实透传 apply 终态和 helper 的中文 JSON 错误。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn powershell_wrapper_applies_idempotently_and_preserves_chinese_error_json() {
    let wrapper = PackagedMigrationWrapper::new();
    let fixture = tempfile::tempdir().expect("isolated wrapper apply fixture");
    let root = fixture.path().join("中文 apply root");

    let first = wrapper.run(root.as_os_str(), "-Apply");
    let first_document = document(&first);
    assert!(first.status.success());
    assert_eq!(first_document["ok"], true);
    assert_eq!(first_document["report"]["outcome"], "initialized");
    let committed = snapshot(&root);

    let second = wrapper.run(root.as_os_str(), "-Apply");
    let second_document = document(&second);
    assert!(second.status.success());
    assert_eq!(second_document["ok"], true);
    assert_eq!(second_document["report"]["outcome"], "alreadyCurrent");
    assert_eq!(second_document["report"]["changedFileCount"], 0);
    assert_eq!(snapshot(&root), committed);

    // 这个保留参数专门触发真实 helper 的中文参数错误，且不会访问任何数据根。
    let direct = run_migration(Path::new("--dry-run"), "--dry-run");
    let wrapped = wrapper.run_with_flag_like_root();
    let direct_document = document(&direct);
    let wrapped_document = document(&wrapped);
    assert!(!direct.status.success());
    assert!(!wrapped.status.success());
    assert_eq!(direct_document["ok"], false);
    assert_eq!(direct_document["code"], "invalid");
    assert_eq!(direct_document["message"], "迁移根目录缺少有效参数");
    assert_eq!(wrapped_document, direct_document);
}

/// POSIX 包装必须对临时中文路径零写入预检、幂等初始化，并原样透传 helper 错误 JSON。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn unix_wrapper_dry_runs_applies_idempotently_and_preserves_error_json() {
    let wrapper = PackagedUnixMigrationWrapper::new();
    let fixture = tempfile::tempdir().expect("isolated Unix wrapper fixture");
    let root = fixture.path().join("中文 migration data 根");
    let before = snapshot(fixture.path());

    let dry_run = wrapper.run(root.as_os_str(), "--dry-run");
    let dry_run_document = document(&dry_run);
    assert!(dry_run.status.success());
    assert_eq!(dry_run_document["ok"], true);
    assert_eq!(dry_run_document["mode"], "dry_run");
    assert_eq!(dry_run_document["inspection"]["readiness"], "fresh");
    assert!(!root.exists());
    assert_eq!(snapshot(fixture.path()), before);

    let first = wrapper.run(root.as_os_str(), "--apply");
    let first_document = document(&first);
    assert!(first.status.success());
    assert_eq!(first_document["ok"], true);
    assert_eq!(first_document["report"]["outcome"], "initialized");
    let committed = snapshot(&root);

    let second = wrapper.run(root.as_os_str(), "--apply");
    let second_document = document(&second);
    assert!(second.status.success());
    assert_eq!(second_document["ok"], true);
    assert_eq!(second_document["report"]["outcome"], "alreadyCurrent");
    assert_eq!(second_document["report"]["changedFileCount"], 0);
    assert_eq!(snapshot(&root), committed);

    // 同一无效根参数触发 Rust helper 的中文参数错误，不读取或写入任何数据目录。
    let direct = run_migration(Path::new("--dry-run"), "--dry-run");
    let wrapped = wrapper.run(OsStr::new("--dry-run"), "--dry-run");
    let direct_document = document(&direct);
    let wrapped_document = document(&wrapped);
    assert!(!direct.status.success());
    assert!(!wrapped.status.success());
    assert_eq!(direct_document["ok"], false);
    assert_eq!(direct_document["code"], "invalid");
    assert_eq!(direct_document["message"], "迁移根目录缺少有效参数");
    assert_eq!(wrapped_document, direct_document);
}
