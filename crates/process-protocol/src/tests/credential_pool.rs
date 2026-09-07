//! Known pool evidence fails closed before a client can expose it.
use super::support::*;
use crate::*;

/// Arbitrary distinct correlations; the member order and exclusion are under test.
fn exhaustion_state(exclusion: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "failed_credential_pool_exhausted",
        "terminal_frontier_id": uuid(1), "terminal_attempt_id": uuid(2),
        "failure_entry_id": uuid(3), "pool_policy_id": uuid(4),
        "policy_members": ["first", "second"],
        "members": [
            {"profile": "first", "reset_at_unix_ms": null, "exclusion": exclusion},
            {"profile": "second", "reset_at_unix_ms": null,
             "exclusion": {"kind": "profile_quarantine", "record_generation": "7"}}
        ]
    })
}

#[test]
fn pool_exhaustion_accepts_each_closed_exclusion_arm() {
    for exclusion in [
        serde_json::json!({"kind":"profile_quarantine","record_generation":null}),
        serde_json::json!({"kind":"membership_exclusion","record_generation":"4"}),
        serde_json::json!({"kind":"session_displacement","record_generation":null}),
        serde_json::json!({"kind":"chain_exclusion","predecessor_model_call_id":uuid(5)}),
        serde_json::json!({"kind":"transient_exclusion","observation_model_call_id":uuid(6)}),
        serde_json::json!({"kind":"headroom_reserve","observed_headroom_percent":10,"reserve_percent":10}),
    ] {
        let state = exhaustion_state(exclusion.clone());
        let decoded: TurnState = serde_json::from_value(state.clone()).expect("valid evidence arm");
        assert_eq!(
            serde_json::to_value(decoded).expect("wire encoding"),
            state,
            "{exclusion}"
        );
    }
}

#[test]
fn pool_exhaustion_rejects_partial_reordered_and_duplicate_members() {
    let baseline =
        exhaustion_state(serde_json::json!({"kind":"profile_quarantine","record_generation":null}));
    let mut partial = baseline.clone();
    partial["members"]
        .as_array_mut()
        .expect("fixture array")
        .pop();
    let mut reordered = baseline.clone();
    reordered["members"]
        .as_array_mut()
        .expect("fixture array")
        .reverse();
    let mut duplicate = baseline.clone();
    duplicate["policy_members"][1] = serde_json::json!("first");
    duplicate["members"][1]["profile"] = serde_json::json!("first");
    let mut oversized = baseline;
    oversized["policy_members"][0] = serde_json::json!("x".repeat(257));
    oversized["members"][0]["profile"] = oversized["policy_members"][0].clone();
    for (case, payload) in [
        ("partial", partial),
        ("reordered", reordered),
        ("duplicate", duplicate),
        ("oversized", oversized),
    ] {
        assert!(
            serde_json::from_value::<TurnState>(payload).is_err(),
            "{case}"
        );
    }
}

#[test]
fn pool_exhaustion_rejects_unknown_exclusions_and_invalid_generations() {
    for exclusion in [
        serde_json::json!({"kind":"future_exclusion"}),
        serde_json::json!({"kind":"profile_quarantine"}),
        serde_json::json!({"kind":"profile_quarantine","record_generation":"0"}),
        serde_json::json!({"kind":"profile_quarantine","record_generation":null,"secret":"unexpected"}),
        serde_json::json!({"kind":"headroom_reserve","observed_headroom_percent":11,"reserve_percent":10}),
        serde_json::json!({"kind":"headroom_reserve","observed_headroom_percent":10,"reserve_percent":101}),
    ] {
        assert!(
            serde_json::from_value::<TurnState>(exhaustion_state(exclusion.clone())).is_err(),
            "{exclusion}"
        );
    }
}
