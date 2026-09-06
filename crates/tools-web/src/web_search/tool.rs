use std::fmt;

use signalbox_application::{CompiledTool, CompiledToolCatalog};
use signalbox_domain::{ToolEffectClass, ToolExecutionErrorDetail, ToolPermissionDefault};
use signalbox_tool_contract::{
    ToolContract, ToolContractCompileError, compile_contract_definition,
};

use super::{arguments::*, egress::*, executor::*, transport::*};

/// Registry name for bounded web search.
pub const WEB_SEARCH_NAME: &str = "web_search";

pub(super) const CREDENTIAL_UNAVAILABLE_DETAIL: &str = "web search credential is unavailable";

pub(super) const REQUEST_FAILED_DETAIL: &str = "web search request failed";

pub(super) const PROVIDER_REJECTED_DETAIL: &str = "web search provider rejected the request";

pub(super) const INVALID_RESPONSE_DETAIL: &str =
    "web search provider returned an invalid bounded response";

#[derive(signalbox_derive::OperatorError)]
/// A static declaration or production search transport could not be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebSearchToolConstructionError {
    #[error("web_search static name is invalid")]
    /// The static name was rejected.
    Name,
    #[error("web_search static schema is invalid")]
    /// The static schema was rejected.
    Schema,
    #[error("web_search static error detail is invalid")]
    /// A static sanitized error detail was rejected.
    ErrorDetail,
    #[error("web_search catalog is duplicated")]
    /// The one-entry catalog unexpectedly reported a duplicate.
    Duplicate,
    #[error("web_search transport could not be constructed")]
    /// The production transport could not be constructed.
    Transport,
}

/// Compiled catalog entry and matching credential-resolving executor.
#[derive(Clone)]
pub struct WebSearchTool<Credentials, Transport> {
    pub(super) catalog: CompiledToolCatalog,
    pub(super) executor: WebSearchExecutor<Credentials, Transport>,
}

impl<Credentials, Transport> fmt::Debug for WebSearchTool<Credentials, Transport> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchTool")
            .field("catalog", &self.catalog)
            .field("executor", &self.executor)
            .finish()
    }
}

impl<Credentials, Transport> ToolContract for WebSearchTool<Credentials, Transport> {
    type Arguments = WebSearchArguments;
    const NAME: &'static str = WEB_SEARCH_NAME;
    const DESCRIPTION: &'static str =
        "Searches the web through the explicitly configured provider and returns bounded results.";
}

impl<Credentials> WebSearchTool<Credentials, ReqwestWebSearchTransport> {
    /// Builds the production tool with the fixed bounded transport policy.
    pub fn try_new_production(
        credentials: Credentials,
        configuration: WebSearchConfiguration,
    ) -> Result<Self, WebSearchToolConstructionError> {
        let transport = ReqwestWebSearchTransport::try_new(DEFAULT_EXCHANGE_TIMEOUT)
            .map_err(|_| WebSearchToolConstructionError::Transport)?;
        Self::try_new(credentials, transport, configuration)
    }
}

impl<Credentials, Transport> WebSearchTool<Credentials, Transport> {
    /// Compiles immutable metadata around injected credential and transport boundaries.
    pub fn try_new(
        credentials: Credentials,
        transport: Transport,
        configuration: WebSearchConfiguration,
    ) -> Result<Self, WebSearchToolConstructionError> {
        let invalid_arguments_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_ARGUMENTS_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let credential_unavailable_detail =
            ToolExecutionErrorDetail::try_new(String::from(CREDENTIAL_UNAVAILABLE_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let request_failed_detail =
            ToolExecutionErrorDetail::try_new(String::from(REQUEST_FAILED_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let provider_rejected_detail =
            ToolExecutionErrorDetail::try_new(String::from(PROVIDER_REJECTED_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let invalid_response_detail =
            ToolExecutionErrorDetail::try_new(String::from(INVALID_RESPONSE_DETAIL))
                .map_err(|_| WebSearchToolConstructionError::ErrorDetail)?;
        let definition = compile_contract_definition::<Self>(
            ToolPermissionDefault::Confirm,
            ToolEffectClass::ExternalEffect,
        )
        .map_err(|error| match error {
            ToolContractCompileError::Name => WebSearchToolConstructionError::Name,
            ToolContractCompileError::Schema => WebSearchToolConstructionError::Schema,
        })?;
        let catalog = CompiledToolCatalog::try_new([CompiledTool::new(
            definition,
            WebSearchArgumentValidator {
                detail: invalid_arguments_detail,
            },
        )])
        .map_err(|_| WebSearchToolConstructionError::Duplicate)?;
        Ok(Self {
            catalog,
            executor: WebSearchExecutor {
                credentials,
                transport,
                configuration,
                credential_unavailable_detail,
                request_failed_detail,
                provider_rejected_detail,
                invalid_response_detail,
            },
        })
    }

    /// Returns the catalog and executor as separate composition roles.
    pub fn into_parts(
        self,
    ) -> (
        CompiledToolCatalog,
        WebSearchExecutor<Credentials, Transport>,
    ) {
        (self.catalog, self.executor)
    }
}
