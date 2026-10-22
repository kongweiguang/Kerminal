//! 容器文件复制共享唯一传输队列，文件内容走受限 tar 管道。
//!
//! @author kongweiguang

mod archive;
mod backend;
mod download;
mod local_commit;
mod model;
mod upload;

pub use backend::ContainerTransferBackend;
pub(super) use backend::{
    CLEANUP_STAGE_SCRIPT, CREATE_STAGE_SCRIPT, stage_cleanup_invocation, stage_owner_marker,
};
