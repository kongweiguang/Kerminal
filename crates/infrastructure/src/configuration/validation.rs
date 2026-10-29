//! 配置启动恢复扫描与只读 schema 诊断；仓储锁和原子提交仍由 facade 管理。
//!
//! @author kongweiguang

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kerminal_application::AppResult;
use kerminal_application::connections::management::{
    host_note_is_valid, is_reserved_serial_host_tag,
};
use kerminal_application::preferences::{
    ConfigurationValidationDiagnostic, ConfigurationValidationReport, ConfigurationValidationScope,
    ConfigurationValidationSeverity,
};
use kerminal_domain::preferences::{
    ConfigDocumentPath, ConfigurationIssue, ConfigurationIssueKind, ConfigurationMode,
    ConfigurationRecoveryState,
};

use super::{read_configuration_file, safe_relative_path, unreadable_issue};

const SETTINGS_SCHEMA_VERSION: i64 = 1;
const HOST_SCHEMA_VERSION: i64 = 3;

/// 扫描所有用户配置目录；可解析的未知字段不阻断启动，损坏项则固定整个仓储为只读恢复。
pub(super) fn inspect_startup_configuration(root: &Path) -> ConfigurationRecoveryState {
    let mut issues = Vec::new();
    inspect_startup_file(
        root,
        Path::new("settings.toml"),
        ConfigDocumentPath::Settings,
        &mut issues,
    );

    inspect_startup_file(
        root,
        Path::new("hosts/groups.toml"),
        ConfigDocumentPath::HostGroups,
        &mut issues,
    );
    inspect_startup_directory(root, "profiles", ConfigDirectoryKind::Profiles, &mut issues);
    inspect_startup_directory(root, "hosts", ConfigDirectoryKind::Hosts, &mut issues);
    inspect_startup_directory(root, "snippets", ConfigDirectoryKind::Snippets, &mut issues);
    inspect_startup_directory(
        root,
        "workflows",
        ConfigDirectoryKind::Workflows,
        &mut issues,
    );
    issues.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mode = if issues.is_empty() {
        ConfigurationMode::ReadWrite
    } else {
        ConfigurationMode::ReadOnlyRecovery
    };
    ConfigurationRecoveryState { mode, issues }
}

/// 目录扫描只枚举已知 `.toml` 文档；目录和条目 symlink 遵循旧读取语义，但坏链接会报告。
fn inspect_startup_directory(
    root: &Path,
    relative_directory: &str,
    kind: ConfigDirectoryKind,
    issues: &mut Vec<ConfigurationIssue>,
) {
    let directory = root.join(relative_directory);
    let entry_metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            issues.push(unreadable_issue(relative_directory.to_owned()));
            return;
        }
    };
    let metadata = if entry_metadata.file_type().is_symlink() {
        match fs::metadata(&directory) {
            Ok(metadata) => metadata,
            Err(_) => {
                issues.push(unreadable_issue(relative_directory.to_owned()));
                return;
            }
        }
    } else {
        entry_metadata
    };
    if !metadata.is_dir() {
        issues.push(unreadable_issue(relative_directory.to_owned()));
        return;
    }

    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            issues.push(unreadable_issue(relative_directory.to_owned()));
            return;
        }
    };
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.as_ref().map(|entry| entry.file_name()).ok());
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                issues.push(unreadable_issue(relative_directory.to_owned()));
                continue;
            }
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("toml")) {
            continue;
        }
        if kind == ConfigDirectoryKind::Hosts && entry.file_name() == OsStr::new("groups.toml") {
            continue;
        }

        let relative = PathBuf::from(relative_directory).join(entry.file_name());
        let display_path = safe_relative_path(&relative);
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                issues.push(unreadable_issue(display_path));
                continue;
            }
        };
        if !file_type.is_file() && !file_type.is_symlink() {
            issues.push(unreadable_issue(display_path));
            continue;
        }
        let Some(id) = path.file_stem().and_then(OsStr::to_str) else {
            issues.push(unreadable_issue(display_path));
            continue;
        };
        inspect_startup_file(root, &relative, kind.document_path(id), issues);
    }
}

/// 按目录选择磁盘 schema；groups.toml 由固定路径单独检查，不会误当作 host ID。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigDirectoryKind {
    Profiles,
    Hosts,
    Snippets,
    Workflows,
}

impl ConfigDirectoryKind {
    /// 把已枚举且经过路径约束的文件 stem 映射成业务文档类型。
    fn document_path(self, id: &str) -> ConfigDocumentPath {
        match self {
            Self::Profiles => ConfigDocumentPath::Profile(id.to_owned()),
            Self::Hosts => ConfigDocumentPath::Host(id.to_owned()),
            Self::Snippets => ConfigDocumentPath::Snippet(id.to_owned()),
            Self::Workflows => ConfigDocumentPath::Workflow(id.to_owned()),
        }
    }
}

/// 只读 validator 的安全文件坐标；absolute path 永不进入诊断结果。
struct ValidationFile {
    path: ConfigDocumentPath,
    relative: PathBuf,
    label: String,
}

