//! Derive-based daemon tool contracts.
//!
//! A tool's argument struct is the single authority for its argument shape:
//! the struct's serde implementation is the decoder, and its schemars
//! implementation renders the model-facing JSON Schema, so the two cannot
//! drift apart. `#[serde(deny_unknown_fields)]` on every argument struct
//! keeps the rendered `additionalProperties: false` and the decoder's
//! rejection of unexpected members in agreement by construction.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use signalbox_application::{ToolDefinition, ToolInputSchema};
use signalbox_domain::{ToolEffectClass, ToolName, ToolPermissionDefault};

/// One daemon tool's model-facing contract: registry name, description, and
/// the typed argument shape its schema is derived from.
pub trait ToolContract {
    /// Typed argument shape decoded by serde and rendered by schemars.
    type Arguments: DeserializeOwned + JsonSchema;

    /// Registry name the model proposes this tool under.
    const NAME: &'static str;

    /// Model-facing description.
    const DESCRIPTION: &'static str;
}

/// A static contract could not compile into a registry definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolContractCompileError {
    /// The contract name was rejected by the registry name rules.
    Name,
    /// The rendered schema was rejected by the registry schema rules.
    Schema,
}

/// Renders one contract's argument schema as a self-contained JSON object.
///
/// The derived root keeps schemars' draft 2020-12 vocabulary but drops three
/// root annotations: `$schema` and `title` because the wire contract has
/// never carried them and the Rust type name a derived title would leak is
/// not model-facing, and the root `description` the argument struct's own doc
/// comment would render, because [`ToolContract::DESCRIPTION`] already
/// carries the model-facing tool description in the definition itself. Field
/// doc comments stay: they render as the per-property descriptions. Argument
/// newtypes implement [`JsonSchema::inline_schema`], so the rendered object
/// references no external definitions.
pub fn rendered_contract_schema<Contract: ToolContract + ?Sized>() -> serde_json::Value {
    let mut value = schemars::SchemaGenerator::default()
        .into_root_schema_for::<Contract::Arguments>()
        .to_value();
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
        object.remove("description");
    }
    value
}

/// Compiles one contract into the immutable registry definition.
///
/// Permission default and effect class stay declaration-site facts: they are
/// execution policy, not argument shape, and each tool states them beside its
/// executor wiring.
pub fn compile_contract_definition<Contract: ToolContract + ?Sized>(
    permission_default: ToolPermissionDefault,
    effect_class: ToolEffectClass,
) -> Result<ToolDefinition, ToolContractCompileError> {
    let name = ToolName::try_new(String::from(Contract::NAME))
        .map_err(|_| ToolContractCompileError::Name)?;
    let schema = ToolInputSchema::try_new(rendered_contract_schema::<Contract>().to_string())
        .map_err(|_| ToolContractCompileError::Schema)?;
    Ok(ToolDefinition::new(
        name,
        String::from(Contract::DESCRIPTION),
        schema,
        permission_default,
        effect_class,
    ))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use expect_test::expect;
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use signalbox_domain::{ToolEffectClass, ToolPermissionDefault};

    use super::{ToolContract, compile_contract_definition, rendered_contract_schema};

    /// Fixture newtype whose manual schema states a real constraint.
    #[derive(Debug, serde::Deserialize)]
    struct BoundedLabel(#[expect(dead_code, reason = "fixture decodes only")] String);

    impl JsonSchema for BoundedLabel {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("BoundedLabel")
        }

        fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "type": "string",
                "maxLength": 16,
            })
        }

        fn inline_schema() -> bool {
            true
        }
    }

    /// Rust-facing summary that must not become a root description.
    #[derive(Debug, serde::Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct FixtureArguments {
        /// Exact fixture label.
        #[expect(dead_code, reason = "fixture renders only")]
        label: BoundedLabel,
    }

    struct FixtureContract;

    impl ToolContract for FixtureContract {
        type Arguments = FixtureArguments;
        const NAME: &'static str = "fixture_tool";
        const DESCRIPTION: &'static str = "Fixture contract for rendering tests.";
    }

    /// The rendered root drops the `$schema`, `title`, and struct-doc
    /// `description` annotations, inlines the newtype's constraint schema
    /// under the field's doc-comment description, and states
    /// `additionalProperties: false` from `deny_unknown_fields`.
    #[test]
    fn rendered_schema_is_self_contained_and_annotation_free() {
        let schema = rendered_contract_schema::<FixtureContract>();

        expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "label": {
                  "description": "Exact fixture label.",
                  "maxLength": 16,
                  "type": "string"
                }
              },
              "required": [
                "label"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
    }

    /// The compiled definition carries the contract's name and description and
    /// stores the rendered schema in canonical compact form.
    #[test]
    fn compiled_definition_binds_contract_facts_to_the_registry_shape() {
        let definition = compile_contract_definition::<FixtureContract>(
            ToolPermissionDefault::Auto,
            ToolEffectClass::EffectFree,
        )
        .expect("fixture contract compiles");

        assert_eq!(definition.name().as_str(), FixtureContract::NAME);
        assert_eq!(definition.description(), FixtureContract::DESCRIPTION);
        assert_eq!(
            definition.input_schema().as_str(),
            rendered_contract_schema::<FixtureContract>().to_string()
        );
        assert_eq!(definition.permission_default(), ToolPermissionDefault::Auto);
        assert_eq!(definition.effect_class(), ToolEffectClass::EffectFree);
    }
}
