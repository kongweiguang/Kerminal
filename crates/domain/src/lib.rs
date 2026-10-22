//! Domain primitives and invariants for Kerminal.
//!
//! @author kongweiguang

pub mod agents;
pub mod commands;
pub mod connections;
pub mod error;
pub mod files;
pub mod identity;
pub mod operations;
pub mod preferences;
pub mod sessions;
pub mod workspace;

pub use error::DomainError;
