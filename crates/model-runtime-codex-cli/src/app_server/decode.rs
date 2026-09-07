use serde::de::DeserializeOwned;
use serde_json::Value;
use signalbox_model_runtime::{
    RedactingSink, provider_json_has_duplicate_members, trailing_credential_context,
    validate_provider_json_nesting,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProtocolError(pub(crate) &'static str);

pub(crate) fn parse(line: &[u8]) -> Result<Value, ProtocolError> {
    validate_provider_json_nesting(line)
        .map_err(|_| ProtocolError("frame exceeds JSON nesting bound"))?;
    let text = std::str::from_utf8(line).map_err(|_| ProtocolError("frame is not UTF-8"))?;
    if provider_json_has_duplicate_members(text).map_err(|_| ProtocolError("frame is not JSON"))? {
        return Err(ProtocolError("frame has duplicate object members"));
    }
    serde_json::from_str(text).map_err(|_| ProtocolError("frame is not JSON"))
}

pub(crate) fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value.clone())
        .map_err(|_| ProtocolError("known frame has an invalid shape"))
}

/// Each path names a field that is separately sanitized or matched against an
/// adapter-owned protocol constant. Everything else contributes dropped context.
pub(crate) fn fold_uninterpreted<C: Clone>(
    sink: &mut RedactingSink<'_, C>,
    value: &Value,
    interpreted: &[&[&str]],
) {
    let mut units = Vec::new();
    collect_uninterpreted(value, interpreted, &mut units);
    let markers: Vec<&str> = units
        .iter()
        .map(String::as_str)
        .filter(|unit| !trailing_credential_context(unit).is_empty())
        .collect();
    match markers.as_slice() {
        [] => {}
        [only] => sink.extend_dropped_context(only),
        _ => sink.suppress_remaining(),
    }
}

fn collect_uninterpreted(value: &Value, interpreted: &[&[&str]], units: &mut Vec<String>) {
    if interpreted.iter().any(|path| path.is_empty()) {
        return;
    }
    if let Value::Object(fields) = value {
        for (key, field) in fields {
            let nested: Vec<&[&str]> = interpreted
                .iter()
                .filter(|path| path.first().copied() == Some(key.as_str()))
                .map(|path| &path[1..])
                .collect();
            if nested.is_empty() {
                collect_units(field, units);
            } else {
                collect_uninterpreted(field, &nested, units);
            }
        }
    } else {
        collect_units(value, units);
    }
}

fn collect_units(value: &Value, units: &mut Vec<String>) {
    match value {
        Value::String(text) => units.push(text.clone()),
        Value::Array(items) => {
            let mut adjacent = String::new();
            for item in items {
                collect_array_element(item, &mut adjacent, units);
            }
            push_unit(adjacent, units);
        }
        Value::Object(fields) => {
            for field in fields.values() {
                collect_units(field, units);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_array_element(value: &Value, adjacent: &mut String, units: &mut Vec<String>) {
    match value {
        Value::String(text) => adjacent.push_str(text),
        Value::Array(items) => {
            for item in items {
                collect_array_element(item, adjacent, units);
            }
        }
        Value::Object(fields) => {
            push_unit(std::mem::take(adjacent), units);
            for field in fields.values() {
                collect_units(field, units);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn push_unit(unit: String, units: &mut Vec<String>) {
    if !unit.is_empty() {
        units.push(unit);
    }
}
