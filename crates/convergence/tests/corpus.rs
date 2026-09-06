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
fn fixing_revision_reply_forms_disposition_a_thread() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/python-fixed-finding.json"))?;
    for reply in [
        "Fixed in `113746d23d6c7b398ef34464a071dca532791898`: corrected.",
        "Fixed in 113746d23d6c7b398ef34464a071dca532791898: corrected.",
        "Fixed in commit 113746d23d6c7b398ef34464a071dca532791898: corrected.",
        "Fixed in commits `113746d23d6c7b398ef34464a071dca532791898`: corrected.",
    ] {
        let mut snapshot = recording.snapshot(&policy)?;
        for node in [&mut snapshot.initial, &mut snapshot.current] {
            node["reviewThreads"]["nodes"][0]["comments"]["nodes"][1]["body"] = json!(reply);
        }
        assert_eq!(
            evaluate(&snapshot, &policy)?.undispositioned_review_threads,
            0,
            "{reply}"
        );
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
fn inventory_stability_requires_an_explicit_boolean() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let mut facts = evaluate(&recording.snapshot(&policy)?, &policy)?.facts;
    assert!(evaluate_facts(&facts, &policy).is_converged());
    facts.check_inventory_stable = false;
    assert!(!evaluate_facts(&facts, &policy).is_converged());
    let mut value = serde_json::to_value(facts)?;
    value["check_inventory_stable"] = Value::Null;
    assert!(serde_json::from_value::<signalbox_convergence::Facts>(value.clone()).is_err());
    value
        .as_object_mut()
        .ok_or("facts must be an object")?
        .remove("check_inventory_stable");
    assert!(serde_json::from_value::<signalbox_convergence::Facts>(value).is_err());
    Ok(())
}

#[test]
fn unknown_cli_options_fail_before_policy_or_evidence_io() -> Result<(), Box<dyn Error>> {
    let repository = policy()?.repository;
    for command in ["record", "evaluate"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_signalbox-converge"))
            .args([
                command,
                "--policy",
                "/does-not-exist/policy.toml",
                "--repo",
                &repository,
                "--polciy",
                "alternate.toml",
            ])
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains("unknown option --polciy"));
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_signalbox-converge"))
        .args([
            "evalute",
            "--policy",
            "/does-not-exist/policy.toml",
            "--pr",
            "1566",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("expected record or evaluate"));
    Ok(())
}

#[test]
fn draft_and_description_evidence_require_their_declared_types() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let snapshot = recording.snapshot(&policy)?;
    let facts = serde_json::to_value(evaluate(&snapshot, &policy)?.facts)?;
    for (provider_key, facts_key, malformed) in [
        ("isDraft", "is_draft", json!("false")),
        ("body", "body", json!({"text": ""})),
    ] {
        for value in [Some(Value::Null), Some(malformed), None] {
            let mut candidate = snapshot.clone();
            let mut projected = facts.clone();
            for (object, key) in [
                (&mut candidate.initial, provider_key),
                (&mut candidate.current, provider_key),
                (&mut projected, facts_key),
            ] {
                let object = object.as_object_mut().ok_or("evidence must be an object")?;
                if let Some(value) = &value {
                    object.insert(key.into(), value.clone());
                } else {
                    object.remove(key);
                }
            }
            assert!(
                evaluate(&candidate, &policy).is_err(),
                "{provider_key}: {value:?}"
            );
            assert!(serde_json::from_value::<signalbox_convergence::Facts>(projected).is_err());
        }
    }
    Ok(())
}

#[test]
fn dispositioned_escalation_remains_a_blocker_until_the_thread_closes() -> Result<(), Box<dyn Error>>
{
    let recording =
        Recording::read(&root().join("fixtures/mutations/escalated-open-after-review.json"))?;
    let policy = serde_json::from_value(recording.previous["policy_identity"].clone())?;
    let result = evaluate(&recording.snapshot(&policy)?, &policy)?;
    assert!(
        result.facts.review_threads.iter().any(|thread| {
            thread.is_dispositioned && thread.is_escalated && !thread.is_resolved
        })
    );
    assert!(!result.converged);
    assert_eq!(result.unresolved_review_threads, 1);
    assert!(
        result
            .reasons
            .iter()
            .any(|reason| reason == "unresolved-review-threads:1")
    );
    let mut resolved = result.facts;
    for thread in &mut resolved.review_threads {
        thread.is_resolved = true;
    }
    assert!(evaluate_facts(&resolved, &policy).is_converged());
    Ok(())
}

