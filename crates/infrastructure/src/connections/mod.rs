//! 远程连接基础设施适配器。
//!
//! @author kongweiguang

mod proxy;
mod ssh;
mod terminal;

pub(crate) mod exec;
pub mod external_runtime;
pub mod known_hosts;
pub mod management;
pub mod probe;
pub mod rdp;
pub mod serial;
pub mod telnet;

pub use external_runtime::ExternalRuntimeRegistry;
pub use known_hosts::KnownHostsStore;
pub use rdp::SystemRdpPlatformLauncher;
pub use ssh::{
    RusshConnectionCleanupStatus, RusshRemoteConnectionDependencies, RusshRemoteConnectionService,
};
pub use terminal::SshSessionTransportFactory;
