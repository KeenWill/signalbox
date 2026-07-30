use std::collections::BTreeMap;

use expect_test::expect;
use signalbox_tool_contract::ToolSchema as _;
use signalbox_tool_schema_derive::ToolSchema;

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct ScalarFields {
    #[tool_schema(description = "A string field.")]
    string_value: String,
    #[tool_schema(description = "A boolean field.")]
    bool_value: bool,
    #[tool_schema(description = "A signed integer field.")]
    signed_value: i64,
    #[tool_schema(description = "An unsigned integer field.")]
    unsigned_value: u64,
    #[tool_schema(description = "A number field.")]
    number_value: f64,
}

#[derive(serde::Deserialize, ToolSchema)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct OptionalField {
    #[tool_schema(description = "An optional field.")]
    value: Option<String>,
}

type MaybeString = Option<String>;

#[derive(serde::Deserialize, ToolSchema)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct AliasedOptionalField {
    #[tool_schema(description = "An aliased optional field.")]
    value: MaybeString,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct NestedFields {
    #[tool_schema(description = "Nested label.")]
    label: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct CollectionFields {
    #[tool_schema(description = "Ordered labels.")]
    labels: Vec<String>,
    #[tool_schema(description = "Named switches.")]
    switches: BTreeMap<String, bool>,
    #[tool_schema(description = "Boxed nested shape.")]
    nested: Box<NestedFields>,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[expect(dead_code, reason = "schema fixture renders only")]
struct RenamedFields {
    #[tool_schema(description = "Container-renamed field.")]
    regular_name: String,
    #[serde(rename = "exact_name")]
    #[tool_schema(description = "Explicitly renamed field.", name = "exact_name")]
    explicit_name: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(rename_all = "UPPERCASE")]
#[expect(dead_code, reason = "schema fixture renders only")]
struct NonAsciiRenamedField {
    #[tool_schema(description = "ASCII-only renamed field.")]
    über_name: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(rename_all(serialize = "camelCase"))]
#[expect(dead_code, reason = "schema fixture renders only")]
struct SerializationOnlyRenamedField {
    #[serde(rename(serialize = "wire"))]
    #[tool_schema(description = "Deserialize-name field.")]
    original_name: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(rename_all = "lowercase")]
#[allow(
    non_snake_case,
    reason = "fixture distinguishes serde lowercase from lowercasing"
)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct LowercaseRenamedField {
    #[tool_schema(description = "Unchanged mixed-case field.")]
    camelCase: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct RawIdentifierField {
    #[tool_schema(description = "Keyword-named field.")]
    r#type: String,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct DefaultAndSkippedFields {
    #[serde(default)]
    #[tool_schema(description = "Defaulted field.")]
    defaulted: String,
    #[serde(skip)]
    skipped: String,
}

#[derive(Default, serde::Deserialize, ToolSchema)]
#[serde(default, deny_unknown_fields)]
struct ContainerDefaultFields {
    #[tool_schema(description = "Defaulted text.")]
    text: String,
    #[tool_schema(description = "Defaulted count.")]
    count: u64,
}

fn string_from_number<'de, Deserializer>(
    deserializer: Deserializer,
) -> Result<String, Deserializer::Error>
where
    Deserializer: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.to_string())
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct CustomDecoderField {
    #[serde(deserialize_with = "string_from_number")]
    #[tool_schema(description = "Numeric wire value.", with = u64)]
    value: String,
}

fn optional_string_from_number<'de, Deserializer>(
    deserializer: Deserializer,
) -> Result<Option<String>, Deserializer::Error>
where
    Deserializer: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    Ok(Some(value.to_string()))
}

#[derive(serde::Deserialize, ToolSchema)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct CustomOptionalDecoderField {
    #[serde(deserialize_with = "optional_string_from_number")]
    #[tool_schema(description = "Required numeric wire value.", with = u64)]
    value: Option<String>,
}

struct AnyValue;

impl signalbox_tool_contract::ToolSchema for AnyValue {
    fn schema() -> serde_json::Value {
        serde_json::Value::Bool(true)
    }
}

