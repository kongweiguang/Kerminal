//! 压测参数解析与边界校验。
//!
//! @author kongweiguang

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
/// 命令行参数；所有压力上限都在进入 SessionService 前完成校验。
pub(in crate::acceptance) struct Options {
    pub(in crate::acceptance) session_counts: Vec<usize>,
    pub(in crate::acceptance) churn_cycles: usize,
    pub(in crate::acceptance) echo_samples: usize,
    pub(in crate::acceptance) mixed_duration: Duration,
    pub(in crate::acceptance) mixed_sessions: usize,
    pub(in crate::acceptance) sample_interval: Duration,
    pub(in crate::acceptance) throughput_lines: usize,
    pub(in crate::acceptance) output_dir: Option<PathBuf>,
    pub(in crate::acceptance) shell_program: Option<OsString>,
}

/// 在启动异步 runtime 前解析参数，避免压力任务运行中途发现无效配置。
pub(in crate::acceptance) fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        session_counts: vec![1, 10, 50],
        churn_cycles: 100,
        echo_samples: 10,
        mixed_duration: Duration::from_secs(7200),
        mixed_sessions: 10,
        sample_interval: Duration::from_millis(1000),
        throughput_lines: 128,
        output_dir: None,
        shell_program: None,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        let name = argument.to_string_lossy();
        match name.as_ref() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--session-counts" => {
                let value = next_value(&mut args, "--session-counts")?;
                options.session_counts = parse_counts(&value.to_string_lossy())?;
            }
            "--churn-cycles" => {
                options.churn_cycles = parse_usize(&mut args, "--churn-cycles")?;
            }
            "--echo-samples" => {
                options.echo_samples = parse_usize(&mut args, "--echo-samples")?;
            }
            "--mixed-duration-secs" => {
                options.mixed_duration =
                    Duration::from_secs(parse_u64(&mut args, "--mixed-duration-secs")?);
            }
            "--mixed-sessions" => {
                options.mixed_sessions = parse_usize(&mut args, "--mixed-sessions")?;
            }
            "--sample-interval-ms" => {
                options.sample_interval =
                    Duration::from_millis(parse_u64(&mut args, "--sample-interval-ms")?);
            }
            "--throughput-lines" => {
                options.throughput_lines = parse_usize(&mut args, "--throughput-lines")?;
            }
            "--output-dir" => {
                options.output_dir = Some(PathBuf::from(next_value(&mut args, "--output-dir")?));
            }
            "--shell-program" => {
                options.shell_program = Some(next_value(&mut args, "--shell-program")?);
            }
            _ => return Err(format!("未知参数：{name}；使用 --help 查看参数")),
        }
    }
    if options.session_counts.is_empty()
        || options
            .session_counts
            .iter()
            .any(|count| *count == 0 || *count > 256)
    {
        return Err("--session-counts 数量必须在 1..=256".to_owned());
    }
    if options.mixed_sessions == 0 || options.mixed_sessions > 256 {
        return Err("--mixed-sessions 必须在 1..=256".to_owned());
    }
    if options.churn_cycles == 0 || options.churn_cycles > 100_000 {
        return Err("--churn-cycles 必须在 1..=100000".to_owned());
    }
    if options.echo_samples == 0 || options.echo_samples > 10_000 {
        return Err("--echo-samples 必须在 1..=10000".to_owned());
    }
    if options.sample_interval < Duration::from_millis(50) {
        return Err("--sample-interval-ms 不能小于 50".to_owned());
    }
    if options.sample_interval > Duration::from_secs(60) {
        return Err("--sample-interval-ms 不能大于 60000".to_owned());
    }
    if options.mixed_duration > Duration::from_secs(172_800) {
        return Err("--mixed-duration-secs 不能大于 172800".to_owned());
    }
    if options.throughput_lines == 0 || options.throughput_lines > 100_000 {
        return Err("--throughput-lines 必须在 1..=100000".to_owned());
    }
    Ok(options)
}

/// 生成 CLI 帮助，使 xtask 和本 example 共享同一可读参数约定。
fn print_help() {
    println!(
        "session-acceptance [--session-counts 1,10,50] [--churn-cycles 100] [--echo-samples 10] [--mixed-duration-secs 7200] [--mixed-sessions 10] [--sample-interval-ms 1000] [--throughput-lines 128] [--output-dir PATH] [--shell-program PATH]"
    );
}

/// 读取必填的下一项参数，并将选项名保留在错误消息中。
fn next_value(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<OsString, String> {
    args.next().ok_or_else(|| format!("{name} 缺少参数值"))
}

/// 解析无符号整数参数，并拒绝负数、空值和溢出。
fn parse_u64(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<u64, String> {
    let value = next_value(args, name)?;
    value
        .to_string_lossy()
        .parse()
        .map_err(|_| format!("{name} 必须是非负整数"))
}

/// 将整数参数收窄为 usize，同时保留平台溢出错误。
fn parse_usize(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<usize, String> {
    usize::try_from(parse_u64(args, name)?).map_err(|_| format!("{name} 超出当前平台范围"))
}

/// 解析逗号分隔的会话规模，保留传入顺序并拒绝重复项。
fn parse_counts(value: &str) -> Result<Vec<usize>, String> {
    let mut counts = Vec::new();
    for part in value.split(',') {
        let count = part
            .trim()
            .parse::<usize>()
            .map_err(|_| "--session-counts 格式应为 1,10,50".to_owned())?;
        if counts.contains(&count) {
            return Err("--session-counts 不允许重复数量".to_owned());
        }
        counts.push(count);
    }
    Ok(counts)
}
