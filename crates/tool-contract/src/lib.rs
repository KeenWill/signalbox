//! Typed daemon tool contracts and their model-facing schemas.
//!
//! A tool's argument struct is the single authority for its argument shape:
//! serde decodes it and [`ToolSchema`] renders its model-facing JSON Schema.
//! The proc-macro implementation lives in `signalbox-tool-schema-derive`.
//! Existing schemars contracts remain supported while tool crates migrate to
//! the owned derive.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use signalbox_application::{ToolDefinition, ToolInputSchema};
use signalbox_domain::{ToolEffectClass, ToolName, ToolPermissionDefault};

/// A Rust type that owns its model-facing JSON Schema declaration.
pub trait ToolSchema {
    /// Renders the complete JSON Schema for this type.
    ///
    /// A manual implementation that composes other [`ToolSchema`] values wraps
    /// its complete assembly expression in [`__private::root_schema`] so any
    /// recursive definitions attach to the public schema root.
    fn schema() -> serde_json::Value;

    #[doc(hidden)]
    fn is_optional() -> bool {
        false
    }
}

/// One daemon tool's model-facing contract: registry name, description, and
/// the typed argument shape its schema is derived from.
pub trait ToolContract {
    /// Typed argument shape decoded by serde and rendered as JSON Schema.
    ///
    /// The schemars bound is a compatibility seam for contracts not yet
    /// migrated to [`ToolSchema`]. The owned derive supplies this bridge.
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
/// Compatibility rendering removes schemars' root annotations because the
/// wire contract has never carried them. Schemas produced by [`ToolSchema`]
/// contain no such annotations, so the removal is a no-op for migrated types.
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

fn scalar_schema(kind: &'static str) -> serde_json::Value {
    serde_json::json!({ "type": kind })
}

macro_rules! impl_scalar_schema {
    ($($type:ty => $kind:literal),+ $(,)?) => {
        $(
            impl ToolSchema for $type {
                fn schema() -> serde_json::Value {
                    scalar_schema($kind)
                }
            }
        )+
    }
}

impl_scalar_schema! {
    String => "string",
    bool => "boolean",
    f32 => "number",
    f64 => "number",
}

macro_rules! impl_integer_schema {
    ($($type:ty),+ $(,)?) => {
        $(
            impl ToolSchema for $type {
                fn schema() -> serde_json::Value {
                    serde_json::json!({
                        "maximum": <$type>::MAX,
                        "minimum": <$type>::MIN,
                        "type": "integer",
                    })
                }
            }
        )+
    }
}

impl_integer_schema! {
    i8, i16, i32, i64, isize,
    u8, u16, u32, u64, usize,
}

impl<Value: ToolSchema> ToolSchema for Option<Value> {
    fn schema() -> serde_json::Value {
        __private::root_schema(|| {
            serde_json::json!({
                "anyOf": [
                    Value::schema(),
                    { "type": "null" },
                ],
            })
        })
    }

    fn is_optional() -> bool {
        true
    }
}

impl<Value: ToolSchema> ToolSchema for Box<Value> {
    fn schema() -> serde_json::Value {
        __private::root_schema(Value::schema)
    }
}

impl<Value: ToolSchema> ToolSchema for Vec<Value> {
    fn schema() -> serde_json::Value {
        __private::root_schema(|| {
            serde_json::json!({
                "items": Value::schema(),
                "type": "array",
            })
        })
    }
}

impl<Value: ToolSchema> ToolSchema for std::collections::BTreeMap<String, Value> {
    fn schema() -> serde_json::Value {
        __private::root_schema(|| {
            serde_json::json!({
                "additionalProperties": Value::schema(),
                "type": "object",
            })
        })
    }
}

/// Implementation details used by the derive expansion.
#[doc(hidden)]
pub mod __private {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    pub use schemars;
    pub use serde_json;

    thread_local! {
        static RENDER_STATE: RefCell<Option<RenderState>> = const { RefCell::new(None) };
    }

