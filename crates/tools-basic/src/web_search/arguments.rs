use signalbox_application::ToolArgumentValidator;
use signalbox_domain::{NormalizedToolArguments, ToolExecutionErrorDetail};
use std::fmt;

use super::{egress::*, request::*};

pub(super) const INVALID_ARGUMENTS_DETAIL: &str =
    "expected a nonempty web search query of at most 400 characters and 50 words";

pub(super) const MAX_QUERY_CHARACTERS: usize = 400;

pub(super) const MAX_QUERY_WORDS: usize = 50;

pub(super) const MAX_QUERY_BYTES: usize = MAX_QUERY_CHARACTERS * 4;

/// Typed `web_search` argument shape; decoder and schema share it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArguments {
    /// Nonempty query of at most 400 characters and 50 words.
    pub(super) query: WebSearchQuery,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(try_from = "String")]
pub(super) struct WebSearchQuery(String);

impl schemars::JsonSchema for WebSearchQuery {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WebSearchQuery")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "maxLength": MAX_QUERY_CHARACTERS,
            "minLength": 1,
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

impl TryFrom<String> for WebSearchQuery {
    type Error = InvalidWebSearchArguments;

    fn try_from(query: String) -> Result<Self, Self::Error> {
        if query.len() > MAX_QUERY_BYTES
            || query.trim().is_empty()
            || query.chars().count() > MAX_QUERY_CHARACTERS
            || query.split_whitespace().count() > MAX_QUERY_WORDS
        {
            return Err(InvalidWebSearchArguments);
        }
        Ok(Self(query))
    }
}

#[derive(Clone, Debug)]
pub(super) struct WebSearchArgumentValidator {
    pub(super) detail: ToolExecutionErrorDetail,
}

impl ToolArgumentValidator for WebSearchArgumentValidator {
    fn validate(
        &self,
        arguments: &NormalizedToolArguments,
    ) -> Result<(), ToolExecutionErrorDetail> {
        decode_arguments(arguments)
            .map(|_| ())
            .map_err(|_| self.detail.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidWebSearchArguments;

impl fmt::Display for InvalidWebSearchArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(INVALID_ARGUMENTS_DETAIL)
    }
}

pub(super) fn decode_arguments(
    arguments: &NormalizedToolArguments,
) -> Result<WebSearchQuery, InvalidWebSearchArguments> {
    let decoded: WebSearchArguments =
        serde_json::from_str(arguments.as_str()).map_err(|_| InvalidWebSearchArguments)?;
    Ok(decoded.query)
}

pub(super) fn decode_arguments_for_provider(
    arguments: &NormalizedToolArguments,
    provider: WebSearchProvider,
) -> Result<WebSearchRequest, InvalidWebSearchArguments> {
    let query = decode_arguments(arguments)?;
    Ok(WebSearchRequest {
        provider,
        query: query.0,
    })
}
