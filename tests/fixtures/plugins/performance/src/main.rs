//! 插件性能洪泛夹具的 Sidecar 入口。
//! @author kongweiguang

include!("../../../../support/sidecars/protocol.rs");
mod plugin {
    use super::*;
    include!("../../../../support/sidecars/performance_impl.rs");
}
