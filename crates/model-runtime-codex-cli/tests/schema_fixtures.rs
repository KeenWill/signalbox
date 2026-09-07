//! The adapter's consumed shapes are checked against the pinned public schemas.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "schema fixture assertions fail with the incompatible shape"
)]

// Compile the private wire definitions here so the guard checks the decoder's
// fields and enum members without exporting protocol types from the adapter.
#[allow(
    dead_code,
    reason = "this test exercises only schema-bearing wire types"
)]
#[path = "../src/app_server/frame.rs"]
mod frame;

use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf};

fn schema(name: &str) -> Value {
    let directory = std::env::var_os("SIGNALBOX_CODEX_SCHEMA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tooling/codex-cli/schema")
        });
    serde_json::from_slice(
        &std::fs::read(directory.join(format!("{name}.json"))).expect("pinned schema is readable"),
    )
    .expect("pinned schema is JSON")
}

fn strings(value: &Value) -> BTreeSet<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| entry.as_str().expect("schema set member is a string"))
        .collect()
}

#[path = "support/schema_shape.rs"]
mod schema_shape;

fn check_object(label: &str, expected: Value, actual: &Value, root: &Value) {
    let additions = schema_shape::object_shape(&expected, actual, root)
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    for field in additions {
        println!("{label}: additive field {field}");
    }
}

fn derived<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("derived schema serializes")
}

#[test]
fn pinned_notifications_preserve_consumed_and_adapter_required_fields() {
    let errors = schema("ErrorNotification");
    let turns = schema("TurnCompletedNotification");
    let rates = schema("AccountRateLimitsUpdatedNotification");
    check_object(
        "ErrorNotification",
        derived::<frame::ErrorNotification>(),
        &errors,
        &errors,
    );
    check_object(
        "TurnError",
        derived::<frame::TurnError>(),
        &errors["definitions"]["TurnError"],
        &errors,
    );
    check_object(
        "TurnCompletedNotification.TurnError",
        derived::<frame::TurnError>(),
        &turns["definitions"]["TurnError"],
        &turns,
    );
    check_object(
        "TurnCompletedNotification",
        derived::<frame::TurnCompleted>(),
        &turns,
        &turns,
    );
    check_object(
        "Turn",
        derived::<frame::Turn>(),
        &turns["definitions"]["Turn"],
        &turns,
    );
    check_object(
        "AccountRateLimitsUpdatedNotification",
        derived::<frame::AccountRateLimitsUpdated>(),
        &rates,
        &rates,
    );
    check_object(
        "RateLimitSnapshot",
        derived::<frame::RateLimits>(),
        &rates["definitions"]["RateLimitSnapshot"],
        &rates,
    );
    check_object(
        "RateLimitWindow",
        derived::<frame::RateLimitWindow>(),
        &rates["definitions"]["RateLimitWindow"],
        &rates,
    );
}

fn variants(schema: &Value) -> std::collections::BTreeMap<String, Vec<Value>> {
    let mut variants: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for entry in schema["oneOf"]
        .as_array()
        .expect("error info is a tagged union")
    {
        if let Some(tags) = entry["enum"].as_array() {
            for tag in tags {
                let mut shape = entry.clone();
                shape["enum"] = serde_json::json!([tag]);
                variants
                    .entry(tag.as_str().expect("tag is a string").to_owned())
                    .or_default()
                    .push(shape);
            }
        } else {
            for tag in entry["properties"]
                .as_object()
                .expect("data variant has properties")
                .keys()
            {
                variants.entry(tag.clone()).or_default().push(entry.clone());
            }
        }
    }
    variants
}

fn enum_members(expected: &BTreeSet<&str>, actual: &BTreeSet<&str>) -> Result<Vec<String>, String> {
    let missing: Vec<_> = expected.difference(actual).collect();
    if !missing.is_empty() {
        return Err(format!("enum members removed: {missing:?}"));
    }
    Ok(actual
        .difference(expected)
        .map(|tag| (*tag).to_owned())
        .collect())
}

#[test]
fn pinned_error_and_turn_enums_preserve_the_adapter_members() {
    let errors = schema("ErrorNotification");
    let turns = schema("TurnCompletedNotification");
    for schema in [&errors, &turns] {
        check_errors(schema).expect("known error representations remain decoder-compatible");
    }
    let expected = derived::<frame::TurnStatus>();
    assert_eq!(
        strings(&expected["enum"]),
        strings(&turns["definitions"]["TurnStatus"]["enum"]),
        "the closed TurnStatus decoder requires the same members"
    );
}

fn check_errors(schema: &Value) -> Result<(), String> {
    let expected_schema = derived::<frame::KnownError>();
    let expected = variants(&expected_schema);
    let actual = variants(&schema["definitions"]["CodexErrorInfo"]);
    for (tag, representations) in &actual {
        for candidate in representations {
            if candidate["type"] == "object"
                && (candidate["additionalProperties"] != false
                    || !strings(&candidate["required"]).contains(tag.as_str())
                    || !candidate["properties"]
                        .as_object()
                        .is_some_and(|fields| fields.len() == 1 && fields.contains_key(tag)))
            {
                return Err(format!("error envelope must contain only its tag: {tag}"));
            }
        }
    }
    let additions = enum_members(
        &expected.keys().map(String::as_str).collect(),
        &actual.keys().map(String::as_str).collect(),
    )?;
    for tag in additions {
        println!("CodexErrorInfo: additive member {tag}");
    }
    for (tag, shapes) in expected {
        for candidate in &actual[&tag] {
            let shape = shapes
                .iter()
                .find(|shape| schema_shape::compatible(shape, &expected_schema, candidate, schema))
                .ok_or_else(|| format!("incompatible known error representation: {tag}"))?;
            if shape["type"] == "object" {
                check_object(
                    &tag,
                    shape["properties"][&tag].clone(),
                    &candidate["properties"][&tag],
                    schema,
                );
            }
        }
    }
    Ok(())
}

