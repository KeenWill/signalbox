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
    object_fields(expected, expected, actual, actual_root)
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
    while let Some(reference) = value["$ref"].as_str() {
        value = root
            .pointer(reference.strip_prefix('#').expect("local schema reference"))
            .expect("schema reference resolves");
    }
    value
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
    if let Some(variants) = alternatives(actual) {
        return variants
            .iter()
            .all(|branch| compatible(expected, expected_root, branch, actual_root));
    }
    if let Some(variants) = alternatives(expected) {
        return variants
            .iter()
            .any(|branch| compatible(branch, expected_root, actual, actual_root));
    }
    let expected_type = expected["type"].as_str();
    let actual_type = actual["type"].as_str();
    if expected_type != actual_type
        && !(expected_type == Some("number") && actual_type == Some("integer"))
    {
        return false;
    }
    if expected["enum"].is_array()
        && (!actual["enum"].is_array()
            || !strings(&actual["enum"]).is_subset(&strings(&expected["enum"])))
    {
        return false;
    }
    match expected_type {
        Some("object") => object_fields(expected, expected_root, actual, actual_root).is_ok(),
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