#[derive(ToolSchema)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct BooleanSchemaField {
    #[tool_schema(description = "Any JSON value.")]
    value: AnyValue,
}

#[derive(serde::Deserialize, ToolSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "schema fixture renders only")]
struct RecursiveNode {
    #[tool_schema(description = "Next node.")]
    child: Option<Box<RecursiveNode>>,
}

struct LegacyRecursiveOuter;

impl signalbox_tool_contract::__private::schemars::JsonSchema for LegacyRecursiveOuter {
    fn schema_name() -> ::std::borrow::Cow<'static, str> {
        ::std::borrow::Cow::Borrowed("LegacyRecursiveOuter")
    }

    #[expect(
        clippy::expect_used,
        reason = "fixture schema is statically object-valued"
    )]
    fn json_schema(
        generator: &mut signalbox_tool_contract::__private::schemars::SchemaGenerator,
    ) -> signalbox_tool_contract::__private::schemars::Schema {
        let node = generator.subschema_for::<RecursiveNode>().to_value();
        signalbox_tool_contract::__private::schemars::Schema::try_from(serde_json::json!({
            "properties": {
                "node": node,
            },
            "required": ["node"],
            "type": "object",
        }))
        .expect("legacy fixture schema is object-valued")
    }
}

struct ManualRecursiveRoot;

impl signalbox_tool_contract::ToolSchema for ManualRecursiveRoot {
    fn schema() -> serde_json::Value {
        signalbox_tool_contract::__private::root_schema(|| {
            serde_json::json!({
                "properties": {
                    "node": RecursiveNode::schema(),
                },
                "type": "object",
            })
        })
    }
}

fn manual_leaf_schema() -> serde_json::Value {
    serde_json::json!({ "type": "string" })
}

struct ManualDefinitionsRoot;

impl signalbox_tool_contract::ToolSchema for ManualDefinitionsRoot {
    fn schema() -> serde_json::Value {
        signalbox_tool_contract::__private::root_schema(|| {
            serde_json::json!({
                "$defs": {
                    "ManualLeaf": manual_leaf_schema(),
                },
                "properties": {
                    "manual": { "$ref": "#/$defs/ManualLeaf" },
                    "node": RecursiveNode::schema(),
                },
                "type": "object",
            })
        })
    }
}

struct ConflictingDefinitionsRoot;

impl signalbox_tool_contract::ToolSchema for ConflictingDefinitionsRoot {
    fn schema() -> serde_json::Value {
        signalbox_tool_contract::__private::root_schema(|| {
            serde_json::json!({
                "$defs": {
                    "derive::RecursiveNode": { "type": "string" },
                },
                "properties": {
                    "node": RecursiveNode::schema(),
                },
                "type": "object",
            })
        })
    }
}

