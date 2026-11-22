//! Orchestration agents: a stage's agent hands self-contained tasks to other
//! harnesses and models through the router. See `docs/delegation.md`.
pub mod config;
pub mod policy;
pub mod records;

pub use config::{Delegate, Delegation, Isolation};
