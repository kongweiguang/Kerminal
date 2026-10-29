//! 本机 GPU 探针供服务器资源监控复用，避免为已移除的 MCP 诊断入口保留采集与归档。
//!
//! @author kongweiguang

pub(crate) mod gpu;

#[cfg(test)]
#[path = "../tests/support/diagnostics_health.rs"]
mod gpu_tests;
