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
        "TurnCompletedNotification",
        derived::<frame::TurnCompleted>(),
        &turns,
        &turns,
    );
    check_object(
        "AccountRateLimitsUpdatedNotification",
        derived::<frame::AccountRateLimitsUpdated>(),
        &rates,
        &rates,
    );
}

use schema_shape::{enum_members, variants};

fn check_errors(schema: &Value) -> Result<(), String> {
    schema_shape::object_shape(&derived::<frame::ErrorNotification>(), schema, schema).map(|_| ())
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
    let expected_variant =
        variants(&expected, &expected).unwrap()["httpConnectionFailed"][0].clone();
    let document = schema("ErrorNotification");
    let mut actual =
        variants(&document["definitions"]["CodexErrorInfo"], &document).unwrap()["httpConnectionFailed"][0].clone();
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
    assert!(check_errors(&actual).is_err());
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
    assert!(check_errors(&actual).is_err());
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

#[test]
fn inlined_error_schemas_are_checked_at_each_consumed_notification() {
    for (name, expected) in [
        ("ErrorNotification", derived::<frame::ErrorNotification>()),
        (
            "TurnCompletedNotification",
            derived::<frame::TurnCompleted>(),
        ),
    ] {
        let mut actual = schema(name);
        let error_info = actual["definitions"]["CodexErrorInfo"].clone();
        actual["definitions"]["TurnError"]["properties"]["codexErrorInfo"] =
            serde_json::json!({"anyOf":[error_info, {"type":"null"}]});
        assert!(
            schema_shape::object_shape(&expected, &actual, &actual).is_ok(),
            "{name}"
        );
        let inline =
            &mut actual["definitions"]["TurnError"]["properties"]["codexErrorInfo"]["anyOf"][0];
        let http_error = inline["oneOf"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|shape| shape["properties"]["httpConnectionFailed"].is_object())
            .unwrap();
        http_error["properties"]["httpConnectionFailed"]["properties"]["httpStatusCode"] =
            serde_json::json!({"type":["string","null"]});
        assert!(
            schema_shape::object_shape(&expected, &actual, &actual).is_err(),
            "{name}"
        );
    }
}

#[test]
fn an_unrestricted_inline_error_object_is_incompatible() {
    let mut actual = schema("ErrorNotification");
    actual["definitions"]["TurnError"]["properties"]["codexErrorInfo"] =
        serde_json::json!({"type":["object","null"]});
    assert!(check_errors(&actual).is_err());
}

#[test]
fn unknown_error_tags_remain_compatible_at_the_consumed_field() {
    let mut actual = schema("ErrorNotification");
    actual["definitions"]["CodexErrorInfo"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .extend([
            serde_json::json!({"type":"string", "enum":["futureUnitError"]}),
            serde_json::json!({
                "type":"object", "additionalProperties":false, "required":["futureDataError"],
                "properties":{"futureDataError":{"type":"object"}}
            }),
        ]);
    assert_eq!(check_errors(&actual), Ok(()));
}

#[test]
fn a_removed_turn_status_fails_the_consumed_turn_schema_check() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    actual["definitions"]["TurnStatus"]["enum"]
        .as_array_mut()
        .unwrap()
        .retain(|status| status != "inProgress");
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}

#[test]
fn an_inlined_error_union_can_flatten_its_nullable_alternative() {
    let mut actual = schema("ErrorNotification");
    let mut error_info = actual["definitions"]["CodexErrorInfo"].clone();
    error_info["oneOf"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"type":"null"}));
    actual["definitions"]["TurnError"]["properties"]["codexErrorInfo"] = error_info;
    assert_eq!(check_errors(&actual), Ok(()));
}

#[test]
fn agent_message_fields_are_checked_through_referenced_and_inlined_turn_items() {
    let expected = derived::<frame::TurnCompleted>();
    for inline in [false, true] {
        let mut actual = schema("TurnCompletedNotification");
        if inline {
            actual["definitions"]["Turn"]["properties"]["items"]["items"] =
                actual["definitions"]["ThreadItem"].clone();
        }
        assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
        let items = if inline {
            &mut actual["definitions"]["Turn"]["properties"]["items"]["items"]
        } else {
            &mut actual["definitions"]["ThreadItem"]
        };
        let message = items["oneOf"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|item| strings(&item["properties"]["type"]["enum"]).contains("agentMessage"))
            .unwrap();
        message["properties"]["text"]["type"] = serde_json::json!(["string", "null"]);
        assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
    }
}

#[test]
fn unconsumed_item_payloads_can_change_and_new_item_tags_are_allowed() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    let items = actual["definitions"]["ThreadItem"]["oneOf"]
        .as_array_mut()
        .unwrap();
    for item in items.iter_mut() {
        if !strings(&item["properties"]["type"]["enum"]).contains("agentMessage") {
            item["properties"]["text"] = serde_json::json!({"type":["string","null"]});
        }
    }
    items.push(serde_json::json!({
        "type":"object", "required":["type"],
        "properties":{"type":{"type":"string","enum":["futureItem"]},"payload":{}}
    }));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
}