#[test]
fn additions_are_allowed_and_consumed_field_or_requirement_removals_fail() {
    let expected = serde_json::json!({"required":["id"],"properties":{"id":{},"optional":{}}});
    let mut actual = expected.clone();
    actual["properties"]["future"] = serde_json::json!({});
    assert_eq!(
        schema_shape::object_shape(&expected, &actual, &actual),
        Ok(vec!["future".to_owned()])
    );
    actual["required"] = serde_json::json!(["id", "future"]);
    assert_eq!(
        schema_shape::object_shape(&expected, &actual, &actual),
        Ok(vec!["future".to_owned()])
    );
    actual["required"] = serde_json::json!(["future"]);
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
    actual = expected.clone();
    actual["properties"]
        .as_object_mut()
        .unwrap()
        .remove("optional");
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
    let expected = BTreeSet::from(["known"]);
    assert_eq!(
        enum_members(&expected, &BTreeSet::from(["known", "future"])),
        Ok(vec!["future".to_owned()])
    );
    assert!(enum_members(&expected, &BTreeSet::from(["future"])).is_err());
}

#[test]
fn a_new_turn_status_fails_the_consumed_turn_schema_check() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    actual["definitions"]["TurnStatus"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("futureStatus"));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}

#[test]
fn incompatible_consumed_field_types_fail_through_references_and_nullable_variants() {
    let expected = derived::<frame::ErrorNotification>();
    let mut actual = schema("ErrorNotification");
    actual["definitions"]["TurnError"]["properties"]["message"] =
        serde_json::json!({"type":"integer"});
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
    let mut actual = schema("ErrorNotification");
    actual["properties"]["threadId"]["type"] = serde_json::json!(["string", "null"]);
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());

    let expected = derived::<frame::KnownError>();
    let expected_variant = variants(&expected)["httpConnectionFailed"][0].clone();
    let document = schema("ErrorNotification");
    let mut actual =
        variants(&document["definitions"]["CodexErrorInfo"])["httpConnectionFailed"][0].clone();
    actual["properties"]["httpConnectionFailed"]["properties"]["httpStatusCode"]["type"] =
        serde_json::json!(["string", "null"]);
    assert!(!schema_shape::compatible(
        &expected_variant,
        &expected,
        &actual,
        &document
    ));
    actual["properties"]["httpConnectionFailed"]["properties"]["httpStatusCode"] =
        serde_json::json!({"type":["integer","null"],"format":"int64"});
    assert!(!schema_shape::compatible(
        &expected_variant,
        &expected,
        &actual,
        &document
    ));
    actual["properties"]["httpConnectionFailed"]["properties"]["httpStatusCode"] =
        serde_json::json!({"type":["integer","null"],"format":"uint8"});
    assert!(schema_shape::compatible(
        &expected_variant,
        &expected,
        &actual,
        &document
    ));
}

#[test]
fn a_known_unit_error_cannot_change_to_an_object_representation() {
    let mut actual = schema("ErrorNotification");
    let alternatives = actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap();
    alternatives
        .iter_mut()
        .find(|shape| strings(&shape["enum"]).contains("unauthorized"))
        .unwrap()["enum"]
        .as_array_mut()
        .unwrap()
        .retain(|tag| tag != "unauthorized");
    alternatives.push(serde_json::json!({
        "type":"object", "additionalProperties":false, "required":["unauthorized"],
        "properties":{"unauthorized":{"type":"object"}}
    }));
    assert_eq!(
        check_errors(&actual),
        Err("incompatible known error representation: unauthorized".into())
    );
}

#[test]
fn a_known_unit_error_cannot_add_an_object_representation() {
    let mut actual = schema("ErrorNotification");
    actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type":"object", "additionalProperties":false, "required":["unauthorized"],
            "properties":{"unauthorized":{"type":"object"}}
        }));
    assert_eq!(
        check_errors(&actual),
        Err("incompatible known error representation: unauthorized".into())
    );
}

#[test]
fn a_tagged_error_envelope_cannot_add_a_sibling_field() {
    let mut actual = schema("ErrorNotification");
    let envelope = actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|shape| shape["properties"]["httpConnectionFailed"].is_object())
        .unwrap();
    envelope["properties"]["diagnostic"] = serde_json::json!({"type":"string"});
    assert!(check_errors(&actual).is_err());
}

#[test]
fn a_tagged_error_envelope_cannot_allow_arbitrary_fields() {
    let mut actual = schema("ErrorNotification");
    let envelope = actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|shape| shape["properties"]["httpConnectionFailed"].is_object())
        .unwrap();
    envelope
        .as_object_mut()
        .unwrap()
        .remove("additionalProperties");
    assert!(check_errors(&actual).is_err());
}

#[test]
fn a_tagged_error_payload_can_add_a_field() {
    let mut actual = schema("ErrorNotification");
    let envelope = actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|shape| shape["properties"]["httpConnectionFailed"].is_object())
        .unwrap();
    envelope["properties"]["httpConnectionFailed"]["properties"]["diagnostic"] =
        serde_json::json!({"type":"string"});
    assert_eq!(check_errors(&actual), Ok(()));
}