/// 按业务 scope 复用当前 schema validator 扫描配置树，并补上跨文件关联检查。
///
/// @author kongweiguang
pub(super) fn validate_configuration_root(
    root: &Path,
    requested: ConfigurationValidationScope,
) -> AppResult<ConfigurationValidationReport> {
    let mut report = ConfigurationValidationReport::new(requested);
    let needs_groups = requested.includes(ConfigurationValidationScope::Profiles)
        || requested.includes(ConfigurationValidationScope::Hosts);
    let groups_value = needs_groups.then(|| {
        read_validation_document(
            root,
            ConfigDocumentPath::HostGroups,
            Path::new("hosts/groups.toml"),
            ConfigurationValidationScope::Hosts,
            &mut report,
        )
    });
    let group_entries = groups_value
        .as_ref()
        .and_then(Option::as_ref)
        .and_then(|value| value.get("groups"))
        .and_then(toml::Value::as_array);
    let mut group_ids = std::collections::BTreeSet::new();
    if let Some(entries) = group_entries {
        for entry in entries {
            if let Some(id) = entry.get("id").and_then(toml::Value::as_str) {
                group_ids.insert(id.to_owned());
            }
        }
    }

    if requested.includes(ConfigurationValidationScope::Settings) {
        let settings = read_validation_document(
            root,
            ConfigDocumentPath::Settings,
            Path::new("settings.toml"),
            ConfigurationValidationScope::Settings,
            &mut report,
        );
        report.record_checked(
            ConfigurationValidationScope::Settings,
            "settings.toml",
            if settings.is_some() {
                "settings loaded"
            } else {
                "settings defaults active"
            },
        );
    }

    if requested.includes(ConfigurationValidationScope::Profiles) {
        let files = validation_directory_files(
            root,
            "profiles",
            ConfigDirectoryKind::Profiles,
            &mut report,
        );
        let mut loaded = 0usize;
        for file in files {
            let Some(value) = read_validation_document(
                root,
                file.path,
                &file.relative,
                ConfigurationValidationScope::Profiles,
                &mut report,
            ) else {
                continue;
            };
            loaded += 1;
            if value
                .get("sidebar_group_id")
                .and_then(toml::Value::as_str)
                .is_some_and(|group_id| !group_ids.contains(group_id))
            {
                report.record(ConfigurationValidationDiagnostic {
                    severity: ConfigurationValidationSeverity::Error,
                    scope: ConfigurationValidationScope::Profiles,
                    path: file.label,
                    message: "sidebar_group_id does not reference hosts/groups.toml".into(),
                    line: None,
                    column: None,
                    key: Some("sidebar_group_id".to_owned()),
                    recovery: Some(
                        "Add the group to hosts/groups.toml or clear sidebar_group_id.".to_owned(),
                    ),
                });
            }
        }
        report.record_checked(
            ConfigurationValidationScope::Profiles,
            "profiles/*.toml",
            format!("{loaded} profile(s) loaded"),
        );
    }

    if requested.includes(ConfigurationValidationScope::Hosts) {
        let raw_group_count = group_entries.map_or(0, |entries| entries.len());
        report.record_checked(
            ConfigurationValidationScope::Hosts,
            "hosts/groups.toml",
            format!("{raw_group_count} group(s) loaded"),
        );
        if let Some(entries) = group_entries {
            let mut seen = std::collections::BTreeSet::new();
            for entry in entries {
                let Some(id) = entry.get("id").and_then(toml::Value::as_str) else {
                    continue;
                };
                if !seen.insert(id) {
                    report.record(ConfigurationValidationDiagnostic {
                        severity: ConfigurationValidationSeverity::Error,
                        scope: ConfigurationValidationScope::Hosts,
                        path: "hosts/groups.toml".into(),
                        message: "duplicate group id".into(),
                        line: None,
                        column: None,
                        key: Some("id".to_owned()),
                        recovery: Some("Keep each hosts/groups.toml group id unique.".to_owned()),
                    });
                }
            }
        }
        validate_host_secret_ignore_rule(root, &mut report);
        let files =
            validation_directory_files(root, "hosts", ConfigDirectoryKind::Hosts, &mut report);
        let mut loaded = 0usize;
        for file in files {
            let Some(value) = read_validation_document(
                root,
                file.path,
                &file.relative,
                ConfigurationValidationScope::Hosts,
                &mut report,
            ) else {
                continue;
            };
            loaded += 1;
            if value
                .get("group_id")
                .and_then(toml::Value::as_str)
                .is_some_and(|group_id| !group_ids.contains(group_id))
            {
                report.record(ConfigurationValidationDiagnostic {
                    severity: ConfigurationValidationSeverity::Error,
                    scope: ConfigurationValidationScope::Hosts,
                    path: file.label,
                    message: "group_id does not reference hosts/groups.toml".into(),
                    line: None,
                    column: None,
                    key: Some("group_id".to_owned()),
                    recovery: Some(
                        "Add the group to hosts/groups.toml or clear group_id.".to_owned(),
                    ),
                });
            }
        }
        report.record_checked(
            ConfigurationValidationScope::Hosts,
            "hosts/*.toml",
            format!("{loaded} public host file(s) loaded"),
        );
    }

    if requested.includes(ConfigurationValidationScope::Snippets) {
        let files = validation_directory_files(
            root,
            "snippets",
            ConfigDirectoryKind::Snippets,
            &mut report,
        );
        let mut loaded = 0usize;
        for file in files {
            if read_validation_document(
                root,
                file.path,
                &file.relative,
                ConfigurationValidationScope::Snippets,
                &mut report,
            )
            .is_some()
            {
                loaded += 1;
            }
        }
        report.record_checked(
            ConfigurationValidationScope::Snippets,
            "snippets/*.toml",
            format!("{loaded} snippet(s) loaded"),
        );
    }

    if requested.includes(ConfigurationValidationScope::Workflows) {
        let files = validation_directory_files(
            root,
            "workflows",
            ConfigDirectoryKind::Workflows,
            &mut report,
        );
        let mut loaded = 0usize;
        for file in files {
            let label = file.label.clone();
            let Some(value) = read_validation_document(
                root,
                file.path,
                &file.relative,
                ConfigurationValidationScope::Workflows,
                &mut report,
            ) else {
                continue;
            };
            loaded += 1;
            validate_workflow_sequence(&value, &label, &mut report);
        }
        report.record_checked(
            ConfigurationValidationScope::Workflows,
            "workflows/*.toml",
            format!("{loaded} workflow(s) loaded"),
        );
    }

    if requested.includes(ConfigurationValidationScope::Profiles)
        && root
            .join(super::profile_default_transaction::profile_default_journal_path())
            .exists()
    {
        report.record(ConfigurationValidationDiagnostic {
            severity: ConfigurationValidationSeverity::Error,
            scope: ConfigurationValidationScope::Profiles,
            path: ".storage-transactions/profile-default.json".into(),
            message: "default profile transaction needs recovery".into(),
            line: None,
            column: None,
            key: None,
            recovery: Some(
                "Restart the application to recover the pending profile transaction.".to_owned(),
            ),
        });
    }
    Ok(report)
}

