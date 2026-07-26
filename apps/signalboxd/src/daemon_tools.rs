//! Process-lifetime compiled daemon tool catalog and executor dispatch.

use std::{collections::BTreeMap, error::Error, fmt};

use signalbox_application::{
    ClassifyOperatorFailure, CompiledToolCatalog, CorrelatedToolExecutorEvidence,
    OperatorFailureClass, ToolCatalog, ToolCatalogValidationFailure, ToolDefinition,
    ToolExecutionInvocation, ToolExecutor,
};
use signalbox_domain::{NormalizedToolArguments, ToolName};
use sqlx::PgPool;

use crate::{
    CurrentTimeClock, CurrentTimeExecutor, CurrentTimeTool, EchoExecutor, EchoTool,
    PostgresSessionStatusWriter, ReqwestWebFetchTransport, SessionStatusExecutor,
    SessionStatusTool, SessionStatusWriter, WebFetchExecutor, WebFetchTool, WebFetchTransport,
    current_time::CURRENT_TIME_NAME, echo::ECHO_NAME, session_status::SESSION_STATUS_UPDATE_NAME,
    web_fetch::WEB_FETCH_NAME,
};

/// The four daemon-local declarations and their matching dispatch executor.
#[derive(Clone, Debug)]
pub struct DaemonTools<Clock, Transport, Writer> {
    catalog: DaemonToolCatalog,
    executor: DaemonToolExecutor<Clock, Transport, Writer>,
}

impl<Clock> DaemonTools<Clock, ReqwestWebFetchTransport, PostgresSessionStatusWriter> {
    /// Composes the production catalog, credential-free web transport, and
    /// PostgreSQL metadata writer.
    pub fn try_new_production(
        clock: Clock,
        pool: PgPool,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let web_fetch = WebFetchTool::try_new_production()
            .map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let status = SessionStatusTool::try_new_postgres(pool)
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        Self::try_new_with_tools(clock, web_fetch, status)
    }
}

impl<Clock, Transport, Writer> DaemonTools<Clock, Transport, Writer> {
    /// Composes the catalog around injected web and metadata boundaries.
    pub fn try_new(
        clock: Clock,
        transport: Transport,
        writer: Writer,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let web_fetch =
            WebFetchTool::try_new(transport).map_err(|_| DaemonToolsConstructionError::WebFetch)?;
        let status = SessionStatusTool::try_new(writer)
            .map_err(|_| DaemonToolsConstructionError::SessionStatus)?;
        Self::try_new_with_tools(clock, web_fetch, status)
    }

    fn try_new_with_tools(
        clock: Clock,
        web_fetch: WebFetchTool<Transport>,
        status: SessionStatusTool<Writer>,
    ) -> Result<Self, DaemonToolsConstructionError> {
        let (current_time_catalog, current_time) = CurrentTimeTool::try_new(clock)
            .map_err(|_| DaemonToolsConstructionError::CurrentTime)?
            .into_parts();
        let (echo_catalog, echo) = EchoTool::try_new()
            .map_err(|_| DaemonToolsConstructionError::Echo)?
            .into_parts();
        let (web_fetch_catalog, web_fetch) = web_fetch.into_parts();
        let (status_catalog, session_status) = status.into_parts();
        let catalog = DaemonToolCatalog::try_new([
            current_time_catalog,
            echo_catalog,
            web_fetch_catalog,
            status_catalog,
        ])
        .map_err(|_| DaemonToolsConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: DaemonToolExecutor {
                current_time,
                echo,
                web_fetch,
                session_status,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(
        self,
    ) -> (
        DaemonToolCatalog,
        DaemonToolExecutor<Clock, Transport, Writer>,
    ) {
        (self.catalog, self.executor)
    }
}

/// Why the daemon-local tool set could not be composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonToolsConstructionError {
    /// The current-time declaration was invalid.
    CurrentTime,
    /// The echo declaration was invalid.
    Echo,
    /// The web-fetch declaration or transport was invalid.
    WebFetch,
    /// The session-status declaration was invalid.
    SessionStatus,
    /// Two declarations unexpectedly shared one name.
    Duplicate,
}

impl fmt::Display for DaemonToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CurrentTime => "current_time tool construction failed",
            Self::Echo => "echo tool construction failed",
            Self::WebFetch => "web_fetch tool construction failed",
            Self::SessionStatus => "session_status_update tool construction failed",
            Self::Duplicate => "daemon tool catalog contains a duplicate name",
        })
    }
}

impl Error for DaemonToolsConstructionError {}

#[derive(Clone, Debug)]
struct DaemonToolCatalogEntry {
    definition: ToolDefinition,
    catalog: CompiledToolCatalog,
}

/// Stable merged view of independently compiled daemon tool modules.
#[derive(Clone, Debug)]
pub struct DaemonToolCatalog {
    entries: BTreeMap<ToolName, DaemonToolCatalogEntry>,
}

