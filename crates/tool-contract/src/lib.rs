//! Typed daemon tool contracts and their model-facing schemas.
//!
//! A tool's argument struct is the single authority for its argument shape:
//! serde decodes it and [`ToolSchema`] renders its model-facing JSON Schema.
//! The proc-macro implementation lives in `signalbox-tool-schema-derive`.
//! Existing schemars contracts remain supported while tool crates migrate to
//! the owned derive.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use signalbox_application::{ToolDefinition, ToolInputSchema};
use signalbox_domain::{ToolEffectClass, ToolName, ToolPermissionDefault};

/// Exact UTF-8 byte length of a canonical lowercase hyphenated UUID.
pub const CANONICAL_UUID_TEXT_BYTES: usize = 36;

/// Decodes only the canonical lowercase hyphenated UUID spelling.
pub fn decode_canonical_uuid(value: &str) -> Option<uuid::Uuid> {
    if value.len() != CANONICAL_UUID_TEXT_BYTES {
        return None;
    }
    let parsed = uuid::Uuid::parse_str(value).ok()?;
    (parsed.hyphenated().to_string() == value).then_some(parsed)
}

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
///
/// It then folds a schemars internally-tagged enum root — rendered as a
/// bare `oneOf` with no root `type` — into the object-rooted shape every
/// function-tool wire requires. See `object_rooted_schema` for the fold's
/// exact terms and the roots it declines to touch.
pub fn rendered_contract_schema<Contract: ToolContract + ?Sized>() -> serde_json::Value {
    let mut value = schemars::SchemaGenerator::default()
        .into_root_schema_for::<Contract::Arguments>()
        .to_value();
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
        object.remove("description");
    }
    let mut value = object_rooted_schema(value);
    value.sort_all_objects();
    value
}

/// JSON Schema keyword holding a schema root's reusable definitions.
const DEFINITIONS_KEY: &str = "$defs";

/// Folds an internally-tagged union root into one object-rooted schema.
///
/// A function tool advertises the arguments of one call, so its schema root
/// must describe an object. schemars renders `#[serde(tag = "...")]` as a
/// bare root `oneOf` with no `type`, which providers reject outright — and
/// because one request carries the whole tool catalog, a single such schema
/// fails every exchange that offers it.
///
/// The fold keeps the union's information without the root combinator: the
/// tag becomes one `enum`-typed property whose description names each
/// variant with the properties that variant requires, and the variant
/// payloads merge into the root property set. Serde still decodes the
/// original tagged enum, so an argument object the daemon accepted before
/// the fold decodes to exactly the same value after it; the fold only widens
/// what the *advertised* schema permits, and any newly-permitted combination
/// is refused by the tool's own argument validation.
///
/// Anything else is returned unchanged: a root that already declares a
/// `type`, a union whose variants are not internally tagged objects, and a
/// union whose variants disagree about a shared property's constraints —
/// merging that last one would silently drop a constraint, so the fold
/// declines and leaves the catalog conformance gate to report it.
fn object_rooted_schema(mut value: serde_json::Value) -> serde_json::Value {
    let Some(root) = value.as_object_mut() else {
        return value;
    };
    if root.contains_key("type")
        || root
            .keys()
            .any(|key| key != "oneOf" && key != DEFINITIONS_KEY)
    {
        return value;
    }
    let Some(variants) = root.get("oneOf").and_then(serde_json::Value::as_array) else {
        return value;
    };
    let Some(folded) = folded_tagged_union(variants) else {
        return value;
    };
    root.remove("oneOf");
    root.extend(folded);
    value
}

/// Keywords a discriminating tag property may carry into the fold.
///
/// `const` names the variant and `type` may restate that it is a string.
/// The merged tag property is rebuilt from exactly those: its description is
/// generated from the variants' own documentation and the payloads each one
/// requires. A `description` declared on the tag property itself is therefore
/// replaced rather than merged, and model-facing guidance disappearing from
/// the advertised schema is the failure this fold exists to refuse — so it is
/// not admitted. Merging the two descriptions is expressible in principle;
/// leaving it out keeps the decision for the declaration that first needs it,
/// rather than inventing an attribution scheme for a case no argument type in
/// the workspace presents.
const TAG_PROPERTY_KEYWORDS: [&str; 2] = ["const", "type"];