/// 读取单个安全枚举配置文件并用当前 schema validator 验证，失败只追加脱敏诊断。
///
/// @author kongweiguang
fn read_validation_document(
    root: &Path,
    document_path: ConfigDocumentPath,
    relative: &Path,
    scope: ConfigurationValidationScope,
    report: &mut ConfigurationValidationReport,
) -> Option<toml::Value> {
    let label = safe_relative_path(relative);
    let absolute = root.join(relative);
    let source = match read_configuration_file(&absolute) {
        Ok(Some(source)) => source,
        Ok(None) => return None,
        Err(_) => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope,
                path: label,
                message: "配置文件不可读取".into(),
                line: None,
                column: None,
                key: None,
                recovery: Some("检查文件权限或从备份恢复该文件。".to_owned()),
            });
            return None;
        }
    };
    if let Err(failure) = validate_document(&document_path, &source) {
        report.record(ConfigurationValidationDiagnostic {
            severity: ConfigurationValidationSeverity::Error,
            scope,
            path: label,
            message: failure.message.into(),
            line: failure.line,
            column: failure.column,
            key: None,
            recovery: Some("修复报告中的文件后重新运行配置校验。".to_owned()),
        });
        return None;
    }
    match toml::from_str(&source) {
        Ok(value) => Some(value),
        Err(_) => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope,
                path: label,
                message: "配置文档无法解析".into(),
                line: None,
                column: None,
                key: None,
                recovery: Some("修复报告中的文件后重新运行配置校验。".to_owned()),
            });
            None
        }
    }
}

/// 只枚举固定配置目录下的 `.toml` 文件，目录损坏成为诊断而不是空列表。
///
/// @author kongweiguang
fn validation_directory_files(
    root: &Path,
    relative_directory: &str,
    kind: ConfigDirectoryKind,
    report: &mut ConfigurationValidationReport,
) -> Vec<ValidationFile> {
    let scope = match kind {
        ConfigDirectoryKind::Profiles => ConfigurationValidationScope::Profiles,
        ConfigDirectoryKind::Hosts => ConfigurationValidationScope::Hosts,
        ConfigDirectoryKind::Snippets => ConfigurationValidationScope::Snippets,
        ConfigDirectoryKind::Workflows => ConfigurationValidationScope::Workflows,
    };
    let directory = root.join(relative_directory);
    let link_metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope,
                path: relative_directory.into(),
                message: "配置目录不可读取".into(),
                line: None,
                column: None,
                key: None,
                recovery: Some("检查目录权限或从备份恢复配置目录。".to_owned()),
            });
            return Vec::new();
        }
    };
    let metadata = if link_metadata.file_type().is_symlink() {
        match fs::metadata(&directory) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.record(ConfigurationValidationDiagnostic {
                    severity: ConfigurationValidationSeverity::Error,
                    scope,
                    path: relative_directory.into(),
                    message: "配置目录链接不可读取".into(),
                    line: None,
                    column: None,
                    key: None,
                    recovery: Some("修复目录链接或从备份恢复配置目录。".to_owned()),
                });
                return Vec::new();
            }
        }
    } else {
        link_metadata
    };
    if !metadata.is_dir() {
        report.record(ConfigurationValidationDiagnostic {
            severity: ConfigurationValidationSeverity::Error,
            scope,
            path: relative_directory.into(),
            message: "配置路径不是目录".into(),
            line: None,
            column: None,
            key: None,
            recovery: Some("将该路径恢复为配置目录。".to_owned()),
        });
        return Vec::new();
    }
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope,
                path: relative_directory.into(),
                message: "配置目录不可读取".into(),
                line: None,
                column: None,
                key: None,
                recovery: Some("检查目录权限或从备份恢复配置目录。".to_owned()),
            });
            return Vec::new();
        }
    };
    let mut entries = entries.collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.as_ref().map(|entry| entry.file_name()).ok());
    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.record(ConfigurationValidationDiagnostic {
                    severity: ConfigurationValidationSeverity::Error,
                    scope,
                    path: relative_directory.into(),
                    message: "配置目录中存在不可读取的项目".into(),
                    line: None,
                    column: None,
                    key: None,
                    recovery: Some("检查目录权限并重新运行配置校验。".to_owned()),
                });
                continue;
            }
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("toml"))
            || (kind == ConfigDirectoryKind::Hosts
                && entry.file_name() == OsStr::new("groups.toml"))
        {
            continue;
        }
        let relative = PathBuf::from(relative_directory).join(entry.file_name());
        let label = safe_relative_path(&relative);
        let Some(id) = path.file_stem().and_then(OsStr::to_str) else {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope,
                path: label,
                message: "配置文件名无效".into(),
                line: None,
                column: None,
                key: None,
                recovery: Some("将文件重命名为有效配置 ID 后重试。".to_owned()),
            });
            continue;
        };
        files.push(ValidationFile {
            path: kind.document_path(id),
            relative,
            label,
        });
    }
    files
}

