//! Agent-facing desktop CLI contract core.

mod contracts;
mod core;
mod registry;
mod result;

pub use core::{CliConfig, CliCore, InitError, ProcessOutput};
