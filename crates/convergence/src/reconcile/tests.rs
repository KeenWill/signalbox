//! Recorded evidence, dispatch fence, configuration, and child-process scenarios.
use super::*;
use std::{
    collections::VecDeque, os::unix::process::ExitStatusExt, path::PathBuf, sync::atomic::AtomicU64,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "convergence-reconcile-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn config(directory: &Directory) -> Config {
    Config {
        repository: "KeenWill/signalbox".into(),
        policy: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/repository.toml"),
        head_pattern: "agent/*".into(),
        interval: Duration::from_secs(300),
        cool_off: 300.0,
        timeout: Duration::from_secs(60),
        state_file: directory.0.join("state.json"),
        log_file: None,
        active: vec!["active".into()],
        dispatch: vec!["dispatch".into()],
        summary: "none".into(),
        dry_run: false,
        once: true,
    }
}
fn fixture(name: &str) -> Recording {
    Recording::read(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name),
    )
    .unwrap()
}
fn red(config: &Config) -> (Value, Evaluation) {
    let policy = ConvergencePolicy::read(&config.policy).unwrap();
    let snapshot = fixture("mutations/check-red.json")
        .snapshot(&policy)
        .unwrap();
    let evaluated = evaluate(&snapshot, &policy).unwrap();
    assert!(!evaluated.converged);
    (snapshot.current, evaluated)
}
fn empty(config: &Config) -> State {
    State {
        repository: config.repository.clone(),
        pull_requests: BTreeMap::new(),
    }
}
fn exit(code: i32) -> Result<Output, CommandError> {
    Ok(Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

#[derive(Default)]
struct RecordedRuntime {
    responses: VecDeque<Value>,
    comparisons: BTreeMap<String, Value>,
    commands: VecDeque<Result<Output, CommandError>>,
    called: Vec<Vec<String>>,
    times: VecDeque<f64>,
    fence: Option<PathBuf>,
}
impl Runtime for RecordedRuntime {
    fn github(&mut self, request: GitHubRequest, _: Duration) -> Result<Value, Error> {
        match request {
            GitHubRequest::GraphQl { .. } => self
                .responses
                .pop_front()
                .ok_or_else(|| failure("unexpected GitHub request")),
            GitHubRequest::Rest { path } => self
                .comparisons
                .get(path.split("/compare/").nth(1).unwrap_or_default())
                .cloned()
                .ok_or_else(|| failure(format!("unrecorded comparison {path}"))),
        }
    }
    fn command(&mut self, argv: &[String], _: Duration) -> Result<Output, CommandError> {
        self.called.push(argv.to_vec());
        if argv[0] == "dispatch"
            && let Some(path) = &self.fence
        {
            let saved = load_state(path, "KeenWill/signalbox").unwrap();
            let payload: Value = serde_json::from_str(argv.last().unwrap()).unwrap();
            assert_eq!(
                saved.pull_requests[&argv[1]].last_dispatched_at,
                payload["last_dispatched_at"].as_f64()
            );
            assert!(saved.pull_requests[&argv[1]].last_dispatched_at.is_some());
        }
        self.commands
            .pop_front()
            .expect("unexpected operator command")
    }
    fn now(&mut self) -> f64 {
        self.times.pop_front().unwrap_or(1000.0)
    }
}
fn listing_response(nodes: Vec<Value>, tracked: Vec<Value>) -> Value {
    json!({"data":{"repository":{"pullRequests":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}},"tracked":tracked}})
}
fn retained() -> Record {
    Record {
        head_oid: "current-head".into(),
        last_dispatched_head: Some("current-head".into()),
        last_dispatched_at: Some(900.0),
        ..Record::default()
    }
}

#[test]
fn converged_pull_request_is_merge_ready() {
    let dir = Directory::new();
    let cfg = config(&dir);
    assert_eq!(
        choose(&cfg, &Record::default(), true, 1000.0, None),
        decision("merge-ready", "convergence-predicate-satisfied")
    );
}
#[test]
fn recent_dispatch_is_in_cool_off() {
    let dir = Directory::new();
    let cfg = config(&dir);
    assert_eq!(
        choose(&cfg, &retained(), false, 1000.0, None),
        decision("cooling-off", "dispatch-cool-off:200s-remaining")
    );
}
#[test]
fn active_work_prevents_dispatch() {
    let dir = Directory::new();
    let cfg = config(&dir);
    assert_eq!(
        choose(&cfg, &Record::default(), false, 1000.0, Some(true)),
        decision("already-active", "operator-command-reported-active-work")
    );
}
#[test]
fn dry_run_reports_the_dispatch_it_would_make() {
    let dir = Directory::new();
    let cfg = Config {
        dry_run: true,
        ..config(&dir)
    };
    assert_eq!(
        choose(&cfg, &Record::default(), false, 1000.0, Some(false)),
        decision("would-dispatch", "dry-run-and-no-active-work")
    );
}
#[test]
fn inactive_work_dispatches_outside_cool_off() {
    let dir = Directory::new();
    let cfg = config(&dir);
    assert_eq!(
        choose(&cfg, &retained(), false, 1300.0, Some(false)),
        decision("dispatch", "no-active-work-and-outside-cool-off")
    );
}
#[test]
fn missing_dispatch_command_skips_mutation() {
    let dir = Directory::new();
    let cfg = Config {
        dispatch: Vec::new(),
        ..config(&dir)
    };
    assert_eq!(
        choose(&cfg, &Record::default(), false, 1000.0, Some(false)),
        decision("skipped", "dispatch-command-not-configured")
    );
}

fn environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".into(), "/unused".into()),
        (
            "CONVERGENCE_RECONCILER_REPOSITORY".into(),
            "KeenWill/signalbox".into(),
        ),
        (
            "CONVERGENCE_RECONCILER_ACTIVE_COMMAND".into(),
            "active".into(),
        ),
        ("CONVERGENCE_RECONCILER_DRY_RUN".into(), "true".into()),
    ])
}
#[test]
fn non_path_configuration_is_rejected() {
    let dir = Directory::new();
    let path = dir.0.join("config.json");
    for field in ["state_file", "log_file", "convergence_policy"] {
        fs::write(&path, serde_json::to_vec(&json!({field:42})).unwrap()).unwrap();
        assert!(
            Config::load(
                vec!["--config".into(), path.to_string_lossy().into()],
                &environment()
            )
            .is_err(),
            "{field}"
        );
    }
}
#[test]
fn non_string_repository_is_rejected_as_malformed() {
    let dir = Directory::new();
    let path = dir.0.join("state.json");
    fs::write(&path, r#"{"repository":42,"pull_requests":{}}"#).unwrap();
    assert!(load_state(&path, "KeenWill/signalbox").is_err());
}
#[test]
fn non_object_pull_request_record_is_rejected() {
    let dir = Directory::new();
    let path = dir.0.join("state.json");
    fs::write(
        &path,
        r#"{"repository":"KeenWill/signalbox","pull_requests":{"1":[]}}"#,
    )
    .unwrap();
    assert!(load_state(&path, "KeenWill/signalbox").is_err());
}
#[test]
fn non_finite_positive_number_is_rejected() {
    for value in ["NaN", "inf", "-inf"] {
        assert!(
            config::number(&json!(value), "interval_seconds", false).is_err(),
            "{value}"
        );
    }
}
#[test]
fn non_finite_nonnegative_number_is_rejected() {
    for value in ["NaN", "inf", "-inf"] {
        assert!(
            config::number(&json!(value), "cool_off_seconds", true).is_err(),
            "{value}"
        );
    }
}
#[test]
fn non_numeric_configuration_is_rejected() {
    for value in [json!(null), json!({}), json!([]), json!("not-a-number")] {
        assert!(
            config::number(&value, "interval_seconds", false).is_err(),
            "{value}"
        );
    }
}
#[test]
fn non_object_state_is_rejected_as_malformed() {
    let dir = Directory::new();
    let path = dir.0.join("state.json");
    fs::write(&path, "[]").unwrap();
    assert!(load_state(&path, "KeenWill/signalbox").is_err());
}

#[test]
fn dispatch_fence_uses_time_immediately_before_dispatch() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let (node, evaluated) = red(&cfg);
    let mut state = empty(&cfg);
    let mut runtime = RecordedRuntime {
        commands: VecDeque::from([exit(1), exit(0)]),
        times: VecDeque::from([1000.0, 1300.0]),
        fence: Some(cfg.state_file.clone()),
        ..RecordedRuntime::default()
    };
    let result = process_candidate(
        &cfg,
        &mut state,
        &node,
        &evaluated,
        &mut runtime,
        &mut Vec::new(),
    )
    .unwrap();
    let record = &state.pull_requests[&node["number"].to_string()];
    assert_eq!(result["decision"], "dispatched");
    assert_eq!(record.last_dispatched_at, Some(1300.0));
    assert_eq!(
        record.last_dispatched_head.as_deref(),
        Some(evaluated.facts.head_oid.as_str())
    );
    assert_eq!(result["idle_for_seconds"], 300.0);
    assert_eq!(record.idle_since, None);
}
#[test]
fn state_save_replaces_the_file_and_syncs_persistent_state() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let mut state = empty(&cfg);
    save_state(&cfg.state_file, &state).unwrap();
    state.pull_requests.insert("1".into(), retained());
    save_state(&cfg.state_file, &state).unwrap();
    let loaded = load_state(&cfg.state_file, &cfg.repository).unwrap();
    assert_eq!(loaded.pull_requests["1"].last_dispatched_at, Some(900.0));
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    assert!(load_state(&cfg.state_file, "another/repository").is_err());
}
#[test]
fn nonzero_dispatch_exit_retains_cool_off_fence() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let (node, evaluated) = red(&cfg);
    let mut state = empty(&cfg);
    let mut runtime = RecordedRuntime {
        commands: VecDeque::from([exit(1), exit(9)]),
        fence: Some(cfg.state_file.clone()),
        ..RecordedRuntime::default()
    };
    let result = process_candidate(
        &cfg,
        &mut state,
        &node,
        &evaluated,
        &mut runtime,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(
        result["reason"],
        "dispatch-command-exited:9-cool-off-retained"
    );
    let saved = load_state(&cfg.state_file, &cfg.repository).unwrap();
    assert_eq!(
        saved.pull_requests[&node["number"].to_string()].last_dispatched_at,
        Some(1000.0)
    );
    let result = process_candidate(
        &cfg,
        &mut state,
        &node,
        &evaluated,
        &mut runtime,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(result["decision"], "cooling-off");
    assert_eq!(runtime.called.len(), 2);
}
#[test]
fn listing_does_not_evaluate_a_matching_branch_from_another_repository() {
    let dir = Directory::new();
    let cfg = Config {
        repository: "OTHER/REPOSITORY".into(),
        head_pattern: "*".into(),
        ..config(&dir)
    };
    let (node, _) = red(&cfg);
    let mut runtime = RecordedRuntime {
        responses: VecDeque::from([listing_response(vec![node], vec![])]),
        ..RecordedRuntime::default()
    };
    let (candidates, tracked) = listing(&cfg, &empty(&cfg), &mut runtime).unwrap();
    assert!(candidates.is_empty());
    assert!(tracked.is_empty());
    assert!(runtime.responses.is_empty());
}
#[test]
fn listing_matches_recorded_repository_identity_without_case_sensitivity() {
    let dir = Directory::new();
    let cfg = Config {
        repository: "keenwill/SIGNALBOX".into(),
        head_pattern: "*".into(),
        ..config(&dir)
    };
    let (node, _) = red(&cfg);
    let mut runtime = RecordedRuntime {
        responses: VecDeque::from([listing_response(vec![node.clone()], vec![])]),
        ..RecordedRuntime::default()
    };
    let (candidates, tracked) = listing(&cfg, &empty(&cfg), &mut runtime).unwrap();
    assert_eq!(candidates, vec![node]);
    assert!(tracked.is_empty());
}
#[test]
fn terminal_recording_is_returned_for_bookkeeping_without_evaluation() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let policy = ConvergencePolicy::read(&cfg.policy).unwrap();
    let node = fixture("pr-1582.json.gz")
        .snapshot(&policy)
        .unwrap()
        .current;
    let mut state = empty(&cfg);
    state.pull_requests.insert(
        node["number"].to_string(),
        Record {
            node_id: string(&node["id"]).into(),
            unconverged_since: Some(800.0),
            idle_since: Some(900.0),
            ..Record::default()
        },
    );
    save_state(&cfg.state_file, &state).unwrap();
    let mut runtime = RecordedRuntime {
        responses: VecDeque::from([listing_response(vec![], vec![node.clone()])]),
        ..RecordedRuntime::default()
    };
    let values = tick(&cfg, &policy, &mut runtime, &mut Vec::new()).unwrap();
    assert_eq!(values[0]["decision"], "skipped");
    assert_eq!(values[0]["idle_for_seconds"], 100.0);
    assert!(runtime.responses.is_empty());
    let state = load_state(&cfg.state_file, &cfg.repository).unwrap();
    let record = &state.pull_requests[&node["number"].to_string()];
    assert!(record.terminal_state.is_some());
    assert_eq!(record.unconverged_since, None);
    assert_eq!(record.idle_since, None);
}
#[test]
fn evaluation_receives_previous_state_and_returns_its_evidence() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let policy = ConvergencePolicy::read(&cfg.policy).unwrap();
    let recording = fixture("mutations/settled.json");
    let mut runtime = RecordedRuntime {
        responses: recording
            .observations
            .iter()
            .flatten()
            .map(|r| r.response.clone())
            .collect(),
        comparisons: recording.comparisons.clone(),
        ..RecordedRuntime::default()
    };
    let expected = evaluate(&recording.snapshot(&policy).unwrap(), &policy).unwrap();
    let (node, result) = evaluation(
        &cfg,
        &policy,
        recording.number,
        recording.previous.clone(),
        &mut runtime,
    )
    .unwrap();
    assert_eq!(node["number"], recording.number);
    assert_eq!(result.verdict, expected.verdict);
    assert_eq!(
        result.state["check_inventory"],
        expected.state["check_inventory"]
    );
    assert!(
        result.facts.check_inventory_stable,
        "the prior inventory was passed in process"
    );
    assert!(
        runtime.responses.is_empty(),
        "final identity responses must be consumed"
    );
}
#[test]
fn evaluation_command_timeout_terminates_child_and_descendant() {
    let dir = Directory::new();
    let pid_file = dir.0.join("child.pid");
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "sleep 600 & echo $! > \"$1\"; wait".into(),
        "test".into(),
        pid_file.to_string_lossy().into(),
    ];
    let started = Instant::now();
    let result = process::execute(&argv, None, Duration::from_secs(1), &AtomicBool::new(false));
    assert!(matches!(result, Err(CommandError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = fs::read_to_string(pid_file).unwrap();
    let status = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(
        status.trim().is_empty() || status.trim().starts_with('Z'),
        "{status}"
    );
}
#[test]
fn graphql_subprocess_timeout_uses_tick_failure_path() {
    struct Timeout;
    impl Runtime for Timeout {
        fn github(&mut self, _: GitHubRequest, _: Duration) -> Result<Value, Error> {
            Err(failure("gh: command timed out"))
        }
        fn command(&mut self, _: &[String], _: Duration) -> Result<Output, CommandError> {
            panic!("failed listing must not dispatch")
        }
        fn now(&mut self) -> f64 {
            1000.0
        }
    }
    let dir = Directory::new();
    let cfg = config(&dir);
    let policy = ConvergencePolicy::read(&cfg.policy).unwrap();
    assert!(
        tick(&cfg, &policy, &mut Timeout, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert!(!cfg.state_file.exists());
}

#[test]
fn ambiguous_timeout_retains_fence_but_definite_start_failure_removes_it() {
    for (outcome, retained) in [
        (CommandError::Timeout, true),
        (CommandError::Interrupted, true),
        (
            CommandError::Start(std::io::Error::from_raw_os_error(2)),
            false,
        ),
    ] {
        let dir = Directory::new();
        let cfg = config(&dir);
        let (node, evaluated) = red(&cfg);
        let mut state = empty(&cfg);
        let mut runtime = RecordedRuntime {
            commands: VecDeque::from([exit(1), Err(outcome)]),
            fence: Some(cfg.state_file.clone()),
            ..RecordedRuntime::default()
        };
        let result = process_candidate(
            &cfg,
            &mut state,
            &node,
            &evaluated,
            &mut runtime,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(result["decision"], "skipped");
        assert_eq!(
            load_state(&cfg.state_file, &cfg.repository)
                .unwrap()
                .pull_requests[&node["number"].to_string()]
                .last_dispatched_at
                .is_some(),
            retained
        );
    }
}

#[test]
fn interruption_before_dispatch_start_clears_the_persisted_fence() {
    struct InterruptedDispatch {
        state_file: PathBuf,
    }
    impl Runtime for InterruptedDispatch {
        fn github(&mut self, _: GitHubRequest, _: Duration) -> Result<Value, Error> {
            panic!("this scenario uses recorded evidence")
        }
        fn command(&mut self, argv: &[String], timeout: Duration) -> Result<Output, CommandError> {
            if argv[0] == "active" {
                return exit(1);
            }
            let saved = load_state(&self.state_file, "KeenWill/signalbox").unwrap();
            let number = &argv[argv.len() - 2];
            assert_eq!(saved.pull_requests[number].last_dispatched_at, Some(1000.0));
            let result = process::execute(argv, None, timeout, &AtomicBool::new(true));
            assert!(
                matches!(&result, Err(CommandError::Start(error)) if error.kind() == std::io::ErrorKind::Interrupted)
            );
            result
        }
        fn now(&mut self) -> f64 {
            1000.0
        }
    }

    let dir = Directory::new();
    let marker = dir.0.join("dispatch-started");
    let cfg = Config {
        dispatch: vec![
            "sh".into(),
            "-c".into(),
            "printf started > \"$0\"".into(),
            marker.to_string_lossy().into(),
        ],
        ..config(&dir)
    };
    let (node, evaluated) = red(&cfg);
    let mut state = empty(&cfg);
    let mut runtime = InterruptedDispatch {
        state_file: cfg.state_file.clone(),
    };
    let result = process_candidate(
        &cfg,
        &mut state,
        &node,
        &evaluated,
        &mut runtime,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(result["decision"], "skipped");
    assert!(!marker.exists(), "interruption must prevent child startup");
    let restarted = load_state(&cfg.state_file, &cfg.repository).unwrap();
    let number = node["number"].to_string();
    assert_eq!(state.pull_requests[&number].last_dispatched_at, None);
    assert_eq!(restarted.pull_requests[&number].last_dispatched_at, None);
    assert_eq!(restarted.pull_requests[&number].last_dispatched_head, None);
    assert_eq!(
        choose(&cfg, &restarted.pull_requests[&number], false, 1000.0, None).name,
        "active-check-required"
    );
}
#[test]
fn dry_run_payload_and_log_record_observed_idle_time() {
    let dir = Directory::new();
    let cfg = Config {
        dry_run: true,
        ..config(&dir)
    };
    let (node, evaluated) = red(&cfg);
    let mut state = empty(&cfg);
    let mut logs = Vec::new();
    let mut runtime = RecordedRuntime {
        commands: VecDeque::from([exit(1), exit(0)]),
        times: VecDeque::from([1000.0, 1300.0]),
        ..RecordedRuntime::default()
    };
    assert_eq!(
        process_candidate(&cfg, &mut state, &node, &evaluated, &mut runtime, &mut logs).unwrap()["decision"],
        "would-dispatch"
    );
    let result =
        process_candidate(&cfg, &mut state, &node, &evaluated, &mut runtime, &mut logs).unwrap();
    assert_eq!(result["decision"], "already-active");
    assert_eq!(result["idle_for_seconds"], 300.0);
    assert_eq!(
        state.pull_requests[&node["number"].to_string()].idle_since,
        None
    );
    let payload: Value = serde_json::from_str(runtime.called[1].last().unwrap()).unwrap();
    assert_eq!(payload["head_oid"], evaluated.facts.head_oid);
    assert_eq!(payload["idle_since"], 1000.0);
    assert_eq!(String::from_utf8(logs).unwrap().lines().count(), 2);
}
#[test]
fn configuration_precedence_and_quoted_argv_preserve_arguments() {
    let dir = Directory::new();
    let path = dir.0.join("config.json");
    fs::write(
        &path,
        r#"{"head_pattern":"file/*","active_command":["file-active"]}"#,
    )
    .unwrap();
    let mut env = environment();
    env.insert("CONVERGENCE_RECONCILER_HEAD_PATTERN".into(), "env/*".into());
    env.insert("XDG_STATE_HOME".into(), dir.0.to_string_lossy().into());
    let cfg = Config::load(
        vec![
            "--config".into(),
            path.to_string_lossy().into(),
            "--head-pattern".into(),
            "cli/*".into(),
            "--active-command".into(),
            "program 'two words' \"\" escaped\\ space".into(),
        ],
        &env,
    )
    .unwrap();
    assert_eq!(cfg.head_pattern, "cli/*");
    assert_eq!(cfg.active, ["program", "two words", "", "escaped space"]);
    assert_eq!(
        cfg.state_file,
        dir.0.join("signalbox/convergence-reconciler.json")
    );
}
#[test]
fn head_patterns_match_case_sensitively_with_shell_globs() {
    for (pattern, value, expected) in [
        ("agent/*", "agent/a/b", true),
        ("agent/?", "agent/ab", false),
        ("agent/[ab]", "agent/a", true),
        ("agent/[!a]", "agent/b", true),
        ("agent/*", "Agent/a", false),
        ("agent/[", "agent/[", true),
    ] {
        assert_eq!(
            matches_head(pattern, value).unwrap(),
            expected,
            "{pattern}: {value}"
        );
    }
}

#[test]
fn explicit_state_path_does_not_require_a_home_environment() {
    let dir = Directory::new();
    let mut env = environment();
    env.remove("HOME");
    let path = dir.0.join("state.json");
    let cfg = Config::load(
        vec!["--state-file".into(), path.to_string_lossy().into()],
        &env,
    )
    .unwrap();
    assert_eq!(cfg.state_file, path);
}
#[test]
fn summaries_support_text_json_and_none() {
    let values = vec![
        json!({"pull_request":1,"decision":"would-dispatch","reason":"dry-run-and-no-active-work","idle_for_seconds":null}),
    ];
    let mut output = Vec::new();
    summary(&values, "json", &mut output).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap(),
        json!(values)
    );
    output.clear();
    summary(&values, "text", &mut output).unwrap();
    assert!(
        String::from_utf8(output.clone())
            .unwrap()
            .contains("would-dispatch")
    );
    output.clear();
    summary(&values, "none", &mut output).unwrap();
    assert!(output.is_empty());
}

#[test]
fn later_candidate_failure_cannot_erase_a_persisted_dispatch_fence() {
    let dir = Directory::new();
    let cfg = Config {
        head_pattern: "*".into(),
        ..config(&dir)
    };
    let policy = ConvergencePolicy::read(&cfg.policy).unwrap();
    let recording = fixture("mutations/check-red.json");
    let node = recording.snapshot(&policy).unwrap().current;
    let mut next = node.clone();
    next["number"] = json!(recording.number + 1);
    let mut responses = VecDeque::from([listing_response(vec![node.clone(), next], vec![])]);
    responses.extend(
        recording
            .observations
            .iter()
            .flatten()
            .map(|response| response.response.clone()),
    );
    let mut runtime = RecordedRuntime {
        responses,
        comparisons: recording.comparisons.clone(),
        commands: VecDeque::from([exit(1), exit(0)]),
        fence: Some(cfg.state_file.clone()),
        ..RecordedRuntime::default()
    };
    let mut logs = Vec::new();
    let error = tick(&cfg, &policy, &mut runtime, &mut logs).unwrap_err();
    assert!(error.to_string().contains("unexpected GitHub request"));
    let state = load_state(&cfg.state_file, &cfg.repository).unwrap();
    assert_eq!(
        state.pull_requests[&recording.number.to_string()].last_dispatched_at,
        Some(1000.0)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&logs).unwrap()["decision"],
        "dispatched"
    );
    assert_eq!(runtime.called.len(), 2);
}

#[test]
fn new_head_and_convergence_release_the_dispatch_cool_off() {
    let dir = Directory::new();
    let cfg = config(&dir);
    let advanced = Record {
        head_oid: "new-head".into(),
        ..retained()
    };
    assert_eq!(
        choose(&cfg, &advanced, false, 1000.0, None).name,
        "active-check-required"
    );
    let policy = ConvergencePolicy::read(&cfg.policy).unwrap();
    let snapshot = fixture("mutations/settled.json").snapshot(&policy).unwrap();
    let evaluated = evaluate(&snapshot, &policy).unwrap();
    assert!(evaluated.converged);
    let number = snapshot.current["number"].to_string();
    let mut state = empty(&cfg);
    state.pull_requests.insert(
        number.clone(),
        Record {
            unconverged_since: Some(800.0),
            idle_since: Some(900.0),
            ..retained()
        },
    );
    let result = process_candidate(
        &cfg,
        &mut state,
        &snapshot.current,
        &evaluated,
        &mut RecordedRuntime::default(),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(result["decision"], "merge-ready");
    assert_eq!(result["idle_for_seconds"], 100.0);
    let record = &state.pull_requests[&number];
    assert_eq!(record.last_dispatched_at, None);
    assert_eq!(record.last_dispatched_head, None);
    assert_eq!(record.unconverged_since, None);
    assert_eq!(record.idle_since, None);
}
