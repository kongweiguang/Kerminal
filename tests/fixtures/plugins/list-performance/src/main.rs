//! 十万行列表夹具的 Sidecar 入口。
//! @author kongweiguang

include!("../../../../support/sidecars/protocol.rs");
mod plugin {
    use super::*;
    include!("../../../../support/sidecars/list_impl.rs");
}
