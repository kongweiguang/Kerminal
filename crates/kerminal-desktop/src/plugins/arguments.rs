//! 插件启动选项与外部激活参数分离，避免把内部开关当作文件打开请求。
//! @author kongweiguang

use std::{ffi::OsString, path::PathBuf};

use kerminal_application::{AppError, AppResult};

/// 开发模式必须使用独立数据根；正式安装变更仍然只在普通重启时应用。
#[derive(Clone, Debug, Default)]
pub struct PluginStartupOptions {
    pub safe_mode: bool,
    pub dev_plugin: Option<PathBuf>,
}

impl PluginStartupOptions {
    /// 只消费已知插件参数；其余参数按原有外部激活契约保留顺序及原生编码。
    pub fn parse(
        arguments: impl IntoIterator<Item = OsString>,
        isolated_root: bool,
    ) -> AppResult<(Self, Vec<OsString>)> {
        let mut arguments = arguments.into_iter();
        let mut options = Self::default();
        let mut activation = Vec::new();
        if let Some(executable) = arguments.next() {
            activation.push(executable);
        }
        while let Some(argument) = arguments.next() {
            if argument == "--safe-mode" {
                options.safe_mode = true;
            } else if argument == "--dev-plugin" {
                if options.dev_plugin.is_some() || !isolated_root {
                    return Err(AppError::invalid(
                        "开发插件必须指定独立 KERMINAL_CONFIG_ROOT，且只能加载一个目录",
                    ));
                }
                let path = arguments
                    .next()
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .ok_or_else(|| AppError::invalid("--dev-plugin 必须指定绝对目录"))?;
                options.dev_plugin = Some(path);
            } else {
                activation.push(argument);
            }
        }
        if options.safe_mode && options.dev_plugin.is_some() {
            return Err(AppError::invalid("安全模式不加载开发插件"));
        }
        Ok((options, activation))
    }

    /// 环境只在组合根读取一次，避免异步任务在启动中途观察到不同的参数。
    pub fn current() -> AppResult<(Self, Vec<OsString>)> {
        Self::parse(
            std::env::args_os(),
            std::env::var_os("KERMINAL_CONFIG_ROOT").is_some_and(|value| !value.is_empty()),
        )
    }
}
