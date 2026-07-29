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
    #[tool_schema(description = "An optional field.")]
    optional_value: Option<String>,
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

#[test]
fn scalar_shapes_and_option_requiredness_render_exactly() {
    let schema = ScalarFields::schema();

    expect![[r#"
        {
          "additionalProperties": false,
          "properties": {
            "bool_value": {
              "description": "A boolean field.",
              "type": "boolean"
            },
            "number_value": {
              "description": "A number field.",
              "type": "number"
            },
            "optional_value": {
              "anyOf": [
                {
                  "type": "string"
                },
                {
                  "type": "null"
                }
              ],
              "description": "An optional field."
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
          },
          "required": [
            "string_value",
            "bool_value",
            "signed_value",
            "unsigned_value",
            "number_value"
          ],
          "type": "object"
        }"#]]
    .assert_eq(&format!("{schema:#}"));
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
