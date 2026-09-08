use serde::de::DeserializeOwned;
use serde_json::Value;
use signalbox_model_runtime::{
    provider_json_has_duplicate_members, validate_provider_json_nesting,
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