/// One internally-tagged union branch decomposed for merging.
struct TaggedVariant<'schema> {
    description: Option<&'schema str>,
    properties: &'schema serde_json::Map<String, serde_json::Value>,
    required: BTreeSet<&'schema str>,
    closed: bool,
}

impl<'schema> TaggedVariant<'schema> {
    /// Reads this variant's constant for an already-validated tag property.
    ///
    /// The fold rebuilds the tag as an `enum` of the variants' constants,
    /// typed `string`, described by generated prose — so whatever else a
    /// branch states about the tag property is replaced rather than carried.
    /// A `maxLength` its own constant violates, or a `not` making the branch
    /// unsatisfiable, would become an advertised tag the declaration rejects,
    /// which is worse than declining: the schema would invite a call that
    /// cannot decode. Such a property is not a discriminator this fold can
    /// express, so it reads as none and the union is left alone.
    fn tag_value(&self, tag: &str) -> Option<&'schema str> {
        let property = self.properties.get(tag)?.as_object()?;
        if property
            .keys()
            .any(|keyword| !TAG_PROPERTY_KEYWORDS.contains(&keyword.as_str()))
        {
            return None;
        }
        if property
            .get("type")
            .is_some_and(|declared| declared != "string")
        {
            return None;
        }
        property.get("const")?.as_str()
    }

    /// Names the properties this variant requires beside the tag.
    fn required_payload(&self, tag: &str) -> Vec<&'schema str> {
        self.required
            .iter()
            .copied()
            .filter(|name| *name != tag)
            .collect()
    }

    /// Names the properties this variant admits but does not require.
    fn optional_payload(&self, tag: &str) -> Vec<&'schema str> {
        self.properties
            .keys()
            .map(String::as_str)
            .filter(|name| *name != tag && !self.required.contains(name))
            .collect()
    }
}

/// Merges internally-tagged variants into one object schema's members.
fn folded_tagged_union(
    branches: &[serde_json::Value],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let variants = branches
        .iter()
        .map(tagged_variant)
        .collect::<Option<Vec<_>>>()?;
    let (tag, tag_values) = sole_discriminator(&variants)?;

    let mut properties = serde_json::Map::new();
    properties.insert(
        String::from(tag),
        serde_json::json!({
            "description": discriminator_description(&variants, &tag_values, tag),
            "enum": tag_values,
            "type": "string",
        }),
    );
    let payload_names = variants
        .iter()
        .flat_map(|variant| variant.properties.keys())
        .map(String::as_str)
        .filter(|name| *name != tag)
        .collect::<BTreeSet<_>>();
    for name in &payload_names {
        properties.insert(
            String::from(*name),
            merged_property(&variants, &tag_values, name)?,
        );
    }

    let mut required = vec![serde_json::Value::String(String::from(tag))];
    required.extend(
        payload_names
            .iter()
            .filter(|name| {
                variants
                    .iter()
                    .all(|variant| variant.required.contains(**name))
            })
            .map(|name| serde_json::Value::String(String::from(*name))),
    );

    let mut folded = serde_json::Map::new();
    if variants.iter().all(|variant| variant.closed) {
        folded.insert(
            String::from("additionalProperties"),
            serde_json::Value::Bool(false),
        );
    }
    folded.insert(
        String::from("properties"),
        serde_json::Value::Object(properties),
    );
    folded.insert(String::from("required"), serde_json::Value::Array(required));
    folded.insert(
        String::from("type"),
        serde_json::Value::String(String::from("object")),
    );
    Some(folded)
}

