//! Compatibility checks for the schema forms used by the consumed wire fields.
use serde_json::Value;
use std::collections::BTreeSet;

fn strings(value: &Value) -> BTreeSet<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| v.as_str().expect("schema string"))
        .collect()
}

pub(super) fn object_shape(
    expected: &Value,
    actual: &Value,
    actual_root: &Value,
) -> Result<Vec<String>, String> {
    if !compatible(expected, expected, actual, actual_root) {
        return Err("incompatible notification root schema".into());
    }
    object_fields(
        dereference(expected, expected),
        expected,
        dereference(actual, actual_root),
        actual_root,
    )
}

fn object_fields(
    expected: &Value,
    expected_root: &Value,
    actual: &Value,
    actual_root: &Value,
) -> Result<Vec<String>, String> {
    if !strings(&expected["required"]).is_subset(&strings(&actual["required"])) {
        return Err("an adapter-required field is no longer required".into());
    }
    let Some(fields) = expected["properties"].as_object() else {
        return Ok(Vec::new());
    };
    let upstream = actual["properties"]
        .as_object()
        .ok_or("pinned object has no properties")?;
    for (name, field) in fields {
        let candidate = upstream
            .get(name)
            .ok_or_else(|| format!("consumed field removed: {name}"))?;
        if !compatible(field, expected_root, candidate, actual_root) {
            return Err(format!("incompatible consumed field schema: {name}"));
        }
    }
    Ok(upstream
        .keys()
        .filter(|name| !fields.contains_key(*name))
        .cloned()
        .collect())
}

fn dereference<'a>(mut value: &'a Value, root: &'a Value) -> &'a Value {
    loop {
        if let Some(reference) = value["$ref"].as_str() {
            value = root
                .pointer(reference.strip_prefix('#').expect("local schema reference"))
                .expect("schema reference resolves");
        } else if let Some(branches) = value["allOf"]
            .as_array()
            .filter(|branches| branches.len() == 1)
            .filter(|_| {
                value.as_object().is_some_and(|fields| {
                    fields.keys().all(|key| {
                        matches!(
                            key.as_str(),
                            "allOf"
                                | "title"
                                | "description"
                                | "default"
                                | "examples"
                                | "readOnly"
                                | "writeOnly"
                                | "deprecated"
                                | "$comment"
                        )
                    })
                })
            })
        {
            value = &branches[0];
        } else {
            return value;
        }
    }
}

fn alternatives(value: &Value) -> Option<Vec<Value>> {
    if let Some(variants) = value["anyOf"]
        .as_array()
        .or_else(|| value["oneOf"].as_array())
    {
        return Some(variants.clone());
    }
    value["type"].as_array().map(|types| {
        types
            .iter()
            .map(|kind| {
                let mut branch = value.clone();
                branch["type"] = kind.clone();
                branch
            })
            .collect()
    })
}

// Closed string enums may use enum or unions of singleton const schemas.
fn string_members(schema: &Value, root: &Value) -> Option<BTreeSet<String>> {
    let schema = dereference(schema, root);
    if let Some(branches) = alternatives(schema) {
        let mut members = BTreeSet::new();
        for branch in branches {
            members.extend(string_members(&branch, root)?);
        }
        return Some(members);
    }
    if schema.get("type").is_some_and(|kind| kind != "string") {
        return None;
    }
    let mut members = if let Some(values) = schema["enum"].as_array() {
        values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<BTreeSet<_>>>()?
    } else {
        BTreeSet::from([schema["const"].as_str()?.to_owned()])
    };
    if let Some(constant) = schema.get("const") {
        members.retain(|member| constant.as_str() == Some(member.as_str()));
    }
    Some(members)
}

