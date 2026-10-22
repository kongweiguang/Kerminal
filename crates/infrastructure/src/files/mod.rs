//! 本机与 SFTP 文件系统适配器、传输执行器和路径书签存储。
//!
//! @author kongweiguang

mod bookmarks;
mod local;
mod sftp;
mod transfer;
mod transfer_recovery;

pub use bookmarks::JsonFileBookmarkRepository;
pub use local::LocalFileProvider;
pub use sftp::SftpFileProvider;
pub use transfer::SftpTransferBackend;
pub use transfer_recovery::{JsonTransferRecoveryRepository, validate_transfer_recovery_document};
