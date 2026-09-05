use proptest::{prelude::*, test_runner::TestRunner};
use serde_json::{Value, json};
use signalbox_convergence::{
    ConvergencePolicy, Recording, evaluate, evaluate_facts, fetch::complete_connection,
};
use std::{collections::BTreeMap, error::Error, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn policy() -> Result<ConvergencePolicy, signalbox_convergence::Error> {
    ConvergencePolicy::read(&root().join("examples/repository.toml"))
}

#[test]
fn recorded_corpus_matches_frozen_python_verdicts() -> Result<(), Box<dyn Error>> {
    let expected: BTreeMap<String, Value> =
        serde_json::from_slice(&std::fs::read(root().join("fixtures/expected.json"))?)?;
    let policy = policy()?;
    assert!(
        expected
            .keys()
            .filter(|name| name.starts_with("pr-"))
            .count()
            >= 30,
        "the differential corpus must cover at least thirty real pull requests"
    );
    for (name, expected) in expected {
        let result = Recording::read(&root().join("fixtures").join(&name))
            .and_then(|recording| recording.snapshot(&policy))
            .and_then(|snapshot| evaluate(&snapshot, &policy));
        if expected.get("error").is_some() {
            assert!(
                result.is_err(),
                "{name}: incomplete or changed evidence must not produce a verdict"
            );
        } else {
            let result = result?;
            assert_eq!(json!(result.converged), expected["converged"], "{name}");
            let mut actual_reasons = result.reasons;
            let mut expected_reasons: Vec<String> =
                serde_json::from_value(expected["reasons"].clone())?;
            actual_reasons.sort();
            expected_reasons.sort();
            assert_eq!(actual_reasons, expected_reasons, "{name}");
        }
    }
    Ok(())
}

#[test]
fn pagination_completeness_rejects_every_missing_suffix() -> Result<(), Box<dyn Error>> {
    TestRunner::default().run(&(1usize..400, any::<usize>()), |(total,seed)| {
        let missing = seed % total + 1;
        let complete = json!({"totalCount":total,"nodes":(0..total).collect::<Vec<_>>(),"pageInfo":{"hasNextPage":false,"endCursor":null}});
        prop_assert!(complete_connection(&complete).is_ok());
        let mut partial = complete;
        partial["nodes"] = json!((0..total-missing).collect::<Vec<_>>());
        prop_assert!(complete_connection(&partial).is_err(), "a missing suffix must not authenticate a complete census");
        Ok(())
    })?;
    Ok(())
}

#[test]
fn checks_for_any_other_head_cannot_converge() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let evaluation = evaluate(&recording.snapshot(&policy)?, &policy)?;
    assert!(
        evaluation.converged,
        "the exact-head fixture must otherwise converge"
    );
    TestRunner::default().run(&"[0-9a-f]{40}", |head| {
        prop_assume!(head != evaluation.facts.head_oid);
        let mut facts = evaluation.facts.clone();
        facts.checked_head_oid = Some(head);
        prop_assert!(
            !evaluate_facts(&facts, &policy).is_converged(),
            "checks from a different head cannot authorize convergence"
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn advancing_head_during_pagination_invalidates_the_snapshot() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let baseline = recording.snapshot(&policy)?;
    TestRunner::default().run(&"[0-9a-f]{40}", |head| {
        prop_assume!(Some(head.as_str()) != baseline.initial["headRefOid"].as_str());
        let mut snapshot = baseline.clone();
        snapshot.current["headRefOid"] = json!(head);
        prop_assert!(
            evaluate(&snapshot, &policy).is_err(),
            "a moved head invalidates the evidence before a verdict is issued"
        );
        Ok(())
    })?;
    Ok(())
}

#[test]
fn refreshed_checks_determine_both_verdict_and_projection() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    for (fixture, green) in [
        ("check-green-to-red", false),
        ("check-red-to-green", true),
        ("no-gating-checks", false),
        ("only-exempt-checks", false),
    ] {
        let recording =
            Recording::read(&root().join(format!("fixtures/mutations/{fixture}.json")))?;
        let result = evaluate(&recording.snapshot(&policy)?, &policy)?;
        assert_eq!(result.checks_green, green, "{fixture}");
        if !green {
            assert!(!result.converged, "{fixture}");
        }
    }
    Ok(())
}

#[test]
fn state_file_settles_inventory_across_cli_evaluations() -> Result<(), Box<dyn Error>> {
    let directory = std::env::temp_dir().join(format!("convergence-state-{}", std::process::id()));
    std::fs::create_dir(&directory)?;
    let state = directory.join("state.json");
    for exit in [1, 0] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_signalbox-converge"))
            .arg("evaluate")
            .arg("--fixture")
            .arg(root().join("fixtures/mutations/inventory-unsettled.json"))
            .arg("--policy")
            .arg(root().join("examples/repository.toml"))
            .arg("--state")
            .arg(&state)
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)?;
        let persisted: Value = serde_json::from_slice(&std::fs::read(&state)?)?;
        assert_eq!(persisted, result["state"]);
    }
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn observed_resolution_authenticates_later_request_and_is_pruned_on_reopen()
-> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/resolution-observed.json"))?;
    let mut snapshot = recording.snapshot(&policy)?;
    let result = evaluate(&snapshot, &policy)?;
    let id = snapshot.current["reviewThreads"]["nodes"][0]["id"]
        .as_str()
        .ok_or("thread id missing")?
        .to_owned();
    assert_eq!(
        result.state["resolved_thread_observed_at"][&id],
        "2026-09-05T14:00:00Z"
    );
    snapshot.previous = result.state;
    for node in [&mut snapshot.initial, &mut snapshot.current] {
        node["comments"]["nodes"][0]["createdAt"] = json!("2026-09-05T15:00:00Z");
        node["reviews"]["nodes"][0]["submittedAt"] = json!("2026-09-05T16:00:00Z");
    }
    assert!(
        evaluate(&snapshot, &policy)?.converged,
        "an observed resolution precedes the new request"
    );
    snapshot.previous["resolved_thread_observed_at"] = json!({});
    assert!(
        !evaluate(&snapshot, &policy)?.converged,
        "an unobserved resolution cannot authenticate the request"
    );
    snapshot.previous["resolved_thread_observed_at"] = json!({id.clone(): "2026-09-05T14:00:00Z"});
    snapshot.current["reviewThreads"]["nodes"][0]["isResolved"] = json!(false);
    assert!(
        evaluate(&snapshot, &policy)?.state["resolved_thread_observed_at"]
            .get(&id)
            .is_none()
    );
    Ok(())
}

