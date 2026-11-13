//! 工作区笔记 Sidecar 入口；业务与另一代共用，版本由构建清单注入。
//! @author kongweiguang

include!("../../../../../support/sidecars/protocol.rs");
include!("../../../../../support/sidecars/note_text.rs");
mod plugin {
    use super::*;
    include!("../../../../../support/sidecars/notes_impl.rs");
}