#[test]
fn policy_load_rejects_incomplete_reviewer_rules() -> Result<(), Box<dyn Error>> {
    let path = std::env::temp_dir().join(format!("convergence-policy-{}.json", std::process::id()));
    let original = policy()?;
    for pattern in ["complete", "(complete)"] {
        let mut policy = original.clone();
        policy.reviewers[0].verdict_pattern = pattern.into();
        std::fs::write(&path, serde_json::to_vec(&policy)?)?;
        let error = ConvergencePolicy::read(&path).expect_err("both verdict captures are required");
        assert!(error.to_string().contains("verdict_pattern must capture"));
    }
    for login in ["", " ", "\t", "[bot]", "[BOT]"] {
        let mut policy = original.clone();
        policy.reviewers[0].login = login.into();
        std::fs::write(&path, serde_json::to_vec(&policy)?)?;
        let error = ConvergencePolicy::read(&path).expect_err("a reviewer needs an identity");
        assert!(
            error
                .to_string()
                .contains("reviewer login must not be blank")
        );
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[test]
fn tightened_policy_requalifies_state_without_reviving_old_authentication()
-> Result<(), Box<dyn Error>> {
    let strict = policy()?;
    let mut loose = strict.clone();
    loose.reviewers[0].post_green_requests = false;
    let recording = Recording::read(&root().join("fixtures/mutations/request-pre-green.json"))?;
    let mut snapshot = recording.snapshot(&loose)?;
    let authenticated = evaluate(&snapshot, &loose)?;
    assert!(
        authenticated.converged,
        "the loose policy admits the pre-green request"
    );
    assert_eq!(
        authenticated.state["policy_identity"],
        serde_json::to_value(&loose)?
    );
    snapshot.previous = authenticated.state.clone();
    assert!(
        evaluate(&snapshot, &loose)?.converged,
        "unchanged policy retains its evidence"
    );
    let requalified = evaluate(&snapshot, &strict)?;
    assert!(
        !requalified.converged,
        "the strict policy must reject the old request"
    );
    assert!(requalified.state["authenticated_review_id"].is_null());
    assert_eq!(
        requalified.state["policy_identity"],
        serde_json::to_value(&strict)?
    );
    let mut retained = authenticated
        .state
        .as_object()
        .ok_or("state must be an object")?
        .clone();
    retained.extend(
        requalified
            .state
            .as_object()
            .ok_or("state must be an object")?
            .clone(),
    );
    snapshot.previous = Value::Object(retained);
    assert!(
        !evaluate(&snapshot, &strict)?.converged,
        "merging returned state cannot revive invalidated authentication"
    );
    Ok(())
}

#[test]
fn comment_only_head_stays_unreviewed_after_inventory_settles() -> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/comment-only-head.json"))?;
    let mut snapshot = recording.snapshot(&policy)?;
    snapshot.previous = evaluate(&snapshot, &policy)?.state;
    let result = evaluate(&snapshot, &policy)?;
    assert!(result.facts.check_inventory_stable);
    assert!(!result.facts.review_exempt_since_quiet_review);
    assert!(!result.converged);
    assert!(
        result
            .reasons
            .iter()
            .any(|reason| reason == "quiet-review-not-completed-for-current-head")
    );
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
    for name in [
        "live-exempt-comparison.json",
        "multi-commit-clean-base-forward.json",
    ] {
        let fixture = Recording::read(&root().join("fixtures/mutations").join(name))?;
        let policy = serde_json::from_value(fixture.previous["policy_identity"].clone())?;
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
                        signalbox_convergence::Error::Evidence(format!(
                            "unrecorded comparison {key}"
                        ))
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
    }
    Ok(())
}

#[test]
fn policy_controls_repository_dispositions_and_optional_gates() -> Result<(), Box<dyn Error>> {
    let original = policy()?;
    let recording = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let mut facts = evaluate(&recording.snapshot(&original)?, &original)?.facts;
    let mut policy_value = serde_json::to_value(&original)?;
    policy_value
        .as_object_mut()
        .ok_or("policy object missing")?
        .remove("description_word_limit");
    let mut configured: ConvergencePolicy = serde_json::from_value(policy_value)?;
    assert_eq!(configured.description_word_limit, None);
    facts.body = "word ".repeat(
        original
            .description_word_limit
            .ok_or("example limit missing")?
            + 1,
    );
    assert!(evaluate_facts(&facts, &configured).is_converged());
    configured.description_word_limit = Some(2);
    assert!(
        evaluate_facts(&facts, &configured)
            .reasons()
            .iter()
            .any(|reason| matches!(
                reason,
                signalbox_convergence::Reason::DescriptionWordLimitExceeded { limit: 2 }
            ))
    );
    facts.body.clear();
    facts.is_draft = true;
    assert!(!evaluate_facts(&facts, &configured).is_converged());
    configured.reject_drafts = false;
    assert!(evaluate_facts(&facts, &configured).is_converged());

    for name in ["declined-finding.json", "informational-answer.json"] {
        let recording = Recording::read(&root().join("fixtures/mutations").join(name))?;
        let snapshot = recording.snapshot(&original)?;
        assert!(evaluate(&snapshot, &original)?.converged, "{name}");
        let mut changed = original.clone();
        changed.declined_prefix.clear();
        changed.informational_classes.clear();
        assert!(!evaluate(&snapshot, &changed)?.converged, "{name}");
    }
    let mut invalid = original;
    invalid.fixed_in_commit_pattern = "revision".into();
    assert!(invalid.validate().is_err());
    Ok(())
}

#[test]
fn equivalent_timestamp_offsets_and_precision_preserve_corpus_verdicts()
-> Result<(), Box<dyn Error>> {
    fn rewrite(value: &mut Value) {
        match value {
            Value::String(text) => {
                if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
                    let seconds = if (at.timestamp() / 60) % 2 == 0 {
                        19_800
                    } else {
                        -28_800
                    };
                    if let Some(offset) = chrono::FixedOffset::east_opt(seconds) {
                        *text = at
                            .with_timezone(&offset)
                            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false);
                    }
                }
            }
            Value::Object(object) => {
                for child in object.values_mut() {
                    rewrite(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    rewrite(child);
                }
            }
            _ => {}
        }
    }
    let policy = policy()?;
    let expected: BTreeMap<String, Value> =
        serde_json::from_slice(&std::fs::read(root().join("fixtures/expected.json"))?)?;
    for (name, expected) in expected {
        if expected.get("error").is_some() {
            continue;
        }
        let recording = Recording::read(&root().join("fixtures").join(&name))?;
        let before = evaluate(&recording.snapshot(&policy)?, &policy)?;
        let mut recording = serde_json::to_value(recording)?;
        rewrite(&mut recording);
        let recording: Recording = serde_json::from_value(recording)?;
        let after = evaluate(&recording.snapshot(&policy)?, &policy)?;
        assert_eq!(before.verdict, after.verdict, "{name}");
    }
    Ok(())
}