impl DaemonToolCatalog {
    fn try_new(
        catalogs: impl IntoIterator<Item = CompiledToolCatalog>,
    ) -> Result<Self, DuplicateDaemonTool> {
        let mut entries = BTreeMap::new();
        for catalog in catalogs {
            for definition in catalog.definitions() {
                let name = definition.name().clone();
                if entries
                    .insert(
                        name.clone(),
                        DaemonToolCatalogEntry {
                            definition,
                            catalog: catalog.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(DuplicateDaemonTool);
                }
            }
        }
        Ok(Self { entries })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DuplicateDaemonTool;

impl ToolCatalog for DaemonToolCatalog {
    fn definitions(&self) -> Box<[ToolDefinition]> {
        self.entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    fn definition(&self, name: &ToolName) -> Option<ToolDefinition> {
        self.entries.get(name).map(|entry| entry.definition.clone())
    }

    fn validate_arguments(
        &self,
        name: &ToolName,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolCatalogValidationFailure> {
        self.entries
            .get(name)
            .ok_or(ToolCatalogValidationFailure::UnknownTool)?
            .catalog
            .validate_arguments(name, arguments)
    }
}

/// Name-directed daemon executor matching [`DaemonToolCatalog`].
#[derive(Clone, Debug)]
pub struct DaemonToolExecutor<Clock, Transport, Writer> {
    current_time: CurrentTimeExecutor<Clock>,
    echo: EchoExecutor,
    web_fetch: WebFetchExecutor<Transport>,
    session_status: SessionStatusExecutor<Writer>,
}

/// Sanitized aggregate executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonToolExecutorError {
    class: OperatorFailureClass,
}

impl DaemonToolExecutorError {
    fn from_error(error: &impl ClassifyOperatorFailure) -> Self {
        Self {
            class: error.operator_failure_class(),
        }
    }
}

impl fmt::Display for DaemonToolExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("daemon tool executor failed")
    }
}

impl Error for DaemonToolExecutorError {}

impl ClassifyOperatorFailure for DaemonToolExecutorError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        self.class
    }
}

impl<Clock, Transport, Writer> ToolExecutor for DaemonToolExecutor<Clock, Transport, Writer>
where
    Clock: CurrentTimeClock,
    Transport: WebFetchTransport,
    Writer: SessionStatusWriter,
{
    type Error = DaemonToolExecutorError;

    async fn execute(
        &mut self,
        invocation: ToolExecutionInvocation,
    ) -> Result<CorrelatedToolExecutorEvidence, Self::Error> {
        match invocation.request().name().as_str() {
            CURRENT_TIME_NAME => self
                .current_time
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            ECHO_NAME => self
                .echo
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            WEB_FETCH_NAME => self
                .web_fetch
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            SESSION_STATUS_UPDATE_NAME => self
                .session_status
                .execute(invocation)
                .await
                .map_err(|error| DaemonToolExecutorError::from_error(&error)),
            _ => Err(DaemonToolExecutorError {
                class: OperatorFailureClass::CallerOrHubBug,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, time::SystemTime};

    use signalbox_application::ToolCatalog;

    use super::*;
    use crate::{
        SessionStatusWrite, SessionStatusWriteOutcome, WebFetchRequest, WebFetchResponse,
        WebFetchTransportFailure,
    };

    #[derive(Clone, Copy, Debug)]
    struct OfflineTransport;

    impl WebFetchTransport for OfflineTransport {
        async fn fetch(
            &mut self,
            _request: WebFetchRequest,
        ) -> Result<WebFetchResponse, WebFetchTransportFailure> {
            Err(WebFetchTransportFailure::RequestFailed)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct OfflineWriterError;

    impl fmt::Display for OfflineWriterError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("offline writer is not invoked")
        }
    }

    impl Error for OfflineWriterError {}

    impl ClassifyOperatorFailure for OfflineWriterError {
        fn operator_failure_class(&self) -> OperatorFailureClass {
            OperatorFailureClass::CallerOrHubBug
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OfflineWriter;

    impl SessionStatusWriter for OfflineWriter {
        type Error = OfflineWriterError;

        async fn write(
            &mut self,
            _update: SessionStatusWrite,
        ) -> Result<SessionStatusWriteOutcome, Self::Error> {
            Err(OfflineWriterError)
        }
    }

    /// The merged process-lifetime catalog exposes every daemon declaration in
    /// deterministic name order.
    #[test]
    fn daemon_catalog_contains_current_time_and_tier_zero() {
        let (catalog, _executor) =
            DaemonTools::try_new(|| SystemTime::UNIX_EPOCH, OfflineTransport, OfflineWriter)
                .expect("static daemon tools compile")
                .into_parts();

        let definitions = catalog.definitions();
        let names: Vec<&str> = definitions
            .iter()
            .map(|definition| definition.name().as_str())
            .collect();

        assert_eq!(
            names,
            vec![
                CURRENT_TIME_NAME,
                ECHO_NAME,
                SESSION_STATUS_UPDATE_NAME,
                WEB_FETCH_NAME,
            ]
        );
    }
}