/// Object-level keywords the fold can carry from a branch into the merged
/// root.
///
/// The merged object is rebuilt from `type`, `properties`, `required`, and
/// `additionalProperties`, with `description` spent on the tag's wording.
/// A branch stating anything else — `minProperties`, `dependentRequired`,
/// `patternProperties`, a branch-level `allOf` — constrains the argument
/// object in a way the merge has no vocabulary to reproduce across variants,
/// and reconstructing the root without it would advertise a contract weaker
/// than the one declared. That is a wider widening than the fold accepts:
/// merging is allowed to lose the *pairing* between a tag value and its
/// payload, because each family's own validator still enforces it, but it is
/// not allowed to lose a constraint outright.
const FOLDABLE_VARIANT_KEYWORDS: [&str; 5] = [
    "additionalProperties",
    "description",
    "properties",
    "required",
    "type",
];

/// Decomposes one union branch, admitting only object-shaped schemas whose
/// every keyword the merge can reproduce.
fn tagged_variant(branch: &serde_json::Value) -> Option<TaggedVariant<'_>> {
    let branch = branch.as_object()?;
    if branch.get("type")? != "object" {
        return None;
    }
    if branch
        .keys()
        .any(|keyword| !FOLDABLE_VARIANT_KEYWORDS.contains(&keyword.as_str()))
    {
        return None;
    }
    let properties = branch.get("properties")?.as_object()?;
    let required = match branch.get("required") {
        Some(required) => required
            .as_array()?
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<BTreeSet<_>>>()?,
        None => BTreeSet::new(),
    };
    // The merged root's property set is derived from the declared properties,
    // so a required name with no property of its own would not merely lose its
    // schema — it would vanish from the advertised object altogether, which
    // never mentions a member the branch demands. Declining keeps the root
    // union for the conformance gate instead.
    if required.iter().any(|name| !properties.contains_key(*name)) {
        return None;
    }
    let description = match branch.get("description") {
        Some(description) => Some(description.as_str()?),
        None => None,
    };
    // A schema-valued `additionalProperties` constrains what the extra
    // properties may be, which the merged root cannot restate per variant;
    // only the boolean forms survive the fold.
    let closed = match branch.get("additionalProperties") {
        Some(serde_json::Value::Bool(admitted)) => !admitted,
        Some(_) => return None,
        None => false,
    };
    Some(TaggedVariant {
        description,
        properties,
        required,
        closed,
    })
}

/// Names the one property every variant pins to a distinct required constant.
fn sole_discriminator<'schema>(
    variants: &[TaggedVariant<'schema>],
) -> Option<(&'schema str, Vec<&'schema str>)> {
    let mut discriminators = variants
        .first()?
        .properties
        .keys()
        .map(String::as_str)
        .filter_map(|name| Some((name, discriminating_values(variants, name)?)));
    let sole = discriminators.next()?;
    discriminators.next().is_none().then_some(sole)
}

/// Collects one candidate property's constants when it discriminates every variant.
fn discriminating_values<'schema>(
    variants: &[TaggedVariant<'schema>],
    name: &str,
) -> Option<Vec<&'schema str>> {
    let mut values = Vec::with_capacity(variants.len());
    for variant in variants {
        if !variant.required.contains(name) {
            return None;
        }
        let value = variant.tag_value(name)?;
        if values.contains(&value) {
            return None;
        }
        values.push(value);
    }
    Some(values)
}

