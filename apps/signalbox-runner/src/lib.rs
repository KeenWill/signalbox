//! Enrollment, liveness, and serial pure-tool execution runtime for `signalbox-runner`.

mod configuration;
mod executor;
mod journal;
mod protocol;
mod state;

pub use executor::{ECHO_CHILD_ARGUMENT, run_echo_child};

pub use configuration::{
    AllowedNetworkHost, ArgumentError, RunnerConfiguration, RunnerConfigurationError,
    RunnerConfigurationPath, RunnerCredentialConfiguration, RunnerRepositoryConfiguration,
};
pub use protocol::{
    ConnectionEnd, EnrollmentOutcome, MessageKind, ProtocolViolation, RecoveryGap,
    RecoveryUnavailable, RunnerConnection, RunnerConnectionError, ServeOutcome, SocketConnectError,
    connect_verified,
};
pub use state::{
    EnrollmentAuthority, EnrollmentReceipt, RunnerState, RunnerStateError, RunnerStateRoot,
    StateOperation, StateResource,
};