/// 保持旧配置校验要求的敏感 key 文件忽略规则，避免密钥被错误加入仓库。
/// 尚不存在的根没有密钥或仓库，沿用离线校验的缺省语义且不创建目录；已存在的根仍必须有忽略规则。
///
/// @author kongweiguang
fn validate_host_secret_ignore_rule(root: &Path, report: &mut ConfigurationValidationReport) {
    const REQUIRED_RULE: &str = "secrets/vault-key.toml";
    if fs::symlink_metadata(root).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        return;
    }
    let path = root.join(".gitignore");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope: ConfigurationValidationScope::Hosts,
                path: ".gitignore".into(),
                message: "缺少密钥忽略规则".into(),
                line: None,
                column: None,
                key: Some(REQUIRED_RULE.to_owned()),
                recovery: Some("将 secrets/vault-key.toml 添加到 .gitignore。".to_owned()),
            });
            return;
        }
        Err(_) => {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope: ConfigurationValidationScope::Hosts,
                path: ".gitignore".into(),
                message: "密钥忽略规则文件不可读取".into(),
                line: None,
                column: None,
                key: Some(REQUIRED_RULE.to_owned()),
                recovery: Some("检查 .gitignore 文件权限。".to_owned()),
            });
            return;
        }
    };
    let present = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|line| line == REQUIRED_RULE);
    if present {
        report.record_checked(
            ConfigurationValidationScope::Hosts,
            ".gitignore",
            "required secret ignore rule present",
        );
    } else {
        report.record(ConfigurationValidationDiagnostic {
            severity: ConfigurationValidationSeverity::Error,
            scope: ConfigurationValidationScope::Hosts,
            path: ".gitignore".into(),
            message: "缺少密钥忽略规则".into(),
            line: None,
            column: None,
            key: Some(REQUIRED_RULE.to_owned()),
            recovery: Some("将 secrets/vault-key.toml 添加到 .gitignore。".to_owned()),
        });
    }
}

/// 复用旧配置工具的步骤唯一性与递增顺序规则，但诊断不回显用户写入的步骤 ID。
///
/// @author kongweiguang
fn validate_workflow_sequence(
    value: &toml::Value,
    path: &str,
    report: &mut ConfigurationValidationReport,
) {
    let Some(steps) = value.get("steps").and_then(toml::Value::as_array) else {
        return;
    };
    let mut ids = std::collections::BTreeSet::new();
    let mut previous_sort_order = None;
    for step in steps {
        let Some(table) = step.as_table() else {
            continue;
        };
        if let Some(id) = table.get("id").and_then(toml::Value::as_str)
            && !ids.insert(id)
        {
            report.record(ConfigurationValidationDiagnostic {
                severity: ConfigurationValidationSeverity::Error,
                scope: ConfigurationValidationScope::Workflows,
                path: path.into(),
                message: "duplicate workflow step id".into(),
                line: None,
                column: None,
                key: Some("steps.id".to_owned()),
                recovery: Some("Make each workflow step id unique within the workflow.".to_owned()),
            });
        }
        if let Some(sort_order) = table.get("sort_order").and_then(toml::Value::as_integer) {
            if previous_sort_order.is_some_and(|previous| sort_order <= previous) {
                report.record(ConfigurationValidationDiagnostic {
                    severity: ConfigurationValidationSeverity::Error,
                    scope: ConfigurationValidationScope::Workflows,
                    path: path.into(),
                    message: "workflow step sort_order must increase".into(),
                    line: None,
                    column: None,
                    key: Some("steps.sort_order".to_owned()),
                    recovery: Some(
                        "Sort workflow steps by increasing sort_order values.".to_owned(),
                    ),
                });
            }
            previous_sort_order = Some(sort_order);
        }
    }
}

/// 读取并检查启动关键文档；任何 TOML 内容都不写入日志或公开恢复状态。
fn inspect_startup_file(
    root: &Path,
    relative: &Path,
    document_path: ConfigDocumentPath,
    issues: &mut Vec<ConfigurationIssue>,
) {
    let display_path = safe_relative_path(relative);
    let absolute = root.join(relative);
    let source = match read_configuration_file(&absolute) {
        Ok(Some(source)) => source,
        Ok(None) => return,
        Err(_) => {
            issues.push(unreadable_issue(display_path));
            return;
        }
    };
    if let Err(error) = validate_document(&document_path, &source) {
        issues.push(ConfigurationIssue {
            relative_path: display_path,
            kind: error.kind,
            line: error.line,
            column: error.column,
        });
    }
}