#[test]
fn live_recording_fetches_old_review_comparison_and_finishes_with_identity()
-> Result<(), Box<dyn Error>> {
    use signalbox_convergence::fetch::{GitHubRequest, RequestFuture, record_with};
    let policy = policy()?;
    let fixture = Recording::read(&root().join("fixtures/mutations/live-exempt-comparison.json"))?;
    let mut transcript = fixture.observations.iter().flatten();
    let mut compared = Vec::new();
    let mut send = |request| -> RequestFuture<'_> {
        let result = match request {
            GitHubRequest::GraphQl { query, variables } => {
                let recorded = transcript.next().ok_or_else(|| {
                    signalbox_convergence::Error::Evidence("unexpected GraphQL request".into())
                });
                recorded.and_then(|recorded| {
                    if query != recorded.query {
                        return Err(signalbox_convergence::Error::Evidence(
                            "GraphQL query differs from recorded transcript".into(),
                        ));
                    }
                    if variables.get("id").is_some() {
                        assert_eq!(variables["id"], recorded.variables["id"]);
                    }
                    Ok(recorded.response.clone())
                })
            }
            GitHubRequest::Rest { path } => {
                let key = path
                    .split("/compare/")
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                compared.push(key.clone());
                fixture.comparisons.get(&key).cloned().ok_or_else(|| {
                    signalbox_convergence::Error::Evidence(format!("unrecorded comparison {key}"))
                })
            }
        };
        Box::pin(async move { result })
    };
    let result = futures_executor::block_on(record_with(
        &mut send,
        fixture.previous.clone(),
        &fixture.repository,
        fixture.number,
        &policy,
    ))?;
    assert!(
        transcript.next().is_none(),
        "all final identity responses must be consumed"
    );
    assert!(compared.iter().any(|key| {
        key.starts_with(
            fixture.previous["authenticated_review_head"]
                .as_str()
                .unwrap_or_default(),
        )
    }));
    let mut snapshot = result.snapshot(&policy)?;
    snapshot.previous = evaluate(&snapshot, &policy)?.state;
    assert!(
        evaluate(&snapshot, &policy)?.converged,
        "an exempt head with settled new CI retains its review"
    );
    Ok(())
}
