mod artifact;
mod cli;
mod contracts;
mod process;
mod result_schema;
mod runner;
mod sockets;

pub use artifact::{ArtifactError, ArtifactReceipt, write_artifact};
pub use cli::{CliExecution, command_manifest, command_model, run_cli};
pub use contracts::{ContractError, ContractFailure, ContractReport, ContractSuite};
pub use process::{
    Capture, CompletionReason, ProcessError, ProcessOutcome, ProcessSpec, Termination, run_process,
};
pub use result_schema::{SpikeOutcome, SpikeResult, spike_result_schema};
pub use runner::{
    CommandOutput, CommandRequest, CommandRunner, PlatformProbe, PlatformTools, TokioCommandRunner,
};
pub use sockets::{LocalEndpoint, SocketError, round_trip_local};
