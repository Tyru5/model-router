//! Orchestration agents: a stage's agent hands self-contained tasks to other
//! harnesses and models through the router. See `docs/delegation.md`.
pub mod bridge;
pub mod broker;
pub mod config;
pub mod instructions;
pub mod policy;
pub mod records;
pub mod schedules;
pub mod tools;

pub use config::{Delegate, Delegation, Isolation};
