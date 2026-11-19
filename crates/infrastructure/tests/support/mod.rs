//! 隔离集成测试公共夹具。
//!
//! @author kongweiguang

#[allow(dead_code, reason = "各独立集成测试仅使用共享 SSH 夹具的部分能力")]
pub mod ssh_harness;

#[allow(dead_code, reason = "只有 RSA 认证用例使用共享 key")]
pub mod ssh_test_keys;
