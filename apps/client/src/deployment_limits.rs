use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientDeploymentLimits {
    pub(crate) max_message_utf8_bytes: Option<usize>,
    pub(crate) max_system_prompt_utf8_bytes: Option<usize>,
    pub(crate) terminal_input_channel_capacity: Option<usize>,
    pub(crate) min_metadata_page_size: Option<u64>,
    pub(crate) max_metadata_page_size: Option<u64>,
    pub(crate) max_review_findings_per_run: Option<u64>,
}

impl ClientDeploymentLimits {
    #[cfg(test)]
    pub(crate) const fn unbounded() -> Self {
        Self {
            max_message_utf8_bytes: None,
            max_system_prompt_utf8_bytes: None,
            terminal_input_channel_capacity: None,
            min_metadata_page_size: None,
            max_metadata_page_size: None,
            max_review_findings_per_run: None,
        }
    }
}

pub(crate) async fn read_deployment_limits(
    client: &mut ProcessClient,
) -> Result<ClientDeploymentLimits, ClientError> {
    let mut connection = client
        .request(ClientRequest::ReadDeploymentLimits {})
        .await?;
    match connection.message().await? {
        ServerMessage::DeploymentLimits {
            max_message_utf8_bytes,
            max_system_prompt_utf8_bytes,
            terminal_input_channel_capacity,
            min_metadata_page_size,
            max_metadata_page_size,
            max_review_findings_per_run,
        } => Ok(ClientDeploymentLimits {
            max_message_utf8_bytes: optional_usize_limit(max_message_utf8_bytes)?,
            max_system_prompt_utf8_bytes: optional_usize_limit(max_system_prompt_utf8_bytes)?,
            terminal_input_channel_capacity: optional_usize_limit(terminal_input_channel_capacity)?,
            min_metadata_page_size: min_metadata_page_size.map(CanonicalU64::value),
            max_metadata_page_size: max_metadata_page_size.map(CanonicalU64::value),
            max_review_findings_per_run: max_review_findings_per_run.map(CanonicalU64::value),
        }),
        ServerMessage::Error {
            code,
            message,
            detail,
        } => Err(ClientError::remote(code, message, detail)),
        _ => Err(ClientError::Protocol(
            "deployment limits read returned an unexpected response",
        )),
    }
}

fn optional_usize_limit(value: Option<CanonicalU64>) -> Result<Option<usize>, ClientError> {
    value
        .map(|value| {
            usize::try_from(value.value())
                .map_err(|_| ClientError::Protocol("deployment limit is not representable"))
        })
        .transpose()
}

pub(crate) fn command_uses_deployment_limits(command: &Command) -> bool {
    match command {
        Command::Send { .. }
        | Command::Steer { .. }
        | Command::Reconcile { .. }
        | Command::Stop { .. }
        | Command::Search(_)
        | Command::Conversations(_)
        | Command::Create {
            system_prompt_file: Some(_),
            ..
        }
        | Command::Model {
            system_prompt: SystemPromptArgument::File(_),
            ..
        } => true,
        Command::Review(command) => matches!(
            command.as_ref(),
            ReviewCommand::RecordFinding { .. }
                | ReviewCommand::RecordFindings { .. }
                | ReviewCommand::ListFindings { .. }
        ),
        _ => false,
    }
}

pub(crate) fn validate_message_policy(
    content: &str,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_message_utf8_bytes
        .is_some_and(|maximum| content.len() > maximum)
    {
        return Err(ClientError::Input(
            "standard-input content exceeds the deployment UTF-8 byte limit",
        ));
    }
    Ok(())
}

pub(crate) fn validate_system_prompt_policy(
    system_prompt: &SystemPromptText,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .max_system_prompt_utf8_bytes
        .is_some_and(|maximum| system_prompt.as_str().len() > maximum)
    {
        return Err(ClientError::Input(
            "system prompt exceeds the deployment UTF-8 byte limit",
        ));
    }
    Ok(())
}

pub(crate) fn validate_metadata_page_policy(
    page_size: CanonicalU64,
    limits: Option<ClientDeploymentLimits>,
) -> Result<(), ClientError> {
    let limits = limits.ok_or(ClientError::Protocol("deployment limits were not read"))?;
    if limits
        .min_metadata_page_size
        .is_some_and(|minimum| page_size.value() < minimum)
        || limits
            .max_metadata_page_size
            .is_some_and(|maximum| page_size.value() > maximum)
    {
        return Err(ClientError::Input(
            "the result limit is outside the deployment page-size range",
        ));
    }
    Ok(())
}
