use std::{collections::VecDeque, ffi::OsString, fmt, path::PathBuf};

use signalbox_process_protocol::{CanonicalU64, CanonicalUuid, CommandId, ModelSelection};
use uuid::Uuid;

pub(crate) const USAGE: &str = "\
usage:
  signalbox [--socket PATH] [--raw-output] create (--model UUID | --alias UUID) [--command-id UUID]
  signalbox [--socket PATH] [--raw-output] list
  signalbox [--socket PATH] [--raw-output] send SESSION [--command-id UUID --defaults-version DECIMAL]
  signalbox [--socket PATH] [--raw-output] transcript SESSION
  signalbox [--socket PATH] [--raw-output] follow SESSION";

#[derive(Debug)]
pub(crate) struct Arguments {
    pub(crate) socket: Option<PathBuf>,
    pub(crate) raw_output: bool,
    pub(crate) command: Command,
}

#[derive(Debug)]
pub(crate) enum Command {
    Create {
        selection: ModelSelection,
        command_id: Option<CommandId>,
    },
    List,
    Send {
        session_id: CanonicalUuid,
        command_id: Option<CommandId>,
        defaults_version: Option<CanonicalU64>,
    },
    Transcript {
        session_id: CanonicalUuid,
    },
    Follow {
        session_id: CanonicalUuid,
    },
}

#[derive(Debug)]
pub(crate) enum ParseOutcome {
    Help,
    Run(Arguments),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UsageError {
    message: &'static str,
}

impl UsageError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for UsageError {}

pub(crate) fn parse(
    values: impl IntoIterator<Item = OsString>,
) -> Result<ParseOutcome, UsageError> {
    let mut values = values.into_iter().collect::<VecDeque<_>>();
    let mut socket = None;
    let mut raw_output = false;

    loop {
        match front_text(&values)? {
            Some("--help" | "-h") => return Ok(ParseOutcome::Help),
            Some("--socket") => {
                values.pop_front();
                if socket.is_some() {
                    return Err(UsageError::new("--socket may be supplied only once"));
                }
                socket = Some(PathBuf::from(
                    values
                        .pop_front()
                        .ok_or(UsageError::new("--socket requires a path"))?,
                ));
            }
            Some("--raw-output") => {
                values.pop_front();
                if raw_output {
                    return Err(UsageError::new("--raw-output may be supplied only once"));
                }
                raw_output = true;
            }
            Some(value) if value.starts_with('-') => {
                return Err(UsageError::new("unknown global option"));
            }
            _ => break,
        }
    }

    let command = text(
        values
            .pop_front()
            .ok_or(UsageError::new("a command is required"))?,
    )?;
    let command = match command.as_str() {
        "create" => parse_create(&mut values)?,
        "list" => {
            require_empty(&values)?;
            Command::List
        }
        "send" => parse_send(&mut values)?,
        "transcript" => parse_session_only(&mut values, false)?,
        "follow" => parse_session_only(&mut values, true)?,
        _ => return Err(UsageError::new("unknown command")),
    };
    Ok(ParseOutcome::Run(Arguments {
        socket,
        raw_output,
        command,
    }))
}

fn parse_create(values: &mut VecDeque<OsString>) -> Result<Command, UsageError> {
    let mut selection = None;
    let mut command_id = None;
    while let Some(option) = values.pop_front() {
        match text(option)?.as_str() {
            "--model" => {
                set_selection(
                    &mut selection,
                    ModelSelection::Direct {
                        selection_id: canonical_uuid(take_text(
                            values,
                            "--model requires a UUID",
                        )?)?,
                    },
                )?;
            }
            "--alias" => {
                set_selection(
                    &mut selection,
                    ModelSelection::Alias {
                        alias_id: canonical_uuid(take_text(values, "--alias requires a UUID")?)?,
                    },
                )?;
            }
            "--command-id" => {
                if command_id.is_some() {
                    return Err(UsageError::new("--command-id may be supplied only once"));
                }
                command_id = Some(command_id_value(take_text(
                    values,
                    "--command-id requires a UUID",
                )?)?);
            }
            _ => return Err(UsageError::new("unknown create option")),
        }
    }
    Ok(Command::Create {
        selection: selection.ok_or(UsageError::new(
            "create requires exactly one of --model or --alias",
        ))?,
        command_id,
    })
}

fn set_selection(
    current: &mut Option<ModelSelection>,
    selection: ModelSelection,
) -> Result<(), UsageError> {
    if current.replace(selection).is_some() {
        Err(UsageError::new(
            "create requires exactly one of --model or --alias",
        ))
    } else {
        Ok(())
    }
}

fn parse_send(values: &mut VecDeque<OsString>) -> Result<Command, UsageError> {
    let session_id = canonical_uuid(take_text(values, "send requires a session UUID")?)?;
    let mut command_id = None;
    let mut defaults_version = None;
    while let Some(option) = values.pop_front() {
        match text(option)?.as_str() {
            "--command-id" => {
                if command_id.is_some() {
                    return Err(UsageError::new("--command-id may be supplied only once"));
                }
                command_id = Some(command_id_value(take_text(
                    values,
                    "--command-id requires a UUID",
                )?)?);
            }
            "--defaults-version" => {
                if defaults_version.is_some() {
                    return Err(UsageError::new(
                        "--defaults-version may be supplied only once",
                    ));
                }
                defaults_version = Some(canonical_u64(take_text(
                    values,
                    "--defaults-version requires a decimal value",
                )?)?);
            }
            _ => return Err(UsageError::new("unknown send option")),
        }
    }
    if command_id.is_some() != defaults_version.is_some() {
        return Err(UsageError::new(
            "--command-id and --defaults-version must be supplied together",
        ));
    }
    Ok(Command::Send {
        session_id,
        command_id,
        defaults_version,
    })
}

fn parse_session_only(
    values: &mut VecDeque<OsString>,
    follow: bool,
) -> Result<Command, UsageError> {
    let session_id = canonical_uuid(take_text(values, "a session UUID is required")?)?;
    require_empty(values)?;
    Ok(if follow {
        Command::Follow { session_id }
    } else {
        Command::Transcript { session_id }
    })
}

fn take_text(values: &mut VecDeque<OsString>, missing: &'static str) -> Result<String, UsageError> {
    text(values.pop_front().ok_or(UsageError::new(missing))?)
}

fn front_text(values: &VecDeque<OsString>) -> Result<Option<&str>, UsageError> {
    values
        .front()
        .map(|value| {
            value
                .to_str()
                .ok_or(UsageError::new("arguments must be valid UTF-8"))
        })
        .transpose()
}

fn text(value: OsString) -> Result<String, UsageError> {
    value
        .into_string()
        .map_err(|_| UsageError::new("arguments must be valid UTF-8"))
}

fn canonical_uuid(value: String) -> Result<CanonicalUuid, UsageError> {
    let parsed = Uuid::parse_str(&value).map_err(|_| UsageError::new("UUID is invalid"))?;
    if parsed.hyphenated().to_string() != value {
        return Err(UsageError::new(
            "UUID must be lowercase canonical hyphenated text",
        ));
    }
    Ok(CanonicalUuid::from_uuid(parsed))
}

fn command_id_value(value: String) -> Result<CommandId, UsageError> {
    CommandId::try_from_uuid(canonical_uuid(value)?.into_uuid())
        .map_err(|_| UsageError::new("command UUID uses a reserved value"))
}

fn canonical_u64(value: String) -> Result<CanonicalU64, UsageError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(UsageError::new(
            "decimal value must use its shortest unsigned spelling",
        ));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| UsageError::new("decimal value exceeds the unsigned 64-bit range"))?;
    Ok(CanonicalU64::new(parsed))
}

