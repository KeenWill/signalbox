use signalbox_tool_schema_derive::ToolSchema;

fn decode<'de, Deserializer>(deserializer: Deserializer) -> Result<String, Deserializer::Error>
where
    Deserializer: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer)
}

#[derive(serde::Deserialize, ToolSchema)]
struct CustomDecoderWithoutShape {
    #[serde(deserialize_with = "decode")]
    #[tool_schema(description = "Custom-decoded value.")]
    value: String,
}

fn main() {}
