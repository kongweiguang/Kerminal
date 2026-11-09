//! 原生技术探针的 GPUI 边界；业务插件装配留给后续阶段。
//!
//! @author kongweiguang

#[cfg(feature = "acceptance-metrics")]
pub mod native_probe;
pub mod webview;
