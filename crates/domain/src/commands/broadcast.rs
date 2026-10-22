//! 多终端广播的风险说明；确认界面由调用方负责。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use super::history::CommandHistoryTarget;
use crate::identity::PaneId;

/// 广播风险维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BroadcastRisk {
    Batch,
    Remote,
    Destructive,
}

/// 一个可发送目标；只有支持终端输入的目标可以进入广播集合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadcastTarget {
    pub pane_id: PaneId,
    pub title: String,
    pub target: CommandHistoryTarget,
}

/// 分析结果只包含待发送字节和确认理由，不执行任何写操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadcastAnalysis {
    pub command: String,
    pub data: String,
    pub targets: Vec<BroadcastTarget>,
    pub risks: Vec<BroadcastRisk>,
    pub reasons: Vec<String>,
    pub requires_confirmation: bool,
}

impl BroadcastAnalysis {
    /// 统一加入终端回车并计算批量/远程/破坏性风险。
    pub fn analyze(command: &str, targets: Vec<BroadcastTarget>) -> Self {
        let command = command.trim().to_owned();
        let mut risks = Vec::new();
        let mut reasons = Vec::new();
        if targets.len() > 1 {
            risks.push(BroadcastRisk::Batch);
            reasons.push(format!("将发送到 {} 个分屏", targets.len()));
        }
        if targets
            .iter()
            .any(|target| !matches!(&target.target, CommandHistoryTarget::Local))
        {
            risks.push(BroadcastRisk::Remote);
            reasons.push("包含远程或设备分屏".to_owned());
        }
        let destructive = destructive_reasons(&command);
        if !destructive.is_empty() {
            risks.push(BroadcastRisk::Destructive);
            reasons.extend(destructive);
        }
        let requires_confirmation = !risks.is_empty();
        let data = format!("{command}\r");
        Self {
            command,
            data,
            targets,
            risks,
            reasons,
            requires_confirmation,
        }
    }

    /// 空命令或无目标时不启动部分广播任务。
    pub fn is_sendable(&self) -> bool {
        !self.command.is_empty() && !self.targets.is_empty()
    }
}

/// 识别常见不可逆命令，提示用户确认而不尝试解析任意 shell 语法。
fn destructive_reasons(command: &str) -> Vec<String> {
    let lower = command.to_ascii_lowercase();
    let checks = [
        (
            lower.contains("rm -rf") || lower.contains("rm -fr"),
            "包含 rm 递归删除",
        ),
        (
            lower.contains("remove-item") && lower.contains("-recurse"),
            "包含 PowerShell 递归删除",
        ),
        (lower.contains("del /s"), "包含 Windows 批量删除"),
        (
            lower.split_whitespace().any(|part| part == "format"),
            "包含格式化命令",
        ),
        (lower.contains("mkfs"), "包含文件系统格式化"),
        (
            lower.contains("dd ") && lower.contains("of="),
            "包含磁盘写入命令",
        ),
        (
            ["shutdown", "reboot", "poweroff", "halt"]
                .iter()
                .any(|word| lower.split_whitespace().any(|part| part == *word)),
            "包含关机或重启命令",
        ),
        (
            lower.contains("drop database") || lower.contains("truncate table"),
            "包含数据库破坏性操作",
        ),
        (lower.contains("kubectl delete"), "包含 Kubernetes 删除操作"),
        (
            lower.contains("docker rm") || lower.contains("podman rm"),
            "包含容器删除操作",
        ),
        (lower.contains("git reset --hard"), "包含 Git 强制重置"),
        (lower.contains("chmod -r 777"), "包含递归放宽权限"),
    ];
    checks
        .iter()
        .filter_map(|(matches, reason)| matches.then_some((*reason).to_owned()))
        .collect()
}
