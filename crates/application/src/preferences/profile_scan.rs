//! 显式扫描系统 Shell 时仅追加新模板，可能覆盖现有配置的更新作为建议返回。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, HashSet};

use tokio_util::sync::CancellationToken;

use super::{ProfileDraft, ProfileService, VersionedProfile};
use crate::{AppError, AppResult, local_shells::LocalShellCandidate};

/// 用户核对所用的原始 revision 与建议启动设置，不从扫描直接写回。
#[derive(Clone, Debug)]
pub struct ProfileScanConflict {
    current: VersionedProfile,
    suggested: ProfileDraft,
}

impl ProfileScanConflict {
    /// 保留用户看到的版本，确认时必须按同一 revision 提交。
    /// @author kongweiguang
    pub fn current(&self) -> &VersionedProfile {
        &self.current
    }

    /// 仅替换启动程序和参数，默认项、名称及其它用户配置由原模板提供。
    /// @author kongweiguang
    pub fn suggested(&self) -> &ProfileDraft {
        &self.suggested
    }
}

/// 每个新增模板独立提交；后续失败时仍返回已确认的新增数量。
#[derive(Debug, Default)]
pub struct ProfileScanReport {
    added: usize,
    skipped: usize,
    conflicts: Vec<ProfileScanConflict>,
    stopped: Option<AppError>,
}

impl ProfileScanReport {
    /// 统计实际收到成功回执的新增，不把候选或未知结果计为已保存。
    /// @author kongweiguang
    pub fn added(&self) -> usize {
        self.added
    }

    /// 报告无法无歧义匹配或无损表示的候选，以便用户知道为何没有自动添加。
    /// @author kongweiguang
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// 将待确认项和中断原因交还调用者，避免隐式覆盖或重试。
    /// @author kongweiguang
    pub fn into_review(self) -> (Vec<ProfileScanConflict>, Option<AppError>) {
        (self.conflicts, self.stopped)
    }
}

impl ProfileService {
    /// 重新读取当前目录再追加；每项使用创建 CAS，失败或取消后不重试，也不继续写下一项。
    /// @author kongweiguang
    pub async fn rescan_system_profiles(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<ProfileScanReport> {
        let candidates = self.discover_local_shells(cancellation.clone()).await?;
        let profiles = self.list_profiles().await?;
        let (new_profiles, mut report) = plan_scan(&candidates, &profiles);
        for draft in new_profiles {
            if cancellation.is_cancelled() {
                report.stopped = Some(AppError::cancelled("已取消系统 Shell 扫描"));
                break;
            }
            match self.save_profile(draft, None).await {
                Ok(_) => report.added += 1,
                Err(error) => {
                    report.stopped = Some(error);
                    break;
                }
            }
        }
        Ok(report)
    }
}

/// ID 优先识别标准模板；自定义模板按启动设置或名称判断，歧义不猜测。
/// @author kongweiguang
fn plan_scan(
    candidates: &[LocalShellCandidate],
    profiles: &[VersionedProfile],
) -> (Vec<ProfileDraft>, ProfileScanReport) {
    let mut report = ProfileScanReport::default();
    let mut new_profiles: Vec<ProfileDraft> = Vec::new();
    let ids = candidates
        .iter()
        .map(LocalShellCandidate::standard_profile_id)
        .collect::<HashSet<_>>();
    let mut sort_order = profiles
        .iter()
        .map(|row| row.profile.sort_order)
        .max()
        .unwrap_or(-1);
    for candidate in candidates {
        let Some(shell) = candidate.program().to_str() else {
            report.skipped += 1;
            continue;
        };
        let Some(args) = candidate
            .args()
            .iter()
            .map(|arg| arg.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            report.skipped += 1;
            continue;
        };
        let id = candidate.standard_profile_id();
        let name = candidate.display_name();
        let same_launch = |profile_shell: &str, profile_args: &[String]| {
            programs_match(profile_shell, shell) && profile_args == args
        };
        let stable = profiles.iter().find(|row| row.profile.id == id);
        let mut named = profiles
            .iter()
            .filter(|row| names_match(&row.profile.name, name));
        let matching = if stable.is_some() {
            stable
        } else if profiles
            .iter()
            .any(|row| same_launch(&row.profile.shell, &row.profile.args))
        {
            continue;
        } else {
            let first = named.next();
            if named.next().is_some() || first.is_some_and(|row| ids.contains(&row.profile.id)) {
                report.skipped += 1;
                continue;
            }
            first
        };
        if let Some(row) = matching {
            if !same_launch(&row.profile.shell, &row.profile.args) {
                report.conflicts.push(ProfileScanConflict {
                    current: row.clone(),
                    suggested: ProfileDraft {
                        id: Some(row.profile.id.clone()),
                        name: row.profile.name.clone(),
                        shell: shell.to_owned(),
                        args,
                        cwd: row.profile.cwd.clone(),
                        env: row.profile.env.clone(),
                        sidebar_group_id: None,
                        sort_order: row.profile.sort_order,
                    },
                });
            }
            continue;
        }
        if new_profiles
            .iter()
            .any(|draft| same_launch(&draft.shell, &draft.args))
        {
            continue;
        }
        if new_profiles
            .iter()
            .any(|draft| names_match(&draft.name, name))
        {
            report.skipped += 1;
            continue;
        }
        sort_order = sort_order.saturating_add(1);
        new_profiles.push(ProfileDraft {
            id: Some(id),
            name: name.to_owned(),
            shell: shell.to_owned(),
            args,
            cwd: None,
            env: BTreeMap::new(),
            sidebar_group_id: None,
            sort_order,
        });
    }
    (new_profiles, report)
}

/// 名称是提示冲突的线索，忽略空白和大小写差异，防止轻易产生同名项。
/// @author kongweiguang
fn names_match(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

/// Windows 接受两种分隔符和路径大小写；其它平台只匹配原始路径，不猜测命令别名。
/// @author kongweiguang
fn programs_match(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.replace('/', "\\")
            .eq_ignore_ascii_case(&right.replace('/', "\\"))
    } else {
        left == right
    }
}

#[cfg(test)]
#[path = "../../tests/unit/profile_scan.rs"]
mod tests;
