//! Enrollment, registration, and liveness runtime for `signalbox-runner`.

mod configuration;
mod dispatch_https;
mod https_broker;
mod protocol;
mod state;
mod workspace;

pub use configuration::{
    AllowedNetworkHost, ArgumentError, RunnerConfiguration, RunnerConfigurationError,
    RunnerConfigurationPath, RunnerCredentialConfiguration, RunnerRepositoryConfiguration,
};
pub use dispatch_https::{DispatchHttpsEndpoint, DispatchHttpsError};
pub use https_broker::{
    HttpsBroker, HttpsBrokerError, HttpsConnector, HttpsHostResolver, TokioHttpsConnector,
    TokioHttpsHostResolver,
};
pub use protocol::{
    ConnectionEnd, EnrollmentOutcome, MessageKind, ProtocolViolation, RecoveryGap,
    RecoveryUnavailable, RunnerConnection, RunnerConnectionError, RunnerDispatchReady,
    RunnerWorkspaceReleaseReady, ServeOutcome, SocketConnectError, connect_verified,
};
pub use state::{
    AcceptedWorkspaceRelease, EnrollmentAuthority, EnrollmentReceipt, RunnerState,
    RunnerStateError, RunnerStateRoot, StateOperation, StateResource,
};
pub use workspace::{PrivateWorkspaceRequest, RunnerWorkspaceError, RunnerWorkspaceStore};
