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

fn object_shape(expected: &Value, actual: &Value) -> Result<Vec<String>, String> {
    if !strings(&expected["required"]).is_subset(&strings(&actual["required"])) {
        return Err(format!(
            "adapter-required fields no longer required: adapter={} pinned={}",
            expected["required"], actual["required"]
        ));
    }
    let expected = expected["properties"]
        .as_object()
        .expect("adapter object has properties");
    let actual = actual["properties"]
        .as_object()
        .ok_or("pinned object has no properties")?;
    for field in expected.keys() {
        if !actual.contains_key(field) {
            return Err(format!("consumed field removed: {field}"));
        }
    }
    Ok(actual
        .keys()
        .filter(|field| !expected.contains_key(*field))
        .cloned()
        .collect())
}

fn check_object(label: &str, expected: Value, actual: &Value) {
    let additions =
        object_shape(&expected, actual).unwrap_or_else(|error| panic!("{label}: {error}"));
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
    );
    check_object(
        "TurnError",
        derived::<frame::TurnError>(),
        &errors["definitions"]["TurnError"],
    );
    check_object(
        "TurnCompletedNotification.TurnError",
        derived::<frame::TurnError>(),
        &turns["definitions"]["TurnError"],
    );
    check_object(
        "TurnCompletedNotification",
        derived::<frame::TurnCompleted>(),
        &turns,
    );
    check_object(
        "Turn",
        derived::<frame::Turn>(),
        &turns["definitions"]["Turn"],
    );
    check_object(
        "AccountRateLimitsUpdatedNotification",
        derived::<frame::AccountRateLimitsUpdated>(),
        &rates,
    );
    check_object(
        "RateLimitSnapshot",
        derived::<frame::RateLimits>(),
        &rates["definitions"]["RateLimitSnapshot"],
    );
    check_object(
        "RateLimitWindow",
        derived::<frame::RateLimitWindow>(),
        &rates["definitions"]["RateLimitWindow"],
    );
}

fn variants(schema: &Value) -> std::collections::BTreeMap<String, Value> {
    let mut variants = std::collections::BTreeMap::new();
    for entry in schema["oneOf"]
        .as_array()
        .expect("error info is a tagged union")
    {
        if let Some(tags) = entry["enum"].as_array() {
            for tag in tags {
                variants.insert(
                    tag.as_str().expect("tag is a string").to_owned(),
                    Value::Null,
                );
            }
        } else {
            for tag in entry["properties"]
                .as_object()
                .expect("data variant has properties")
                .keys()
            {
                variants.insert(tag.clone(), entry.clone());
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
        check_errors(&schema["definitions"]["CodexErrorInfo"]);
    }
    let expected = derived::<frame::TurnStatus>();
    let additions = enum_members(
        &strings(&expected["enum"]),
        &strings(&turns["definitions"]["TurnStatus"]["enum"]),
    )
    .expect("turn statuses remain present");
    for tag in additions {
        println!("TurnStatus: additive member {tag}");
    }
}

fn check_errors(schema: &Value) {
    let expected = variants(&derived::<frame::KnownError>());
    let actual = variants(schema);
    let additions = enum_members(
        &expected.keys().map(String::as_str).collect(),
        &actual.keys().map(String::as_str).collect(),
    )
    .expect("known error members remain present");
    for tag in additions {
        println!("CodexErrorInfo: additive member {tag}");
    }
    for (tag, shape) in expected {
        if !shape.is_null() {
            check_object(&tag, shape.clone(), &actual[&tag]);
            check_object(
                &tag,
                shape["properties"][&tag].clone(),
                &actual[&tag]["properties"][&tag],
            );
        }
    }
}

#[test]
fn additions_are_allowed_and_consumed_field_or_requirement_removals_fail() {
    let expected = serde_json::json!({"required":["id"],"properties":{"id":{},"optional":{}}});
    let mut actual = expected.clone();
    actual["properties"]["future"] = serde_json::json!({});
    assert_eq!(
        object_shape(&expected, &actual),
        Ok(vec!["future".to_owned()])
    );
    actual["required"] = serde_json::json!(["id", "future"]);
    assert_eq!(
        object_shape(&expected, &actual),
        Ok(vec!["future".to_owned()])
    );
    actual["required"] = serde_json::json!(["future"]);
    assert!(object_shape(&expected, &actual).is_err());
    actual = expected.clone();
    actual["properties"]
        .as_object_mut()
        .unwrap()
        .remove("optional");
    assert!(object_shape(&expected, &actual).is_err());
    let expected = BTreeSet::from(["known"]);
    assert_eq!(
        enum_members(&expected, &BTreeSet::from(["known", "future"])),
        Ok(vec!["future".to_owned()])
    );
    assert!(enum_members(&expected, &BTreeSet::from(["future"])).is_err());
}
