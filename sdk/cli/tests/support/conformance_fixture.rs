//! 自包含测试 Sidecar，不依赖开发机的 Node 或真实用户数据。
//! @author kongweiguang

use std::io::{self, BufRead, Write};

/// 帧由测试工具生成，测试程序只需要提取其中的简单字符串 ID。
fn id(frame: &str) -> Option<&str> {
    frame
        .split_once("\"id\":\"")?
        .1
        .split_once('"')
        .map(|(id, _)| id)
}

/// 通过标准输出模拟握手、过载、取消和关闭，便于验证真实进程边界。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut output = io::stdout().lock();
    let mut blocked = 0usize;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.contains("\"method\":\"initialize\"") {
            writeln!(
                output,
                "{{\"jsonrpc\":\"2.0\",\"id\":\"h:initialize\",\"result\":{{\"protocol\":1,\"plugin\":{{\"id\":\"acme.test\",\"version\":\"1.0.0\"}}}}}}"
            )?;
        } else if line.contains("\"method\":\"$/ping\"") {
            writeln!(
                output,
                "{{\"jsonrpc\":\"2.0\",\"id\":\"h:ping\",\"result\":{{}}}}"
            )?;
        } else if line.contains("\"method\":\"conformance.block\"") {
            blocked += 1;
            if blocked == 65 {
                writeln!(
                    output,
                    "{{\"jsonrpc\":\"2.0\",\"id\":\"h:probe64\",\"error\":{{\"code\":-32003,\"message\":\"overloaded\"}}}}"
                )?;
            }
        } else if line.contains("\"method\":\"$/cancel\"") {
            let request = id(&line).ok_or("missing cancel id")?;
            writeln!(
                output,
                "{{\"jsonrpc\":\"2.0\",\"id\":\"{request}\",\"error\":{{\"code\":-32002,\"message\":\"cancelled\"}}}}"
            )?;
        } else if line.contains("\"method\":\"shutdown\"") {
            writeln!(
                output,
                "{{\"jsonrpc\":\"2.0\",\"id\":\"h:shutdown\",\"result\":{{}}}}"
            )?;
            output.flush()?;
            break;
        }
        output.flush()?;
    }
    Ok(())
}
