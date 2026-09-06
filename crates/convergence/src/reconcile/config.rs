//! Operator configuration and argv parsing for the reconciliation loop.
use super::failure;
use clap::Args;
use serde_json::{Map, Value, json};
use signalbox_convergence::Error;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[derive(Debug, Args)]
#[command(
    after_help = "Values use CLI, CONVERGENCE_RECONCILER_<JSON_KEY>, JSON file, then defaults.\nCommands receive the PR number and compact JSON state as two appended arguments."
)]
pub(crate) struct ReconcileArgs {
    /// JSON configuration file; omitted: use CONVERGENCE_RECONCILER_CONFIG.
    #[arg(long, value_name = "FILE")]
    config: Option<String>,
    /// Required repository; omitted: use environment or JSON configuration.
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    repository: Option<String>,
    /// Required TOML/JSON policy; omitted: use environment or JSON configuration.
    #[arg(long = "policy", value_name = "FILE")]
    convergence_policy: Option<String>,
    /// Case-sensitive branch pattern; default: agent/*.
    #[arg(long, value_name = "GLOB")]
    head_pattern: Option<String>,
    /// Required quoted command or JSON argv array; omitted: use environment or JSON configuration.
    #[arg(long, value_name = "ARGV")]
    active_command: Option<String>,
    /// Quoted command or JSON argv array; required outside dry-run.
    #[arg(long, value_name = "ARGV")]
    dispatch_command: Option<String>,
    /// Finite positive interval in seconds; default: 300.
    #[arg(long, value_name = "SECONDS", allow_negative_numbers = true)]
    interval_seconds: Option<String>,
    /// Finite nonnegative cool-off in seconds; default: 1800.
    #[arg(long, value_name = "SECONDS", allow_negative_numbers = true)]
    cool_off_seconds: Option<String>,
    /// Finite positive command timeout in seconds; default: 60.
    #[arg(long, value_name = "SECONDS", allow_negative_numbers = true)]
    command_timeout_seconds: Option<String>,
    /// State path; default: XDG_STATE_HOME or HOME/.local/state, plus signalbox/convergence-reconciler.json; required when both variables are unset.
    #[arg(long, value_name = "FILE")]
    state_file: Option<String>,
    /// Append JSON decisions to a file; default: stderr.
    #[arg(long, value_name = "FILE")]
    log_file: Option<String>,
    /// Stdout summary format; default: text.
    #[arg(long, value_name = "FORMAT", value_parser = ["text", "json", "none"])]
    summary: Option<String>,
    /// Suppress dispatch; default: false.
    #[arg(long)]
    dry_run: bool,
    /// Run one tick; default: repeat until SIGINT.
    #[arg(long)]
    once: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Config {
    pub repository: String,
    pub policy: PathBuf,
    pub head_pattern: String,
    pub interval: Duration,
    pub cool_off: f64,
    pub timeout: Duration,
    pub state_file: PathBuf,
    pub log_file: Option<PathBuf>,
    pub active: Vec<String>,
    pub dispatch: Vec<String>,
    pub summary: String,
    pub dry_run: bool,
    pub once: bool,
}

impl Config {
    pub fn load(args: &ReconcileArgs, env: &BTreeMap<String, String>) -> Result<Self, Error> {
        let config_path = args
            .config
            .as_deref()
            .or_else(|| env.get("CONVERGENCE_RECONCILER_CONFIG").map(String::as_str));
        let file = match config_path {
            Some(path) => serde_json::from_slice::<Value>(&std::fs::read(path)?)?
                .as_object()
                .cloned()
                .ok_or_else(|| failure("configuration file must contain a JSON object"))?,
            None => Map::new(),
        };
        Self::from_values(args, env, &file)
    }