/// 按文档类型执行 TOML、schema、profile 必填字段与主机凭据安全检查。
///
/// @author kongweiguang
pub(super) fn validate_document(
    path: &ConfigDocumentPath,
    source: &str,
) -> Result<(), ValidationFailure> {
    let value = toml::from_str::<toml::Value>(source).map_err(|error| {
        let (line, column) = error
            .span()
            .map(|span| line_column(source, span.start))
            .map_or((None, None), |(line, column)| (Some(line), Some(column)));
        ValidationFailure::with_source(ConfigurationIssueKind::MalformedToml, line, column, error)
    })?;
    let table = value.as_table().ok_or_else(|| {
        ValidationFailure::plain(
            ConfigurationIssueKind::MalformedToml,
            Some(1),
            Some(1),
            "TOML 根节点必须是表",
        )
    })?;
    if path.relative_path().is_err() {
        return Err(ValidationFailure::plain(
            ConfigurationIssueKind::MalformedToml,
            Some(1),
            Some(1),
            "配置文件名不受支持",
        ));
    }
    let expected = if matches!(path, ConfigDocumentPath::Host(_)) {
        HOST_SCHEMA_VERSION
    } else {
        SETTINGS_SCHEMA_VERSION
    };
    let schema_version = table
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| {
            ValidationFailure::plain(
                ConfigurationIssueKind::UnsupportedSchema,
                Some(1),
                Some(1),
                "配置缺少有效 schema_version",
            )
        })?;
    if schema_version != expected {
        return Err(ValidationFailure::plain(
            ConfigurationIssueKind::UnsupportedSchema,
            Some(1),
            Some(1),
            "配置 schema_version 不受支持",
        ));
    }

    if let Some(expected_id) = document_id(path)
        && table.get("id").and_then(toml::Value::as_str) != Some(expected_id)
    {
        return Err(ValidationFailure::plain(
            ConfigurationIssueKind::MalformedToml,
            Some(1),
            Some(1),
            "配置文件中的 id 必须与文件名一致",
        ));
    }
    match path {
        ConfigDocumentPath::Settings => validate_settings_fields(source)?,
        ConfigDocumentPath::Profile(_) => validate_profile_fields(table)?,
        ConfigDocumentPath::HostGroups => validate_host_groups(table)?,
        ConfigDocumentPath::Host(_) => {
            let protocol = table
                .get("protocol")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    ValidationFailure::plain(
                        ConfigurationIssueKind::MalformedToml,
                        Some(1),
                        Some(1),
                        "主机必须显式声明 protocol",
                    )
                })?;
            if !matches!(protocol, "ssh" | "sftp" | "rdp" | "telnet" | "serial") {
                return Err(ValidationFailure::plain(
                    ConfigurationIssueKind::MalformedToml,
                    Some(1),
                    Some(1),
                    "主机 protocol 不受支持",
                ));
            }
            validate_host_fields(table, protocol)?;
            if contains_inline_secret(&value) {
                return Err(ValidationFailure::plain(
                    ConfigurationIssueKind::MalformedToml,
                    Some(1),
                    Some(1),
                    "主机凭据必须通过 vault 引用",
                ));
            }
        }
        ConfigDocumentPath::Snippet(_) => validate_snippet_fields(table)?,
        ConfigDocumentPath::Workflow(_) => validate_workflow_fields(table)?,
    }
    Ok(())
}

/// 启动时按应用同一 typed 设置投影验证已知字段，未知旧版键继续由原文仓储保留。
fn validate_settings_fields(source: &str) -> Result<(), ValidationFailure> {
    let settings = super::settings::decode_settings(source).map_err(|error| {
        let (line, column) = error
            .span()
            .map(|span| line_column(source, span.start))
            .map_or((None, None), |(line, column)| (Some(line), Some(column)));
        ValidationFailure::with_source(ConfigurationIssueKind::MalformedToml, line, column, error)
    })?;
    settings
        .validated()
        .map(|_| ())
        .map_err(|source| ValidationFailure {
            kind: ConfigurationIssueKind::MalformedToml,
            line: None,
            column: None,
            message: "设置字段或取值无效",
            source: Some(Box::new(source)),
        })
}

/// 校验现行 schema 1 profile 的强制字段类型，同时允许未知字段原样保留。
fn validate_profile_fields(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    let required = [
        ("id", ValueKind::String),
        ("name", ValueKind::String),
        ("shell", ValueKind::String),
        ("is_default", ValueKind::Boolean),
        ("sort_order", ValueKind::Integer),
        ("created_at", ValueKind::String),
        ("updated_at", ValueKind::String),
    ];
    for (field, kind) in required {
        let matches = table.get(field).is_some_and(|value| kind.matches(value));
        if !matches {
            return Err(ValidationFailure::plain(
                ConfigurationIssueKind::MalformedToml,
                Some(1),
                Some(1),
                "profile 缺少有效必填字段",
            ));
        }
    }
    Ok(())
}

/// 校验 hosts/groups.toml 的稳定列表结构；缺少 groups 与旧文档默认空列表一致。
fn validate_host_groups(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    let Some(groups) = table.get("groups") else {
        return Ok(());
    };
    let Some(groups) = groups.as_array() else {
        return Err(invalid_configuration_shape("主机分组列表类型无效"));
    };
    for group in groups {
        let Some(group) = group.as_table() else {
            return Err(invalid_configuration_shape("主机分组结构无效"));
        };
        validate_required_fields(
            group,
            &[
                ("id", ValueKind::String),
                ("name", ValueKind::String),
                ("sort_order", ValueKind::Integer),
                ("created_at", ValueKind::String),
                ("updated_at", ValueKind::String),
            ],
            "主机分组字段缺失或类型无效",
        )?;
    }
    Ok(())
}

/// 校验 schema v3 主机的协议字段与可选备注；其它非保留键继续由原文仓储保留。
///
/// @author kongweiguang
fn validate_host_fields(
    table: &toml::map::Map<String, toml::Value>,
    protocol: &str,
) -> Result<(), ValidationFailure> {
    validate_required_fields(
        table,
        &[
            ("id", ValueKind::String),
            ("name", ValueKind::String),
            ("auth_type", ValueKind::String),
            ("sort_order", ValueKind::Integer),
            ("created_at", ValueKind::String),
            ("updated_at", ValueKind::String),
        ],
        "主机字段缺失或类型无效",
    )?;

    if protocol == "serial" {
        if ["host", "port", "username"]
            .iter()
            .any(|field| table.contains_key(*field))
        {
            return Err(invalid_configuration_shape(
                "串口主机不得重复声明网络 endpoint 字段",
            ));
        }
        validate_serial_fields(table)?;
    } else {
        validate_required_fields(
            table,
            &[
                ("host", ValueKind::String),
                ("port", ValueKind::Integer),
                ("username", ValueKind::String),
            ],
            "网络主机 endpoint 字段缺失或类型无效",
        )?;
        let port = table.get("port").and_then(toml::Value::as_integer);
        if !port.is_some_and(|port| (1..=u16::MAX as i64).contains(&port)) {
            return Err(invalid_configuration_shape("主机端口类型无效"));
        }
        if table.contains_key("serial") {
            return Err(invalid_configuration_shape("非串口主机不得声明 serial 表"));
        }
    }
    validate_optional_fields(
        table,
        &[
            ("group_id", ValueKind::String),
            ("credential_ref", ValueKind::String),
            ("secret_ref", ValueKind::String),
            ("key_passphrase_ref", ValueKind::String),
            ("note", ValueKind::String),
            ("tags", ValueKind::StringArray),
            ("ssh_options", ValueKind::Table),
            ("rdp_options", ValueKind::Table),
            ("serial", ValueKind::Table),
        ],
        "主机扩展字段类型无效",
    )?;
    if table
        .get("note")
        .and_then(toml::Value::as_str)
        .is_some_and(|note| !host_note_is_valid(note))
    {
        return Err(invalid_configuration_shape(
            "主机备注不能超过 4096 个字符或包含不支持的控制字符",
        ));
    }
    if table
        .get("tags")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .any(is_reserved_serial_host_tag)
    {
        return Err(invalid_configuration_shape(
            "串口参数必须使用 serial 表，tags 只能保存用户标签",
        ));
    }
    validate_host_connection_options(table)?;
    Ok(())
}

