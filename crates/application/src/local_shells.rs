//! 本地 Shell 候选发现的应用端口与稳定候选描述。
//!
//! @author kongweiguang

use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

const MAX_CANDIDATE_ID_CHARS: usize = 128;
const MAX_DISPLAY_NAME_CHARS: usize = 160;
const MAX_CANDIDATE_ARGUMENTS: usize = 16;
/// 限制 discovery port 的返回体大小，避免异常 adapter 将无界列表传入表单。
pub const MAX_LOCAL_SHELL_CANDIDATES: usize = 32;

/// 一个可由本地主机 Profile 选择的已安装 shell；身份不依赖本机安装路径。
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalShellCandidate {
    id: String,
    display_name: String,
    program: OsString,
    args: Vec<OsString>,
}

impl LocalShellCandidate {
    /// 在跨层暴露前约束候选身份、文案和 argv 数量，避免 UI 承接不受控目录扫描结果。
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        program: OsString,
        args: Vec<OsString>,
    ) -> AppResult<Self> {
        let id = id.into();
        let display_name = display_name.into();
        if id.is_empty()
            || id.len() > MAX_CANDIDATE_ID_CHARS
            || !id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_:".contains(character))
        {
            return Err(AppError::invalid("本地 Shell 候选身份无效"));
        }
        if display_name.trim().is_empty()
            || display_name.chars().count() > MAX_DISPLAY_NAME_CHARS
            || display_name.chars().any(char::is_control)
        {
            return Err(AppError::invalid("本地 Shell 候选名称无效"));
        }
        if !Path::new(&program).is_absolute() || args.len() > MAX_CANDIDATE_ARGUMENTS {
            return Err(AppError::invalid("本地 Shell 候选启动参数无效"));
        }

        Ok(Self {
            id,
            display_name,
            program,
            args,
        })
    }

    /// 以稳定标识匹配 UI 当前选择，避免安装路径变化导致候选项身份漂移。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 返回适合列表展示的本地化候选名称，不把 executable 路径当作标题。
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// 返回无损系统路径供 Profile 草稿回填；UI 负责在无法表示为文本时保留手动输入入口。
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// 返回 shell 自身需要的参数，调用方不得把它们拼接成命令字符串。
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// 只检查安装元数据并尊重取消；适配器不得为发现候选而启动 shell 或用户启动脚本。
pub trait LocalShellDiscovery: Send + Sync + 'static {
    /// 返回稳定、有界的候选列表；扫描路径时需在每批文件检查之间读取取消 token。
    fn discover(&self, cancellation: &CancellationToken) -> AppResult<Vec<LocalShellCandidate>>;
}

#[cfg(test)]
#[path = "../tests/unit/local_shells_contract.rs"]
mod tests;