    fn from_values(
        args: &ReconcileArgs,
        env: &BTreeMap<String, String>,
        file: &Map<String, Value>,
    ) -> Result<Self, Error> {
        let selected = |cli: Option<&str>, name: &str, default: Value| {
            cli.map(|value| json!(value))
                .or_else(|| {
                    env.get(&format!("CONVERGENCE_RECONCILER_{}", name.to_uppercase()))
                        .map(|v| json!(v))
                })
                .or_else(|| file.get(name).cloned())
                .unwrap_or(default)
        };
        let string = |cli: Option<&str>, name: &str, default: Value| -> Result<String, Error> {
            selected(cli, name, default)
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failure(format!("{name} must be a string")))
        };
        let repository = string(args.repository.as_deref(), "repository", Value::Null)?;
        split_repository(&repository)?;
        let policy = selected(
            args.convergence_policy.as_deref(),
            "convergence_policy",
            Value::Null,
        );
        let policy = policy.as_str().ok_or_else(|| failure("convergence_policy requires a path via --policy, environment, or JSON configuration"))?;
        let dry_run = boolean(
            &selected(args.dry_run.then_some("true"), "dry_run", json!(false)),
            "dry_run",
        )?;
        let active = command(&selected(
            args.active_command.as_deref(),
            "active_command",
            Value::Null,
        ))?;
        let dispatch = command(&selected(
            args.dispatch_command.as_deref(),
            "dispatch_command",
            Value::Null,
        ))?;
        if active.is_empty() {
            return Err(failure("active_command is required"));
        }
        if !dry_run && dispatch.is_empty() {
            return Err(failure(
                "dispatch_command is required unless dry-run is enabled",
            ));
        }
        let interval = duration(
            &selected(
                args.interval_seconds.as_deref(),
                "interval_seconds",
                json!(300),
            ),
            "interval_seconds",
        )?;
        let timeout = duration(
            &selected(
                args.command_timeout_seconds.as_deref(),
                "command_timeout_seconds",
                json!(60),
            ),
            "command_timeout_seconds",
        )?;
        let cool_off = number(
            &selected(
                args.cool_off_seconds.as_deref(),
                "cool_off_seconds",
                json!(1800),
            ),
            "cool_off_seconds",
            true,
        )?;
        let state_root = env
            .get("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env.get("HOME")
                    .map(|v| PathBuf::from(v).join(".local/state"))
            });
        let default_state =
            state_root.map(|root| root.join("signalbox/convergence-reconciler.json"));
        let state_file = selected(
            args.state_file.as_deref(),
            "state_file",
            json!(default_state),
        );
        let state_file = state_file.as_str().ok_or_else(|| failure("state_file must be a path; --state-file is required when XDG_STATE_HOME and HOME are unset"))?;
        let log_file = selected(args.log_file.as_deref(), "log_file", Value::Null);
        let summary = string(args.summary.as_deref(), "summary", json!("text"))?;
        if !matches!(summary.as_str(), "text" | "json" | "none") {
            return Err(failure("summary must be text, json, or none"));
        }
        Ok(Self {
            repository,
            policy: policy.into(),
            head_pattern: string(
                args.head_pattern.as_deref(),
                "head_pattern",
                json!("agent/*"),
            )?,
            interval,
            cool_off,
            timeout,
            state_file: state_file.into(),
            log_file: if log_file.is_null() {
                None
            } else {
                Some(
                    log_file
                        .as_str()
                        .ok_or_else(|| failure("log_file must be a path"))?
                        .into(),
                )
            },
            active,
            dispatch,
            summary,
            dry_run,
            once: boolean(
                &selected(args.once.then_some("true"), "once", json!(false)),
                "once",
            )?,
        })
    }
}

pub(super) fn split_repository(value: &str) -> Result<(&str, &str), Error> {
    match value.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(failure("repository must have the form OWNER/NAME")),
    }
}

fn boolean(value: &Value, name: &str) -> Result<bool, Error> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::String(value) => match value.to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(failure(format!("{name} must be a boolean"))),
        },
        _ => Err(failure(format!("{name} must be a boolean"))),
    }
}

pub(super) fn number(value: &Value, name: &str, zero: bool) -> Result<f64, Error> {
    let value = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()));
    match value {
        Some(value) if value.is_finite() && (value > 0.0 || (zero && value == 0.0)) => Ok(value),
        _ => Err(failure(format!(
            "{name} must be a finite {} number",
            if zero { "nonnegative" } else { "positive" }
        ))),
    }
}

fn duration(value: &Value, name: &str) -> Result<Duration, Error> {
    Duration::try_from_secs_f64(number(value, name, false)?)
        .map_err(|error| failure(format!("{name}: {error}")))
}

pub(super) fn command(value: &Value) -> Result<Vec<String>, Error> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if value.is_array() {
        return Ok(serde_json::from_value(value.clone())?);
    }
    let text = value
        .as_str()
        .ok_or_else(|| failure("command must be a quoted string or argv array"))?;
    if text.trim_start().starts_with('[') {
        return Ok(serde_json::from_str(text)?);
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None | Some('"'), '\\') => {
                let next = chars
                    .next()
                    .ok_or_else(|| failure("command ends with an escape"))?;
                if quote == Some('"') && !matches!(next, '"' | '\\') {
                    word.push('\\');
                }
                word.push(next);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(failure("command has an unterminated quote"));
    }
    if started {
        words.push(word);
    }
    Ok(words)
}