/// 当前选项在文件入口统一校验，避免 UI 能保存但外部编辑绕过约束，仍保留未知扩展字段。
///
/// @author kongweiguang
fn validate_host_connection_options(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    if let Some(ssh) = table.get("ssh_options").and_then(toml::Value::as_table) {
        validate_optional_fields(
            ssh,
            &[("terminal", ValueKind::Table), ("sftp", ValueKind::Table)],
            "SSH 选项表类型无效",
        )?;
        if let Some(terminal) = ssh.get("terminal").and_then(toml::Value::as_table) {
            validate_optional_fields(
                terminal,
                &[
                    ("connect_timeout_seconds", ValueKind::Integer),
                    ("keepalive_seconds", ValueKind::Integer),
                    ("term", ValueKind::String),
                    ("working_directory", ValueKind::String),
                ],
                "SSH 终端选项类型无效",
            )?;
            for (key, minimum) in [("connect_timeout_seconds", 1), ("keepalive_seconds", 0)] {
                if terminal
                    .get(key)
                    .and_then(toml::Value::as_integer)
                    .is_some_and(|value| !(minimum..=600).contains(&value))
                {
                    return Err(invalid_configuration_shape("SSH 超时或心跳范围无效"));
                }
            }
            if terminal
                .get("term")
                .and_then(toml::Value::as_str)
                .is_some_and(|value| {
                    value.is_empty()
                        || value.len() > 128
                        || value
                            .chars()
                            .any(|character| character.is_whitespace() || character.is_control())
                })
                || terminal
                    .get("working_directory")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|value| {
                        value.trim().is_empty() || value.chars().any(char::is_control)
                    })
            {
                return Err(invalid_configuration_shape("SSH 终端类型或目录无效"));
            }
        }
        if let Some(sftp) = ssh.get("sftp").and_then(toml::Value::as_table) {
            validate_optional_fields(
                sftp,
                &[
                    ("enabled", ValueKind::Boolean),
                    ("preserve_mtime", ValueKind::Boolean),
                    ("follow_source_symlinks", ValueKind::Boolean),
                    ("local_start_directory", ValueKind::String),
                    ("remote_start_directory", ValueKind::String),
                    ("concurrency", ValueKind::Integer),
                ],
                "SFTP 选项类型无效",
            )?;
            if sftp
                .get("concurrency")
                .and_then(toml::Value::as_integer)
                .is_some_and(|value| !(1..=8).contains(&value))
                || sftp
                    .get("local_start_directory")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|value| {
                        value.trim().is_empty()
                            || !Path::new(value).is_absolute()
                            || value.chars().any(char::is_control)
                    })
                || sftp
                    .get("remote_start_directory")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|value| {
                        !value.starts_with('/')
                            || value.contains('\\')
                            || value.split('/').any(|part| part == "..")
                            || value.chars().any(char::is_control)
                    })
            {
                return Err(invalid_configuration_shape("SFTP 目录或并发范围无效"));
            }
        }
    }
    if let Some(rdp) = table.get("rdp_options").and_then(toml::Value::as_table) {
        validate_optional_fields(
            rdp,
            &[
                ("fullscreen", ValueKind::Boolean),
                ("width", ValueKind::Integer),
                ("height", ValueKind::Integer),
            ],
            "RDP 显示选项类型无效",
        )?;
        if rdp.contains_key("width") != rdp.contains_key("height")
            || rdp
                .get("width")
                .and_then(toml::Value::as_integer)
                .is_some_and(|value| !(640..=8192).contains(&value))
            || rdp
                .get("height")
                .and_then(toml::Value::as_integer)
                .is_some_and(|value| !(480..=8192).contains(&value))
        {
            return Err(invalid_configuration_shape(
                "RDP 尺寸必须成对且在支持范围内",
            ));
        }
    }
    Ok(())
}

