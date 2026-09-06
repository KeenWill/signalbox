//! Operator configuration and argv parsing for the reconciliation loop.
use super::failure;
use serde_json::{Map, Value, json};
use signalbox_convergence::Error;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub(super) const HELP: &str = "signalbox-converge reconcile [options]
  --config FILE                  JSON configuration; omitted: CONVERGENCE_RECONCILER_CONFIG
  --repo OWNER/NAME              Required repository
  --policy FILE                  TOML/JSON policy; omitted: crates/convergence/examples/repository.toml
  --head-pattern GLOB            Case-sensitive branch pattern; omitted: agent/*
  --active-command ARGV          Required quoted command or JSON argv array
  --dispatch-command ARGV        Required outside dry-run; quoted command or JSON argv array
  --interval-seconds SECONDS     Finite positive interval; omitted: 300 seconds
  --cool-off-seconds SECONDS     Finite nonnegative cool-off; omitted: 1800 seconds
  --command-timeout-seconds SECONDS  Finite positive command timeout; omitted: 60 seconds
  --state-file FILE              State path; omitted: XDG_STATE_HOME/signalbox/convergence-reconciler.json
  --log-file FILE                Append JSON decisions; omitted: stderr
  --summary text|json|none        Stdout summary; omitted: text
  --dry-run                     Suppress dispatch; omitted: false
  --once                        Run one tick; omitted: repeat until SIGINT
Values use CLI, CONVERGENCE_RECONCILER_<JSON_KEY>, JSON file, then defaults.
Commands receive the PR number and compact JSON state as two appended arguments.";

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
    pub fn load(args: Vec<String>, env: &BTreeMap<String, String>) -> Result<Self, Error> {
        let mut cli = Map::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let name = match arg.as_str() {
                "--repo" => "repository",
                "--policy" => "convergence_policy",
                "--config" => "config",
                "--head-pattern" => "head_pattern",
                "--interval-seconds" => "interval_seconds",
                "--cool-off-seconds" => "cool_off_seconds",
                "--command-timeout-seconds" => "command_timeout_seconds",
                "--state-file" => "state_file",
                "--log-file" => "log_file",
                "--active-command" => "active_command",
                "--dispatch-command" => "dispatch_command",
                "--summary" => "summary",
                "--dry-run" => "dry_run",
                "--once" => "once",
                _ => return Err(failure(format!("unknown option {arg}"))),
            };
            let value = if matches!(name, "dry_run" | "once") {
                json!(true)
            } else {
                json!(
                    args.next()
                        .ok_or_else(|| failure(format!("missing value for {arg}")))?
                )
            };
            cli.insert(name.into(), value);
        }
        let config_path = cli
            .get("config")
            .and_then(Value::as_str)
            .or_else(|| env.get("CONVERGENCE_RECONCILER_CONFIG").map(String::as_str));
        let file = match config_path {
            Some(path) => serde_json::from_slice::<Value>(&std::fs::read(path)?)?
                .as_object()
                .cloned()
                .ok_or_else(|| failure("configuration file must contain a JSON object"))?,
            None => Map::new(),
        };
        Self::from_values(&cli, env, &file)
    }

    fn from_values(
        cli: &Map<String, Value>,
        env: &BTreeMap<String, String>,
        file: &Map<String, Value>,
    ) -> Result<Self, Error> {
        let selected = |name: &str, default: Value| {
            cli.get(name)
                .cloned()
                .or_else(|| {
                    env.get(&format!("CONVERGENCE_RECONCILER_{}", name.to_uppercase()))
                        .map(|v| json!(v))
                })
                .or_else(|| file.get(name).cloned())
                .unwrap_or(default)
        };
        let string = |name: &str, default: Value| -> Result<String, Error> {
            selected(name, default)
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failure(format!("{name} must be a string")))
        };
        let repository = string("repository", Value::Null)?;
        split_repository(&repository)?;
        let dry_run = boolean(&selected("dry_run", json!(false)), "dry_run")?;
        let active = command(&selected("active_command", Value::Null))?;
        let dispatch = command(&selected("dispatch_command", Value::Null))?;
        if active.is_empty() {
            return Err(failure("active_command is required"));
        }
        if !dry_run && dispatch.is_empty() {
            return Err(failure(
                "dispatch_command is required unless dry-run is enabled",
            ));
        }
        let interval = duration(
            &selected("interval_seconds", json!(300)),
            "interval_seconds",
        )?;
        let timeout = duration(
            &selected("command_timeout_seconds", json!(60)),
            "command_timeout_seconds",
        )?;
        let cool_off = number(
            &selected("cool_off_seconds", json!(1800)),
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
        let log_file = selected("log_file", Value::Null);
        let summary = string("summary", json!("text"))?;
        if !matches!(summary.as_str(), "text" | "json" | "none") {
            return Err(failure("summary must be text, json, or none"));
        }
        Ok(Self {
            repository,
            policy: string(
                "convergence_policy",
                json!("crates/convergence/examples/repository.toml"),
            )?
            .into(),
            head_pattern: string("head_pattern", json!("agent/*"))?,
            interval,
            cool_off,
            timeout,
            state_file: string("state_file", json!(default_state))?.into(),
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
            once: boolean(&selected("once", json!(false)), "once")?,
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
