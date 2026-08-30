//! Agent-facing desktop CLI contract core.

mod catalog;
mod contracts;
mod core;
mod protocol;
mod registry;
mod result;

pub use catalog::{
    CatalogDispatchPhase, CatalogExchangeError, CatalogExchangeFailure, CatalogRuntime,
    CatalogSelectError, FakeCatalogRuntime, OpenedProtocolSession, SessionCandidate,
    SessionSelector, UnconfiguredCatalogRuntime,
};
pub use core::{CliConfig, CliCore, InitError, ProcessOutput};
