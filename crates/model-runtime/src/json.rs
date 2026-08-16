//! Canonical model-facing JSON independent of serde_json's map backend.

pub(crate) fn sort_object_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.sort_keys();
            for nested in object.values_mut() {
                sort_object_keys(nested);
            }
        }
        serde_json::Value::Array(values) => {
            for nested in values {
                sort_object_keys(nested);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}
