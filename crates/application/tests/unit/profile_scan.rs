//! 扫描比对只新增安全项，不把已有或自定义配置悄悄覆盖。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::preferences::{ConfigRevision, TerminalProfile};

/// 测试候选使用当前平台的绝对路径，命令参数用于区分同一个可执行文件的不同入口。
/// @author kongweiguang
fn candidate(id: &str, name: &str, file: &str, args: &[&str]) -> LocalShellCandidate {
    let path = if cfg!(windows) {
        format!(r"C:\shells\{file}")
    } else {
        format!("/shells/{file}")
    };
    LocalShellCandidate::new(
        id,
        name,
        path.into(),
        args.iter().map(|arg| (*arg).into()).collect(),
    )
    .expect("合法测试候选")
}

/// 现存项包含用户的环境、工作目录、排序和 revision，便于检查建议不会丢失这些字段。
/// @author kongweiguang
fn stored(id: &str, name: &str, shell: &str, args: &[&str]) -> VersionedProfile {
    VersionedProfile {
        profile: TerminalProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            shell: shell.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: Some("projects".to_owned()),
            env: BTreeMap::from([("USER_SETTING".to_owned(), "keep".to_owned())]),
            is_default: true,
            sidebar_group_id: None,
            sort_order: 8,
            created_at: "1".to_owned(),
            updated_at: "2".to_owned(),
        },
        revision: ConfigRevision::from_sha256("old-revision".to_owned()),
    }
}

/// 仅新入口加入写入计划；稳定 ID 识别重装后的路径变化，唯一同名自定义项进入确认。
/// @author kongweiguang
#[test]
fn scan_adds_only_new_entries_and_proposes_non_destructive_updates() {
    let moved = candidate("pwsh", "PowerShell", "pwsh.exe", &["-NoLogo"]);
    let custom = candidate("cmd", "Command Prompt", "cmd.exe", &[]);
    let equivalent = candidate("bash", "Bash", "bash", &["-l"]);
    let new = candidate("fish", "Fish", "fish", &[]);
    let profiles = [
        stored(
            &moved.standard_profile_id(),
            "My PowerShell",
            "old-pwsh",
            &[],
        ),
        stored("custom", " command prompt ", "custom-cmd", &["/K", "hello"]),
        stored(
            "same",
            "My Login Shell",
            equivalent.program().to_str().unwrap(),
            &["-l"],
        ),
    ];

    let (adds, report) = plan_scan(
        &[moved.clone(), custom.clone(), equivalent, new.clone()],
        &profiles,
    );
    assert_eq!(adds.len(), 1);
    assert_eq!(
        adds[0].id.as_deref(),
        Some(new.standard_profile_id().as_str())
    );
    assert_eq!(adds[0].sort_order, 9);
    assert_eq!(report.skipped(), 0);
    let (conflicts, error) = report.into_review();
    assert!(error.is_none());
    assert_eq!(conflicts.len(), 2);
    for ((conflict, original), scanned) in conflicts.iter().zip(&profiles).zip([moved, custom]) {
        assert_eq!(conflict.current(), original);
        assert_eq!(conflict.suggested().id.as_ref(), Some(&original.profile.id));
        assert_eq!(conflict.suggested().name, original.profile.name);
        assert_eq!(
            conflict.suggested().shell,
            scanned.program().to_str().unwrap()
        );
        assert_eq!(conflict.suggested().cwd, original.profile.cwd);
        assert_eq!(conflict.suggested().env, original.profile.env);
        assert_eq!(conflict.suggested().sort_order, original.profile.sort_order);
    }
}

/// 多个同名项没有安全的覆盖目标，不能默认选中某一个或再创建同名项。
/// @author kongweiguang
#[test]
fn scan_does_not_guess_between_duplicate_profile_names() {
    let scanned = candidate("cmd", "Command Prompt", "cmd.exe", &[]);
    let profiles = [
        stored("first", "Command Prompt", "first-cmd", &[]),
        stored("second", "command prompt", "second-cmd", &[]),
    ];
    let (adds, report) = plan_scan(&[scanned], &profiles);
    assert!(adds.is_empty());
    assert_eq!(report.skipped(), 1);
    assert!(report.into_review().0.is_empty());
}