#[test]
fn later_authenticated_finding_invalidates_quiet_review_and_persisted_authentication()
-> Result<(), Box<dyn Error>> {
    let policy = policy()?;
    let settled = Recording::read(&root().join("fixtures/mutations/settled.json"))?;
    let prior = evaluate(&settled.snapshot(&policy)?, &policy)?.state;
    for name in [
        "later-body-finding.json",
        "later-changes-requested-finding.json",
    ] {
        let recording = Recording::read(&root().join("fixtures/mutations").join(name))?;
        let mut snapshot = recording.snapshot(&policy)?;
        assert!(!evaluate(&snapshot, &policy)?.converged);
        snapshot.previous = prior.clone();
        assert!(
            !evaluate(&snapshot, &policy)?.converged,
            "a later finding also revokes retained quiet authentication"
        );
        for node in [&mut snapshot.initial, &mut snapshot.current] {
            node["reviews"]["nodes"][0]["submittedAt"] = json!("2026-09-05T15:00:00Z");
        }
        assert!(
            evaluate(&snapshot, &policy)?.converged,
            "a quiet review after the finding authenticates the head again"
        );
    }
    Ok(())
}

#[test]
fn escalation_must_follow_the_latest_reviewer_edit() -> Result<(), Box<dyn Error>> {
    let recording =
        Recording::read(&root().join("fixtures/mutations/escalation-before-reviewer-edit.json"))?;
    let policy = serde_json::from_value(recording.previous["policy_identity"].clone())?;
    let mut snapshot = recording.snapshot(&policy)?;
    let result = evaluate(&snapshot, &policy)?;
    assert!(!result.facts.review_threads[0].is_escalated);
    assert!(!result.facts.review_threads[0].is_dispositioned);
    for node in [&mut snapshot.initial, &mut snapshot.current] {
        node["reviewThreads"]["nodes"][0]["comments"]["nodes"][1]["createdAt"] =
            json!("2026-09-05T15:00:00Z");
    }
    let result = evaluate(&snapshot, &policy)?;
    assert!(result.facts.review_threads[0].is_escalated);
    assert!(result.facts.review_threads[0].is_dispositioned);
    assert!(
        !result.converged,
        "the provider still shows the thread open"
    );
    Ok(())
}