#[test]
fn consumed_item_discriminators_remain_required_strings() {
    let expected = derived::<frame::TurnCompleted>();
    for tag in ["userMessage", "hookPrompt", "agentMessage"] {
        for mutation in ["optional", "nullable", "removed"] {
            let mut actual = schema("TurnCompletedNotification");
            let item = actual["definitions"]["ThreadItem"]["oneOf"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|item| strings(&item["properties"]["type"]["enum"]).contains(tag))
                .unwrap();
            match mutation {
                "optional" => item["required"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|field| field != "type"),
                "nullable" => {
                    item["properties"]["type"]["type"] = serde_json::json!(["string", "null"]);
                    item["properties"]["type"]["enum"]
                        .as_array_mut()
                        .unwrap()
                        .push(Value::Null);
                }
                "removed" => {
                    item["properties"].as_object_mut().unwrap().remove("type");
                }
                _ => panic!("unknown discriminator mutation"),
            }
            assert!(
                schema_shape::object_shape(&expected, &actual, &actual).is_err(),
                "{tag}: {mutation}"
            );
        }
    }
}

#[test]
fn notification_roots_cannot_become_nullable() {
    for (name, expected) in [
        ("ErrorNotification", derived::<frame::ErrorNotification>()),
        (
            "TurnCompletedNotification",
            derived::<frame::TurnCompleted>(),
        ),
        (
            "AccountRateLimitsUpdatedNotification",
            derived::<frame::AccountRateLimitsUpdated>(),
        ),
    ] {
        let mut actual = schema(name);
        actual["type"] = serde_json::json!(["object", "null"]);
        assert!(
            schema_shape::object_shape(&expected, &actual, &actual).is_err(),
            "{name}"
        );
    }
}

#[test]
fn an_unconsumed_item_can_use_a_constant_discriminator() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    let item = actual["definitions"]["ThreadItem"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|item| strings(&item["properties"]["type"]["enum"]).contains("plan"))
        .unwrap();
    let tag = item["properties"]["type"].as_object_mut().unwrap();
    tag.remove("enum");
    tag.insert("const".into(), serde_json::json!("plan"));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
}

#[test]
fn closed_turn_statuses_accept_equivalent_constant_unions() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    let statuses = actual["definitions"]["TurnStatus"]["enum"]
        .as_array()
        .unwrap();
    let alternatives: Vec<_> = statuses
        .iter()
        .map(|status| serde_json::json!({"const": status}))
        .collect();
    actual["definitions"]["TurnStatus"] = serde_json::json!({"oneOf": alternatives});
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
    actual["definitions"]["TurnStatus"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"const":"futureStatus"}));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}

#[test]
fn catch_all_items_can_explicitly_exclude_every_consumed_tag() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    actual["definitions"]["ThreadItem"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type":"object", "required":["type"], "properties":{
                "type":{"type":"string", "not":{"enum":["agentMessage","userMessage","hookPrompt"]}}
            }
        }));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
    let catch_all = actual["definitions"]["ThreadItem"]["oneOf"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    catch_all["properties"]["type"]["not"]["enum"] =
        serde_json::json!(["userMessage", "hookPrompt"]);
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}

#[test]
fn a_known_unit_error_can_use_a_singleton_string_constant() {
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
    alternatives.push(serde_json::json!({"const":"unauthorized"}));
    assert_eq!(check_errors(&actual), Ok(()));
}

#[test]
fn singleton_string_discriminators_need_no_redundant_type_keyword() {
    let expected = derived::<frame::TurnCompleted>();
    for tag in ["userMessage", "hookPrompt", "agentMessage"] {
        for constant in [false, true] {
            let mut actual = schema("TurnCompletedNotification");
            let item = actual["definitions"]["ThreadItem"]["oneOf"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|item| strings(&item["properties"]["type"]["enum"]).contains(tag))
                .unwrap();
            let discriminator = item["properties"]["type"].as_object_mut().unwrap();
            discriminator.remove("type");
            if constant {
                discriminator.remove("enum");
                discriminator.insert("const".into(), serde_json::json!(tag));
            }
            assert!(
                schema_shape::object_shape(&expected, &actual, &actual).is_ok(),
                "{tag}: constant={constant}"
            );
            let item = actual["definitions"]["ThreadItem"]["oneOf"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|item| {
                    item["properties"]["type"]["const"] == tag
                        || strings(&item["properties"]["type"]["enum"]).contains(tag)
                })
                .unwrap();
            item["required"]
                .as_array_mut()
                .unwrap()
                .retain(|field| field != "type");
            assert!(
                schema_shape::object_shape(&expected, &actual, &actual).is_err(),
                "{tag}: constant={constant}"
            );
        }
    }
}

#[test]
fn transparent_all_of_wrappers_preserve_consumed_status_checks() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    let status = actual["definitions"]["Turn"]["properties"]["status"].clone();
    actual["definitions"]["Turn"]["properties"]["status"] =
        serde_json::json!({"allOf":[{"allOf":[status]}]});
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
    actual["definitions"]["TurnStatus"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("futureStatus"));
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}

#[test]
fn all_of_wrappers_ignore_annotations_but_preserve_value_constraints() {
    let expected = derived::<frame::TurnCompleted>();
    let mut actual = schema("TurnCompletedNotification");
    let status = actual["definitions"]["Turn"]["properties"]["status"].clone();
    actual["definitions"]["Turn"]["properties"]["status"] = serde_json::json!({
        "allOf":[status], "title":"Turn status", "description":"Current turn status",
        "default":"completed", "examples":["completed"], "readOnly":true,
        "writeOnly":false, "deprecated":false, "$comment":"Status schema"
    });
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_ok());
    actual["definitions"]["Turn"]["properties"]["status"]["enum"] =
        serde_json::json!(["completed"]);
    assert!(schema_shape::object_shape(&expected, &actual, &actual).is_err());
}