fn require_empty(values: &VecDeque<OsString>) -> Result<(), UsageError> {
    if values.is_empty() {
        Ok(())
    } else {
        Err(UsageError::new("unexpected command argument"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, ParseOutcome, parse};

    #[test]
    fn send_recovery_flags_are_an_exact_pair() {
        let session = "00000000-0000-0000-0000-000000000001";
        assert!(parse(["send", session, "--command-id", session].map(Into::into)).is_err());
        assert!(parse(["send", session, "--defaults-version", "1"].map(Into::into)).is_err());
        assert!(matches!(
            parse(
                [
                    "send",
                    session,
                    "--command-id",
                    session,
                    "--defaults-version",
                    "18446744073709551615",
                ]
                .map(Into::into)
            ),
            Ok(ParseOutcome::Run(super::Arguments {
                command: Command::Send { .. },
                ..
            }))
        ));
    }

    #[test]
    fn duplicate_global_options_are_rejected() {
        assert!(parse(["--raw-output", "--raw-output", "list"].map(Into::into)).is_err());
    }

    #[test]
    fn create_requires_exactly_one_model_selection() {
        assert!(
            parse(
                [
                    "create",
                    "--model",
                    "00000000-0000-0000-0000-000000000001",
                    "--alias",
                    "00000000-0000-0000-0000-000000000002",
                ]
                .map(Into::into)
            )
            .is_err()
        );
        assert!(parse(["create"].map(Into::into)).is_err());
    }
}