/// 以当前格式的字段名、类型和枚举值校验串口目标，不提供旧短别名或默认补齐。
fn validate_serial_fields(
    host: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    let serial = host
        .get("serial")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid_configuration_shape("串口主机必须声明 serial 表"))?;
    validate_required_fields(
        serial,
        &[
            ("device", ValueKind::String),
            ("baud_rate", ValueKind::Integer),
            ("data_bits", ValueKind::Integer),
            ("stop_bits", ValueKind::Integer),
            ("parity", ValueKind::String),
            ("flow_control", ValueKind::String),
        ],
        "串口参数字段缺失或类型无效",
    )?;
    let device = serial.get("device").and_then(toml::Value::as_str);
    let baud_rate = serial.get("baud_rate").and_then(toml::Value::as_integer);
    let data_bits = serial.get("data_bits").and_then(toml::Value::as_integer);
    let stop_bits = serial.get("stop_bits").and_then(toml::Value::as_integer);
    let parity = serial.get("parity").and_then(toml::Value::as_str);
    let flow_control = serial.get("flow_control").and_then(toml::Value::as_str);
    if !device.is_some_and(|value| {
        !value.trim().is_empty() && value.trim() == value && !value.chars().any(char::is_control)
    }) || !baud_rate.is_some_and(|value| (300..=4_000_000).contains(&value))
        || !data_bits.is_some_and(|value| (5..=8).contains(&value))
        || !stop_bits.is_some_and(|value| matches!(value, 1 | 2))
        || !parity.is_some_and(|value| matches!(value, "none" | "odd" | "even"))
        || !flow_control.is_some_and(|value| matches!(value, "none" | "xonxoff" | "rtscts"))
    {
        return Err(invalid_configuration_shape("串口参数取值无效"));
    }
    Ok(())
}

/// 校验 schema v1 片段基础字段和可选扩展字段，同时拒绝 secret 的默认值与建议列表。
fn validate_snippet_fields(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    validate_required_fields(
        table,
        &[
            ("id", ValueKind::String),
            ("title", ValueKind::String),
            ("command", ValueKind::String),
            ("scope", ValueKind::String),
            ("sort_order", ValueKind::Integer),
            ("created_at", ValueKind::String),
            ("updated_at", ValueKind::String),
        ],
        "片段字段缺失或类型无效",
    )?;
    validate_scope(table, "scope", "片段作用域无效")?;
    validate_optional_fields(
        table,
        &[
            ("description", ValueKind::String),
            ("tags", ValueKind::StringArray),
            ("category", ValueKind::String),
            ("risk", ValueKind::String),
            ("default_action", ValueKind::String),
            ("variables", ValueKind::TableArray),
            ("context_bindings", ValueKind::TableArray),
            ("derived_from", ValueKind::String),
        ],
        "片段扩展字段类型无效",
    )?;
    validate_snippet_variables(table)
}

/// 校验每个片段变量的原有 TOML 字段形状，并确保 secret 没有默认值或 suggestions。
fn validate_snippet_variables(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    let Some(variables) = table.get("variables") else {
        return Ok(());
    };
    let Some(variables) = variables.as_array() else {
        return Err(invalid_configuration_shape("片段变量列表类型无效"));
    };
    for variable in variables {
        let Some(variable) = variable.as_table() else {
            return Err(invalid_configuration_shape("片段变量结构无效"));
        };
        validate_required_fields(
            variable,
            &[
                ("name", ValueKind::String),
                ("label", ValueKind::String),
                ("description", ValueKind::String),
                ("kind", ValueKind::String),
                ("required", ValueKind::Boolean),
                ("render_strategy", ValueKind::String),
            ],
            "片段变量字段缺失或类型无效",
        )?;
        validate_optional_fields(
            variable,
            &[
                ("default_value", ValueKind::String),
                ("suggestions", ValueKind::StringArray),
                ("validation", ValueKind::String),
                ("sensitive", ValueKind::Boolean),
            ],
            "片段变量扩展字段类型无效",
        )?;
        if variable.get("kind").and_then(toml::Value::as_str) == Some("secret") {
            let has_default = variable
                .get("default_value")
                .and_then(toml::Value::as_str)
                .is_some_and(|value| !value.is_empty());
            let has_suggestions = variable
                .get("suggestions")
                .and_then(toml::Value::as_array)
                .is_some_and(|values| !values.is_empty());
            if has_default || has_suggestions {
                return Err(invalid_configuration_shape(
                    "secret 片段变量禁止默认值或 suggestions",
                ));
            }
        }
    }
    if let Some(bindings) = table.get("context_bindings") {
        for binding in bindings.as_array().into_iter().flatten() {
            let Some(binding) = binding.as_table() else {
                return Err(invalid_configuration_shape("片段上下文绑定结构无效"));
            };
            validate_required_fields(
                binding,
                &[("kind", ValueKind::String)],
                "片段上下文绑定字段缺失或类型无效",
            )?;
            validate_optional_fields(
                binding,
                &[("target_id", ValueKind::String)],
                "片段上下文绑定字段类型无效",
            )?;
            if !binding
                .get("kind")
                .and_then(toml::Value::as_str)
                .is_some_and(|kind| matches!(kind, "global" | "workspace" | "host" | "hostGroup"))
            {
                return Err(invalid_configuration_shape("片段上下文绑定类型无效"));
            }
        }
    }
    Ok(())
}

