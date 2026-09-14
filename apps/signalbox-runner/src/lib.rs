//! Enrollment, liveness, and serial pure-tool execution runtime for `signalbox-runner`.

mod active_workspaces;
mod ambient;
mod configuration;
mod executor;
mod journal;
mod protocol;
mod state;
mod workspace;

pub use ambient::{AMBIENT_PROBE_ARGUMENT, run_ambient_probe_child};
pub use executor::{ECHO_CHILD_ARGUMENT, run_echo_child};
pub use workspace::provision::WorkspaceProvisionError;

pub use configuration::{
    AllowedNetworkHost, ArgumentError, RunnerConfiguration, RunnerConfigurationError,
    RunnerConfigurationPath, RunnerCredentialConfiguration, RunnerRepositoryConfiguration,
};
pub use protocol::{
    ConnectionEnd, EnrollmentOutcome, MessageKind, ProtocolViolation, RunnerConnection,
    RunnerConnectionError, ServeOutcome, SocketConnectError, WorkspaceReleaseWorker,
    connect_verified,
};
pub use state::{
    EnrollmentAuthority, EnrollmentReceipt, RunnerState, RunnerStateError, RunnerStateRoot,
    StateOperation, StateResource,
};