    #[derive(Default)]
    struct RenderState {
        definitions: BTreeMap<&'static str, serde_json::Value>,
        referenced: BTreeSet<&'static str>,
        stack: Vec<&'static str>,
    }

    struct RenderRootGuard {
        finished: bool,
    }

    impl RenderRootGuard {
        #[expect(
            clippy::expect_used,
            reason = "root schema rendering establishes state before constructing its guard"
        )]
        fn finish(mut self) -> RenderState {
            let state = RENDER_STATE.with(|slot| {
                slot.borrow_mut()
                    .take()
                    .expect("root schema rendering state must exist")
            });
            assert!(
                state.stack.is_empty(),
                "schema rendering stack must be empty at the public root"
            );
            self.finished = true;
            state
        }
    }

    impl Drop for RenderRootGuard {
        fn drop(&mut self) {
            if !self.finished {
                clear_render_state();
            }
        }
    }

    struct NamedSchemaGuard {
        finished: bool,
        name: &'static str,
    }

    impl NamedSchemaGuard {
        #[expect(
            clippy::expect_used,
            reason = "named schemas render only inside an established root context"
        )]
        fn finish(mut self, schema: &serde_json::Value) {
            RENDER_STATE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let state = slot
                    .as_mut()
                    .expect("named schema rendering state must exist");
                complete_named_schema(state, self.name, schema);
            });
            self.finished = true;
        }
    }

    impl Drop for NamedSchemaGuard {
        fn drop(&mut self) {
            if !self.finished {
                clear_render_state();
            }
        }
    }

    fn clear_render_state() {
        RENDER_STATE.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }

    #[expect(
        clippy::expect_used,
        reason = "every named schema guard pushes exactly one rendering stack entry"
    )]
    fn complete_named_schema(
        state: &mut RenderState,
        name: &'static str,
        schema: &serde_json::Value,
    ) {
        let rendered_name = state
            .stack
            .pop()
            .expect("schema rendering stack must contain the current type");
        assert_eq!(
            rendered_name, name,
            "schema rendering stack must unwind in declaration order"
        );
        if state.referenced.contains(name) {
            state.definitions.insert(name, schema.clone());
        }
    }

    /// Owns a complete schema render so recursive definitions attach at its root.
    ///
    /// Manual [`crate::ToolSchema`] implementations that compose other schemas
    /// call this once around their complete assembly expression.
    #[expect(
        clippy::expect_used,
        reason = "recursive definitions require an object-valued public root"
    )]
    pub fn root_schema<Build>(build: Build) -> serde_json::Value
    where
        Build: FnOnce() -> serde_json::Value,
    {
        let root = RENDER_STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return false;
            }
            *slot = Some(RenderState::default());
            true
        });
        if !root {
            return build();
        }

        let guard = RenderRootGuard { finished: false };
        let mut schema = build();
        let generated_definitions = guard.finish().definitions;
        if generated_definitions.is_empty() {
            return schema;
        }
        let object = schema
            .as_object_mut()
            .expect("a recursive public schema root must be an object");
        let definitions = object
            .entry(String::from("$defs"))
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("a public schema root $defs value must be an object");
        for (name, definition) in generated_definitions {
            insert_definition(definitions, String::from(name), definition);
        }
        schema
    }

    /// Renders a derived named schema with cycle-aware `$defs` references.
    #[expect(
        clippy::expect_used,
        reason = "named schemas render only inside root_schema"
    )]
    pub fn named_schema<Schema, Build>(build: Build) -> serde_json::Value
    where
        Build: FnOnce() -> serde_json::Value,
    {
        root_schema(|| {
            let name = std::any::type_name::<Schema>();
            let recursive = RENDER_STATE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let state = slot
                    .as_mut()
                    .expect("named schema rendering state must exist");
                if state.stack.contains(&name) {
                    state.referenced.insert(name);
                    return true;
                }
                state.stack.push(name);
                false
            });
            if recursive {
                return serde_json::json!({
                    "$ref": format!("#/$defs/{}", json_pointer_segment(name)),
                });
            }

            let guard = NamedSchemaGuard {
                finished: false,
                name,
            };
            let schema = build();
            guard.finish(&schema);
            schema
        })
    }

    fn insert_definition(
        definitions: &mut serde_json::Map<String, serde_json::Value>,
        name: String,
        definition: serde_json::Value,
    ) {
        if let Some(existing) = definitions.get(&name) {
            assert_eq!(
                existing, &definition,
                "schema definition collision for `{name}`"
            );
            return;
        }
        definitions.insert(name, definition);
    }

    fn json_pointer_segment(value: &str) -> String {
        value.replace('~', "~0").replace('/', "~1")
    }

    /// Attaches one required field description to its value schema.
    pub fn described_schema(
        mut schema: serde_json::Value,
        description: &'static str,
    ) -> serde_json::Value {
        match schema.as_object_mut() {
            Some(object) => {
                object.insert(
                    String::from("description"),
                    serde_json::Value::String(String::from(description)),
                );
                schema
            }
            None => serde_json::json!({
                "allOf": [schema],
                "description": description,
            }),
        }
    }

    /// Builds one object schema from derive-checked property declarations.
    pub fn object_schema(
        properties: Vec<(&'static str, serde_json::Value)>,
        required: Vec<Option<&'static str>>,
        deny_unknown_fields: bool,
    ) -> serde_json::Value {
        let required = required.into_iter().flatten().collect::<Vec<_>>();
        let properties = properties
            .into_iter()
            .map(|(name, schema)| (String::from(name), schema))
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let mut schema = serde_json::Map::new();
        if deny_unknown_fields {
            schema.insert(
                String::from("additionalProperties"),
                serde_json::Value::Bool(false),
            );
        }
        schema.insert(
            String::from("properties"),
            serde_json::Value::Object(properties),
        );
        if !required.is_empty() {
            schema.insert(
                String::from("required"),
                serde_json::Value::Array(
                    required
                        .into_iter()
                        .map(|name| serde_json::Value::String(String::from(name)))
                        .collect(),
                ),
            );
        }
        schema.insert(
            String::from("type"),
            serde_json::Value::String(String::from("object")),
        );
        serde_json::Value::Object(schema)
    }

    fn rewrite_definition_references(value: &mut serde_json::Value, definitions_path: &str) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(serde_json::Value::String(reference)) = object.get_mut("$ref")
                    && let Some(suffix) = reference.strip_prefix("#/$defs/")
                {
                    *reference = format!("#{definitions_path}/{suffix}");
                }
                for nested in object.values_mut() {
                    rewrite_definition_references(nested, definitions_path);
                }
            }
            serde_json::Value::Array(values) => {
                for nested in values {
                    rewrite_definition_references(nested, definitions_path);
                }
            }
            _ => {}
        }
    }

    /// Converts an owned schema object into the legacy schemars bridge.
    #[expect(
        clippy::expect_used,
        reason = "ToolSchema implementations produce object-valued JSON Schema fragments"
    )]
    pub fn into_schemars_schema(
        mut value: serde_json::Value,
        generator: &mut schemars::SchemaGenerator,
    ) -> schemars::Schema {
        let definitions_path = String::from(generator.settings().definitions_path.as_ref());
        rewrite_definition_references(&mut value, &definitions_path);
        if let Some(definitions) = value
            .as_object_mut()
            .and_then(|object| object.remove("$defs"))
        {
            let definitions = definitions
                .as_object()
                .expect("ToolSchema $defs must be an object");
            for (name, definition) in definitions {
                insert_definition(
                    generator.definitions_mut(),
                    name.clone(),
                    definition.clone(),
                );
            }
        }

        schemars::Schema::try_from(value).expect("ToolSchema must produce a JSON Schema object")
    }
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
