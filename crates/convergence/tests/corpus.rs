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
            assert_eq!(
                json!({"converged":result.converged,"reasons":result.reasons}),
                expected,
                "{name}"
            );
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