/// Restates every variant's documentation and payload shape on the tag property.
fn discriminator_description(
    variants: &[TaggedVariant<'_>],
    tag_values: &[&str],
    tag: &str,
) -> String {
    variants
        .iter()
        .zip(tag_values)
        .map(|(variant, tag_value)| {
            let mut clause = format!("`{tag_value}`:");
            if let Some(description) = variant.description {
                clause.push(' ');
                clause.push_str(description);
            }
            let required = variant.required_payload(tag);
            let optional = variant.optional_payload(tag);
            if !required.is_empty() {
                clause.push_str(&format!(" Requires {}.", quoted_names(&required)));
            }
            if !optional.is_empty() {
                clause.push_str(&format!(" Accepts {}.", quoted_names(&optional)));
            }
            if required.is_empty() && optional.is_empty() {
                clause.push_str(" Takes no other property.");
            }
            clause
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Merges one payload property, refusing variants that disagree on constraints.
///
/// Variants that describe the property differently keep every wording,
/// each attributed to the tag values that declare it, in variant order.
fn merged_property(
    variants: &[TaggedVariant<'_>],
    tag_values: &[&str],
    name: &str,
) -> Option<serde_json::Value> {
    let mut constraints: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut described: Vec<(Vec<String>, String)> = Vec::new();
    for (variant, tag_value) in variants.iter().zip(tag_values) {
        let Some(present) = variant.properties.get(name) else {
            // Absence is skippable only from a closed branch, which forbids
            // the name outright — the merged schema for it then constrains
            // only objects that branch never admitted, and the lost pairing
            // is the widening the fold is allowed. An open branch instead
            // admits any value under that name, so letting another branch's
            // object stand as the global constraint would refuse objects this
            // one accepts. That is the narrowing an explicit `true` already
            // declines, reached implicitly, and it declines the same way.
            if variant.closed {
                continue;
            }
            return None;
        };
        // Absent and present-but-boolean are different answers and only the
        // first is a skip. `true` and `false` are valid JSON Schemas — one
        // admits every value, the other none — and neither merges with another
        // variant's object. Reading a `true` as undeclared would narrow the
        // merged property to the object branch alone, refusing values this
        // branch admits; reading a `false` that way would advertise a property
        // this branch prohibits outright. Neither is expressible in one merged
        // object, so the fold declines.
        let mut declared = present.as_object()?.clone();
        let description = declared.remove("description");
        match &constraints {
            Some(agreed) if *agreed != declared => return None,
            Some(_) => {}
            None => constraints = Some(declared),
        }
        let Some(serde_json::Value::String(description)) = description else {
            continue;
        };
        match described
            .iter_mut()
            .find(|(_, agreed)| *agreed == description)
        {
            Some((tags, _)) => tags.push(format!("`{tag_value}`")),
            None => described.push((vec![format!("`{tag_value}`")], description)),
        }
    }
    let mut merged = constraints?;
    let description = match described.as_slice() {
        [] => None,
        [(_, sole)] => Some(sole.clone()),
        grouped => Some(
            grouped
                .iter()
                .map(|(tags, description)| format!("{}: {description}", tags.join(", ")))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    };
    if let Some(description) = description {
        merged.insert(
            String::from("description"),
            serde_json::Value::String(description),
        );
    }
    Some(serde_json::Value::Object(merged))
}

fn quoted_names(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
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
        if !generated_definitions.is_empty() {
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
        }
        schema.sort_all_objects();
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

    /// Converts an owned schema object into the schemars bridge the derive macro emits.
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

    use super::{
        ToolContract, compile_contract_definition, decode_canonical_uuid, object_rooted_schema,
        rendered_contract_schema,
    };

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

    #[test]
    fn canonical_uuid_text_decodes_the_single_admitted_spelling() {
        let canonical = "00000000-0000-0000-0000-000000000001";
        let decoded = decode_canonical_uuid(canonical).expect("canonical UUID text decodes");

        assert_eq!(decoded.hyphenated().to_string(), canonical);
    }

    #[test]
    fn canonical_uuid_text_rejects_alternate_spellings() {
        assert_eq!(
            decode_canonical_uuid("00000000-0000-0000-0000-00000000000A"),
            None
        );
        assert_eq!(
            decode_canonical_uuid("00000000000000000000000000000001"),
            None
        );
    }

    #[test]
    fn canonical_uuid_text_rejects_malformed_syntax() {
        assert_eq!(decode_canonical_uuid("not-a-uuid"), None);
    }

    /// Internally tagged fixture whose variants share one differently
    /// documented property and whose first variant carries no payload.
    #[derive(Debug, serde::Deserialize, JsonSchema)]
    #[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
    enum FixtureUnionArguments {
        /// Reads the fixture's current point.
        Current,
        /// Compares two fixture points.
        Between {
            /// Older fixture point.
            #[expect(dead_code, reason = "fixture renders only")]
            start: String,
            /// Newer fixture point.
            #[expect(dead_code, reason = "fixture renders only")]
            end: String,
        },
        /// Names one fixture point.
        At {
            /// Exact fixture point.
            #[expect(dead_code, reason = "fixture renders only")]
            start: String,
        },
    }

    struct FixtureUnionContract;

    impl ToolContract for FixtureUnionContract {
        type Arguments = FixtureUnionArguments;
        const NAME: &'static str = "fixture_union_tool";
        const DESCRIPTION: &'static str = "Fixture internally tagged contract.";
    }

    /// An internally tagged root renders as one object whose tag property
    /// discriminates, never as the root `oneOf` schemars produces.
    ///
    /// The tag's description restates each variant's documentation and the
    /// properties that variant requires. `start` keeps both of its wordings,
    /// each attributed to the tag values that declare it, and is not required
    /// at the root because `current` does not declare it. `end` is described
    /// once because only one variant declares it.
    #[test]
    fn internally_tagged_arguments_render_as_one_discriminated_object() {
        let schema = rendered_contract_schema::<FixtureUnionContract>();

        expect![[r#"
            {
              "additionalProperties": false,
              "properties": {
                "end": {
                  "description": "Newer fixture point.",
                  "type": "string"
                },
                "mode": {
                  "description": "`current`: Reads the fixture's current point. Takes no other property. `between`: Compares two fixture points. Requires `end`, `start`. `at`: Names one fixture point. Requires `start`.",
                  "enum": [
                    "current",
                    "between",
                    "at"
                  ],
                  "type": "string"
                },
                "start": {
                  "description": "`between`: Older fixture point. `at`: Exact fixture point.",
                  "type": "string"
                }
              },
              "required": [
                "mode"
              ],
              "type": "object"
            }"#]]
        .assert_eq(&format!("{schema:#}"));
    }

    /// Fixture whose variants disagree about one shared property's bound.
    #[derive(Debug, serde::Deserialize, JsonSchema)]
    #[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
    enum FixtureConflictArguments {
        Short {
            #[schemars(length(max = 4))]
            #[expect(dead_code, reason = "fixture renders only")]
            label: String,
        },
        Long {
            #[schemars(length(max = 64))]
            #[expect(dead_code, reason = "fixture renders only")]
            label: String,
        },
    }

    struct FixtureConflictContract;

    impl ToolContract for FixtureConflictContract {
        type Arguments = FixtureConflictArguments;
        const NAME: &'static str = "fixture_conflict_tool";
        const DESCRIPTION: &'static str = "Fixture contract with irreconcilable variants.";
    }

    /// Merging variants that disagree about a property's constraints would
    /// silently drop one of them, so the fold declines and leaves the root
    /// union in place for the catalog conformance gate to reject.
    #[test]
    fn irreconcilable_variants_are_left_for_the_conformance_gate() {
        let schema = rendered_contract_schema::<FixtureConflictContract>();

        assert!(schema.get("type").is_none());
        assert!(schema.get("oneOf").is_some());
    }

    /// A branch stating an object-level constraint the merge cannot restate
    /// declines the fold rather than losing the constraint.
    ///
    /// `minProperties` counts the members of one variant's object. The merged
    /// root describes every variant at once, so a per-variant count has
    /// nowhere to live in it, and rebuilding the root from the keywords the
    /// fold does reproduce would advertise a schema admitting objects the
    /// declaration refuses. The fold is permitted to lose the pairing between
    /// a tag value and its payload — each family's validator still enforces
    /// that — but never a constraint outright, so the root union stands and
    /// the catalog conformance gate reports it.
    #[test]
    fn a_variant_constraint_the_merge_cannot_restate_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"mode": {"const": "bare"}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "minProperties": 2,
                    "properties": {"label": {"type": "string"}, "mode": {"const": "labelled"}},
                    "required": ["label", "mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// A tag property stating more than its constant declines the fold instead
    /// of having the surplus replaced.
    ///
    /// The merged tag is rebuilt as an `enum` of the branches' constants typed
    /// `string`, so a `maxLength` this branch's own constant already violates
    /// would not survive the rebuild. The advertised tag would then offer a
    /// value the declaration refuses — a schema inviting a call that cannot
    /// decode, which is a worse failure than declining to fold at all.
    #[test]
    fn a_tag_property_constraining_more_than_its_constant_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"mode": {"const": "brief", "maxLength": 2}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "properties": {"mode": {"const": "full"}},
                    "required": ["mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// A branch requiring a name it never declares as a property declines the
    /// fold.
    ///
    /// The merged property set is derived from the declared properties, so
    /// such a name would not merely lose its schema — the advertised object
    /// would never mention a member the branch demands, which is a widening
    /// past the tag-to-payload pairing the fold is allowed to lose.
    #[test]
    fn a_required_name_without_a_declared_property_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"mode": {"const": "bare"}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "properties": {"mode": {"const": "tenanted"}},
                    "required": ["mode", "tenant"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// A description declared on the tag property declines the fold rather
    /// than being overwritten by the generated one.
    ///
    /// The merged tag carries prose built from the variants' documentation and
    /// their required payloads, so guidance written on the tag property itself
    /// has nowhere to go. Dropping it would take model-facing text out of the
    /// advertised schema silently, which is the one loss this fold refuses
    /// everywhere else.
    #[test]
    fn a_described_tag_property_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"mode": {"const": "bare", "description": "Bare mode."}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "properties": {"mode": {"const": "full"}},
                    "required": ["mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// An open branch that omits a property another branch declares declines
    /// the fold, exactly as an explicit `true` does.
    ///
    /// Omitting a name from a branch that admits unknown members is not
    /// silence about it — the branch accepts any value there. Skipping it
    /// would let the declaring branch's schema become the constraint for every
    /// variant, refusing objects the open branch accepted. Absence may only be
    /// skipped from a closed branch, which genuinely forbids the name.
    #[test]
    fn an_open_branch_omitting_a_declared_property_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"mode": {"const": "open"}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "additionalProperties": false,
                    "properties": {"label": {"type": "string"}, "mode": {"const": "typed"}},
                    "required": ["label", "mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// A payload property declared as a boolean schema declines the fold
    /// instead of reading as undeclared.
    ///
    /// `true` and `false` are both valid JSON Schemas — one admits every
    /// value, the other none — so a branch declaring `"label": true` has
    /// declared it, and skipping it would narrow the merged property to the
    /// object branch alone, refusing values the boolean branch admits. A
    /// `false` skipped the same way would advertise a property that branch
    /// prohibits. Absence is the only thing that may be skipped here.
    #[test]
    fn a_boolean_payload_schema_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "properties": {"label": true, "mode": {"const": "open"}},
                    "required": ["label", "mode"],
                    "type": "object"
                },
                {
                    "properties": {"label": {"type": "string"}, "mode": {"const": "typed"}},
                    "required": ["label", "mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }

    /// A schema-valued `additionalProperties` restricts what the extra members
    /// may be, which the merged root cannot restate per variant, so it too
    /// declines the fold. Only the boolean forms — closed, or silent — carry
    /// through.
    #[test]
    fn a_schema_valued_additional_properties_declines_the_fold() {
        let declared = serde_json::json!({
            "oneOf": [
                {
                    "additionalProperties": {"type": "string"},
                    "properties": {"mode": {"const": "open"}},
                    "required": ["mode"],
                    "type": "object"
                },
                {
                    "properties": {"mode": {"const": "closed"}},
                    "required": ["mode"],
                    "type": "object"
                }
            ]
        });

        assert_eq!(object_rooted_schema(declared.clone()), declared);
    }
}