/// 校验 schema v1 工作流与步骤结构；未知字段仍留在原始 TOML 文本中。
fn validate_workflow_fields(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<(), ValidationFailure> {
    validate_required_fields(
        table,
        &[
            ("id", ValueKind::String),
            ("title", ValueKind::String),
            ("scope", ValueKind::String),
            ("sort_order", ValueKind::Integer),
            ("created_at", ValueKind::String),
            ("updated_at", ValueKind::String),
        ],
        "工作流字段缺失或类型无效",
    )?;
    validate_scope(table, "scope", "工作流作用域无效")?;
    validate_optional_fields(
        table,
        &[
            ("description", ValueKind::String),
            ("tags", ValueKind::StringArray),
            ("steps", ValueKind::TableArray),
        ],
        "工作流扩展字段类型无效",
    )?;
    if let Some(steps) = table.get("steps") {
        for step in steps.as_array().into_iter().flatten() {
            let Some(step) = step.as_table() else {
                return Err(invalid_configuration_shape("工作流步骤结构无效"));
            };
            validate_required_fields(
                step,
                &[
                    ("id", ValueKind::String),
                    ("title", ValueKind::String),
                    ("command", ValueKind::String),
                    ("requires_confirmation", ValueKind::Boolean),
                    ("sort_order", ValueKind::Integer),
                    ("created_at", ValueKind::String),
                    ("updated_at", ValueKind::String),
                ],
                "工作流步骤字段缺失或类型无效",
            )?;
            validate_optional_fields(
                step,
                &[
                    ("description", ValueKind::String),
                    ("scope", ValueKind::String),
                ],
                "工作流步骤字段类型无效",
            )?;
            if step.contains_key("scope") {
                validate_scope(step, "scope", "工作流步骤作用域无效")?;
            }
        }
    }
    Ok(())
}

/// 统一检查配置中的 any/local/ssh 作用域枚举，拒绝未知值而不回显字段内容。
fn validate_scope(
    table: &toml::map::Map<String, toml::Value>,
    field: &str,
    message: &'static str,
) -> Result<(), ValidationFailure> {
    if table
        .get(field)
        .and_then(toml::Value::as_str)
        .is_some_and(|scope| matches!(scope, "any" | "local" | "ssh"))
    {
        Ok(())
    } else {
        Err(invalid_configuration_shape(message))
    }
}

/// 检查必填键及其 TOML 原生类型，避免宽松转换掩盖手工编辑错误。
fn validate_required_fields(
    table: &toml::map::Map<String, toml::Value>,
    fields: &[(&str, ValueKind)],
    message: &'static str,
) -> Result<(), ValidationFailure> {
    if fields
        .iter()
        .all(|(field, kind)| table.get(*field).is_some_and(|value| kind.matches(value)))
    {
        Ok(())
    } else {
        Err(invalid_configuration_shape(message))
    }
}

/// 检查已知可选键；其它未知键不会被拒绝或从原始文档中删除。
fn validate_optional_fields(
    table: &toml::map::Map<String, toml::Value>,
    fields: &[(&str, ValueKind)],
    message: &'static str,
) -> Result<(), ValidationFailure> {
    if fields
        .iter()
        .all(|(field, kind)| table.get(*field).is_none_or(|value| kind.matches(value)))
    {
        Ok(())
    } else {
        Err(invalid_configuration_shape(message))
    }
}

/// 结构错误使用静态安全说明，避免把手写 TOML 中可能含密钥的值放进异常。
fn invalid_configuration_shape(message: &'static str) -> ValidationFailure {
    ValidationFailure::plain(
        ConfigurationIssueKind::MalformedToml,
        Some(1),
        Some(1),
        message,
    )
}

/// 对按 ID 命名的配置文件保持旧仓储的文件名与文档标识一致约束。
pub(super) fn document_id(path: &ConfigDocumentPath) -> Option<&str> {
    match path {
        ConfigDocumentPath::Profile(id)
        | ConfigDocumentPath::Host(id)
        | ConfigDocumentPath::Snippet(id)
        | ConfigDocumentPath::Workflow(id) => Some(id),
        ConfigDocumentPath::Settings | ConfigDocumentPath::HostGroups => None,
    }
}

/// 搜索所有 TOML 子表的禁止内联凭据键；引用键 `secret_ref` 与 `key_passphrase_ref` 保留合法。
fn contains_inline_secret(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            let normalized = key
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            matches!(
                normalized.as_str(),
                "password"
                    | "credentialsecret"
                    | "inlineprivatekey"
                    | "keypassphrase"
                    | "privatekey"
                    | "privatekeybody"
            ) || contains_inline_secret(value)
        }),
        toml::Value::Array(values) => values.iter().any(contains_inline_secret),
        _ => false,
    }
}

/// 把解析器的 UTF-8 字节偏移转换成一基行列，界面可定位错误而不展示解析原文。
fn line_column(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut end = byte_offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = source.get(..end).unwrap_or_default();
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, tail)| {
            tail.chars().count() + 1
        });
    (line, column)
}

/// 内容验证失败保持字段安全，不在错误链的公开显示中回显 TOML 值。
#[derive(Debug)]
pub(super) struct ValidationFailure {
    kind: ConfigurationIssueKind,
    line: Option<usize>,
    column: Option<usize>,
    message: &'static str,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ValidationFailure {
    /// 将 parser 原因留在 source 链，但只暴露静态业务说明。
    fn with_source<E>(
        kind: ConfigurationIssueKind,
        line: Option<usize>,
        column: Option<usize>,
        source: E,
    ) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            kind,
            line,
            column,
            message: "配置文档无法解析",
            source: Some(Box::new(source)),
        }
    }

    /// 记录无 parser cause 的 schema 或领域约束错误。
    fn plain(
        kind: ConfigurationIssueKind,
        line: Option<usize>,
        column: Option<usize>,
        message: &'static str,
    ) -> Self {
        Self {
            kind,
            line,
            column,
            message,
            source: None,
        }
    }
}

impl std::fmt::Display for ValidationFailure {
    /// 诊断 source 可能引用配置 token，因此其显示只保留静态说明。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for ValidationFailure {
    /// 让内部日志保留解析器原始错误对象，但 Display/Debug 不向 UI 展开内容。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[derive(Clone, Copy)]
enum ValueKind {
    String,
    Boolean,
    Integer,
    Table,
    StringArray,
    TableArray,
}

impl ValueKind {
    /// 只比较 TOML 原生类型，不做字符串到布尔或数值的宽松转换。
    fn matches(self, value: &toml::Value) -> bool {
        match self {
            Self::String => value.is_str(),
            Self::Boolean => value.is_bool(),
            Self::Integer => value.is_integer(),
            Self::Table => value.is_table(),
            Self::StringArray => value
                .as_array()
                .is_some_and(|values| values.iter().all(toml::Value::is_str)),
            Self::TableArray => value
                .as_array()
                .is_some_and(|values| values.iter().all(toml::Value::is_table)),
        }
    }
}