#[test]
fn scalar_shapes_render_exactly() {
    let schema = ScalarFields::schema();
    let properties = &schema["properties"];

    expect![[r#"
        {
          "bool_value": {
            "description": "A boolean field.",
            "type": "boolean"
          },
          "number_value": {
            "description": "A number field.",
            "type": "number"
          },
          "signed_value": {
            "description": "A signed integer field.",
            "maximum": 9223372036854775807,
            "minimum": -9223372036854775808,
            "type": "integer"
          },
          "string_value": {
            "description": "A string field.",
            "type": "string"
          },
          "unsigned_value": {
            "description": "An unsigned integer field.",
            "maximum": 18446744073709551615,
            "minimum": 0,
            "type": "integer"
          }
        }"#]]
    .assert_eq(&format!("{properties:#}"));
}

#[test]
fn scalar_fields_are_required() {
    let schema = ScalarFields::schema();
    let required = &schema["required"];

    expect![[r#"
        [
          "string_value",
          "bool_value",
          "signed_value",
          "unsigned_value",
          "number_value"
        ]"#]]
    .assert_eq(&format!("{required:#}"));
}

#[test]
fn option_field_renders_nullable_shape() {
    let schema = OptionalField::schema();
    let value_schema = &schema["properties"]["value"];

    expect![[r#"
        {
          "anyOf": [
            {
              "type": "string"
            },
            {
              "type": "null"
            }
          ],
          "description": "An optional field."
        }"#]]
    .assert_eq(&format!("{value_schema:#}"));
}

#[test]
fn option_field_is_not_required() {
    let schema = OptionalField::schema();

    assert!(schema.get("required").is_none());
}

#[test]
fn option_alias_field_is_not_required() {
    let schema = AliasedOptionalField::schema();

    assert!(schema.get("required").is_none());
}

#[test]
fn collection_and_nested_shapes_render_exactly() {
    let schema = CollectionFields::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "labels": {
              "description": "Ordered labels.",
              "items": {
                "type": "string"
              },
              "type": "array"
            },
            "nested": {
              "additionalProperties": false,
              "description": "Boxed nested shape.",
              "properties": {
                "label": {
                  "description": "Nested label.",
                  "type": "string"
                }
              },
              "required": [
                "label"
              ],
              "type": "object"
            },
            "switches": {
              "additionalProperties": {
                "type": "boolean"
              },
              "description": "Named switches.",
              "type": "object"
            }
          },
          "required": [
            "labels",
            "switches",
            "nested"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_and_checked_schema_names_render_exactly() {
    let schema = RenamedFields::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "exact_name": {
              "description": "Explicitly renamed field.",
              "type": "string"
            },
            "regularName": {
              "description": "Container-renamed field.",
              "type": "string"
            }
          },
          "required": [
            "regularName",
            "exact_name"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_non_ascii_rename_uses_ascii_case_conversion() {
    let schema = NonAsciiRenamedField::schema();

    expect![[r#"
        {
          "properties": {
            "üBER_NAME": {
              "description": "ASCII-only renamed field.",
              "type": "string"
            }
          },
          "required": [
            "üBER_NAME"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_serialization_only_renames_leave_deserialize_names_unchanged() {
    let schema = SerializationOnlyRenamedField::schema();

    expect![[r#"
        {
          "properties": {
            "original_name": {
              "description": "Deserialize-name field.",
              "type": "string"
            }
          },
          "required": [
            "original_name"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_lowercase_rule_leaves_struct_field_spelling_unchanged() {
    let schema = LowercaseRenamedField::schema();

    expect![[r#"
        {
          "properties": {
            "camelCase": {
              "description": "Unchanged mixed-case field.",
              "type": "string"
            }
          },
          "required": [
            "camelCase"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn raw_identifier_uses_its_unraw_serde_and_helper_name() {
    let schema = RawIdentifierField::schema();

    expect![[r#"
        {
          "properties": {
            "type": {
              "description": "Keyword-named field.",
              "type": "string"
            }
          },
          "required": [
            "type"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_default_and_skip_control_property_presence() {
    let schema = DefaultAndSkippedFields::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "defaulted": {
              "description": "Defaulted field.",
              "type": "string"
            }
          },
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn serde_container_default_makes_every_field_optional() {
    let schema = ContainerDefaultFields::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "count": {
              "description": "Defaulted count.",
              "maximum": 18446744073709551615,
              "minimum": 0,
              "type": "integer"
            },
            "text": {
              "description": "Defaulted text.",
              "type": "string"
            }
          },
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn custom_decoder_uses_declared_wire_shape() {
    let schema = CustomDecoderField::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "value": {
              "description": "Numeric wire value.",
              "maximum": 18446744073709551615,
              "minimum": 0,
              "type": "integer"
            }
          },
          "required": [
            "value"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn custom_decoder_option_without_default_remains_required() {
    let schema = CustomOptionalDecoderField::schema();

    expect![[r#"
        {
          "properties": {
            "value": {
              "description": "Required numeric wire value.",
              "maximum": 18446744073709551615,
              "minimum": 0,
              "type": "integer"
            }
          },
          "required": [
            "value"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn boolean_custom_schema_retains_its_description() {
    let schema = BooleanSchemaField::schema();

    expect![[r#"
        {
          "properties": {
            "value": {
              "allOf": [
                true
              ],
              "description": "Any JSON value."
            }
          },
          "required": [
            "value"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn recursive_structures_render_finite_definitions_and_references() {
    let schema = RecursiveNode::schema();

    expect![[r##"
        {
          "$defs": {
            "derive::RecursiveNode": {
              "additionalProperties": false,
              "properties": {
                "child": {
                  "anyOf": [
                    {
                      "$ref": "#/$defs/derive::RecursiveNode"
                    },
                    {
                      "type": "null"
                    }
                  ],
                  "description": "Next node."
                }
              },
              "type": "object"
            }
          },
          "additionalProperties": false,
          "properties": {
            "child": {
              "anyOf": [
                {
                  "$ref": "#/$defs/derive::RecursiveNode"
                },
                {
                  "type": "null"
                }
              ],
              "description": "Next node."
            }
          },
          "type": "object"
        }"##]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn recursive_composite_root_hoists_definitions() {
    let schema = <Vec<RecursiveNode>>::schema();

    expect![[r##"
        {
          "$defs": {
            "derive::RecursiveNode": {
              "additionalProperties": false,
              "properties": {
                "child": {
                  "anyOf": [
                    {
                      "$ref": "#/$defs/derive::RecursiveNode"
                    },
                    {
                      "type": "null"
                    }
                  ],
                  "description": "Next node."
                }
              },
              "type": "object"
            }
          },
          "items": {
            "additionalProperties": false,
            "properties": {
              "child": {
                "anyOf": [
                  {
                    "$ref": "#/$defs/derive::RecursiveNode"
                  },
                  {
                    "type": "null"
                  }
                ],
                "description": "Next node."
              }
            },
            "type": "object"
          },
          "type": "array"
        }"##]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn manual_recursive_composition_hoists_definitions() {
    let schema = ManualRecursiveRoot::schema();

    expect![[r##"
        {
          "$defs": {
            "derive::RecursiveNode": {
              "additionalProperties": false,
              "properties": {
                "child": {
                  "anyOf": [
                    {
                      "$ref": "#/$defs/derive::RecursiveNode"
                    },
                    {
                      "type": "null"
                    }
                  ],
                  "description": "Next node."
                }
              },
              "type": "object"
            }
          },
          "properties": {
            "node": {
              "additionalProperties": false,
              "properties": {
                "child": {
                  "anyOf": [
                    {
                      "$ref": "#/$defs/derive::RecursiveNode"
                    },
                    {
                      "type": "null"
                    }
                  ],
                  "description": "Next node."
                }
              },
              "type": "object"
            }
          },
          "type": "object"
        }"##]]
    .assert_eq(&format!("{schema:#}"));
}

#[test]
fn schemars_bridge_hoists_recursive_definitions() {
    let schema = signalbox_tool_contract::__private::schemars::SchemaGenerator::default()
        .into_root_schema_for::<LegacyRecursiveOuter>()
        .to_value();

    assert!(schema["$defs"].get("derive::RecursiveNode").is_some());
    assert_eq!(
        schema["properties"]["node"]["properties"]["child"]["anyOf"][0]["$ref"],
        "#/$defs/derive::RecursiveNode"
    );
}

#[test]
fn schemars_bridge_rewrites_recursive_references_for_draft07() {
    let mut generator =
        signalbox_tool_contract::__private::schemars::generate::SchemaSettings::draft07()
            .into_generator();
    let schema = generator
        .root_schema_for::<LegacyRecursiveOuter>()
        .to_value();

    assert!(schema["definitions"].get("derive::RecursiveNode").is_some());
    assert_eq!(
        schema["properties"]["node"]["properties"]["child"]["anyOf"][0]["$ref"],
        "#/definitions/derive::RecursiveNode"
    );
}

#[test]
fn manual_definitions_survive_recursive_composition() {
    let schema = ManualDefinitionsRoot::schema();

    assert_eq!(schema["$defs"]["ManualLeaf"], manual_leaf_schema());
}

#[test]
#[should_panic(expected = "schema definition collision for `derive::RecursiveNode`")]
fn conflicting_manual_definition_is_rejected() {
    let _schema = ConflictingDefinitionsRoot::schema();
}
