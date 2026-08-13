//! Enrollment, registration, and liveness runtime for `signalbox-runner`.

mod configuration;
mod protocol;
mod state;

pub use configuration::{
    AllowedNetworkHost, ArgumentError, RunnerConfiguration, RunnerConfigurationError,
    RunnerConfigurationPath, RunnerCredentialConfiguration, RunnerRepositoryConfiguration,
};
pub use protocol::{
    ConnectionEnd, EnrollmentOutcome, MessageKind, ProtocolViolation, RecoveryGap,
    RecoveryUnavailable, RunnerConnection, RunnerConnectionError, RunnerDispatchReady,
    ServeOutcome, SocketConnectError, connect_verified,
};
pub use state::{
    EnrollmentAuthority, EnrollmentReceipt, RunnerState, RunnerStateError, RunnerStateRoot,
    StateOperation, StateResource,
};