fn integer_bounds(value: &Value) -> (i128, i128) {
    let format_bounds = value["format"]
        .as_str()
        .and_then(|format| {
            let (unsigned, bits) = if let Some(bits) = format.strip_prefix("uint") {
                (true, bits)
            } else {
                (false, format.strip_prefix("int")?)
            };
            let bits: u32 = bits.parse().ok()?;
            if !(1..=64).contains(&bits) {
                return None;
            }
            Some(if unsigned {
                (0, (1_i128 << bits) - 1)
            } else {
                (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
            })
        })
        .unwrap_or((i128::MIN, i128::MAX));
    let bound = |field: &str, fallback: i128| {
        value[field]
            .as_i64()
            .map(i128::from)
            .or_else(|| value[field].as_u64().map(i128::from))
            .or_else(|| {
                value[field].as_f64().map(|number| {
                    if field == "minimum" {
                        number.ceil() as i128
                    } else {
                        number.floor() as i128
                    }
                })
            })
            .unwrap_or(fallback)
    };
    (
        bound("minimum", format_bounds.0).max(format_bounds.0),
        bound("maximum", format_bounds.1).min(format_bounds.1),
    )
}

pub(super) fn compatible(
    expected: &Value,
    expected_root: &Value,
    actual: &Value,
    actual_root: &Value,
) -> bool {
    let expected = dereference(expected, expected_root);
    let actual = dereference(actual, actual_root);
    if expected == &Value::Bool(true)
        || expected.as_object().is_some_and(|fields| fields.is_empty())
    {
        return true;
    }
    if expected["x-codex-consumed-items"] == true {
        return actual["type"] == "array"
            && turn_items(
                &expected["items"],
                expected_root,
                &actual["items"],
                actual_root,
            ) == Ok(true);
    }
    if expected["x-codex-unknown-tags"] == true {
        return error_info(expected, expected_root, actual, actual_root, false).is_ok();
    }
    let expected_alternatives = alternatives(expected);
    // Keep the optional error union together when checking that its known tags remain present.
    if let Some(branches) = &expected_alternatives {
        let allows_null = branches
            .iter()
            .any(|branch| dereference(branch, expected_root)["type"] == "null");
        if let Some(error_schema) = branches
            .iter()
            .map(|branch| dereference(branch, expected_root))
            .find(|branch| branch["x-codex-unknown-tags"] == true)
            .filter(|_| allows_null && branches.len() == 2)
        {
            return error_info(error_schema, expected_root, actual, actual_root, true).is_ok();
        }
    }
    if expected["enum"].is_array() {
        return string_members(expected, expected_root).is_some_and(|members| {
            string_members(actual, actual_root).is_some_and(|actual| actual == members)
        });
    }
    if let Some(variants) = alternatives(actual) {
        return variants
            .iter()
            .all(|branch| compatible(expected, expected_root, branch, actual_root));
    }
    if let Some(variants) = expected_alternatives {
        return variants
            .iter()
            .any(|branch| compatible(branch, expected_root, actual, actual_root));
    }
    let expected_type = expected["type"].as_str();
    let actual_type = actual["type"]
        .as_str()
        .or_else(|| string_members(actual, actual_root).map(|_| "string"));
    if expected_type != actual_type
        && !(expected_type == Some("number") && actual_type == Some("integer"))
    {
        return false;
    }
    match expected_type {
        Some("object") => match object_fields(expected, expected_root, actual, actual_root) {
            Ok(additions) => {
                for field in additions {
                    println!("additive field {field}");
                }
                true
            }
            Err(_) => false,
        },
        Some("array") => compatible(
            &expected["items"],
            expected_root,
            actual.get("items").unwrap_or(&Value::Bool(true)),
            actual_root,
        ),
        Some("integer") => {
            let (low, high) = integer_bounds(expected);
            let (actual_low, actual_high) = integer_bounds(actual);
            low <= actual_low && actual_high <= high
        }
        _ => true,
    }
}

/// Preserve all representations of a tag, including both string and object forms.
pub(super) fn variants(
    schema: &Value,
    root: &Value,
) -> Result<std::collections::BTreeMap<String, Vec<Value>>, String> {
    error_variants(schema, root, false)
}

fn error_variants(
    schema: &Value,
    root: &Value,
    allows_null: bool,
) -> Result<std::collections::BTreeMap<String, Vec<Value>>, String> {
    let schema = dereference(schema, root);
    let mut result: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    if let Some(branches) = alternatives(schema) {
        for branch in branches {
            for (tag, shapes) in error_variants(&branch, root, allows_null)? {
                result.entry(tag).or_default().extend(shapes);
            }
        }
    } else if let Some(tags) = string_members(schema, root) {
        for tag in tags {
            let mut shape = schema.clone();
            shape["type"] = serde_json::json!("string");
            shape["enum"] = serde_json::json!([tag]);
            result.entry(tag).or_default().push(shape);
        }
    } else if schema["type"] == "object" {
        let fields = schema["properties"]
            .as_object()
            .ok_or("error envelope must name its tag")?;
        if fields.len() != 1 || schema["additionalProperties"] != false {
            return Err("error envelope must contain only its tag".into());
        }
        let tag = fields.keys().next().expect("one error tag");
        if !strings(&schema["required"]).contains(tag.as_str()) {
            return Err("error envelope must require its tag".into());
        }
        result.insert(tag.clone(), vec![schema.clone()]);
    } else if !(allows_null && schema["type"] == "null") {
        return Err("error must be a tagged string or object".into());
    }
    Ok(result)
}

pub(super) fn enum_members(
    expected: &BTreeSet<&str>,
    actual: &BTreeSet<&str>,
) -> Result<Vec<String>, String> {
    let missing: Vec<_> = expected.difference(actual).collect();
    if !missing.is_empty() {
        return Err(format!("enum members removed: {missing:?}"));
    }
    Ok(actual
        .difference(expected)
        .map(|tag| (*tag).to_owned())
        .collect())
}

fn error_info(
    expected: &Value,
    expected_root: &Value,
    actual: &Value,
    actual_root: &Value,
    allows_null: bool,
) -> Result<(), String> {
    let expected = variants(expected, expected_root)?;
    let actual = error_variants(actual, actual_root, allows_null)?;
    let additions = enum_members(
        &expected.keys().map(String::as_str).collect(),
        &actual.keys().map(String::as_str).collect(),
    )?;
    for tag in additions {
        println!("CodexErrorInfo: additive member {tag}");
    }
    for (tag, shapes) in expected {
        for candidate in &actual[&tag] {
            if !shapes
                .iter()
                .any(|shape| compatible(shape, expected_root, candidate, actual_root))
            {
                return Err(format!("incompatible known error representation: {tag}"));
            }
        }
    }
    Ok(())
}

// Check agent-message decoding and the non-output discriminators used by the proof gate.
// Other item payloads are retained as JSON and remain unconstrained.
fn turn_items(
    expected: &Value,
    expected_root: &Value,
    actual: &Value,
    actual_root: &Value,
) -> Result<bool, String> {
    let actual = dereference(actual, actual_root);
    if let Some(branches) = alternatives(actual) {
        let mut found = false;
        for branch in branches {
            found |= turn_items(expected, expected_root, &branch, actual_root)?;
        }
        return Ok(found);
    }
    let tag = dereference(&actual["properties"]["type"], actual_root);
    let consumed_tags = ["agentMessage", "userMessage", "hookPrompt"];
    if tag
        .get("not")
        .and_then(|excluded| string_members(excluded, actual_root))
        .is_some_and(|excluded| consumed_tags.iter().all(|tag| excluded.contains(*tag)))
    {
        return Ok(false);
    }
    let tags = tag["enum"]
        .as_array()
        .map(Vec::as_slice)
        .or_else(|| tag.get("const").map(std::slice::from_ref))
        .ok_or("item schema must identify its possible tags")?;
    if tags
        .iter()
        .any(|tag| tag.as_str().is_some_and(|tag| consumed_tags.contains(&tag)))
    {
        let discriminator = serde_json::json!({
            "type":"object", "required":["type"],
            "properties":{"type":{"type":"string"}}
        });
        if !compatible(&discriminator, &discriminator, actual, actual_root) {
            return Err("consumed item discriminator must remain a required string".into());
        }
    }
    if !tags.iter().any(|tag| tag == "agentMessage") {
        return Ok(false);
    }
    if !compatible(expected, expected_root, actual, actual_root) {
        return Err("incompatible agent-message item schema".into());
    }
    Ok(true)
}
