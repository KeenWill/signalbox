use signalbox_tool_schema_derive::ToolSchema;

#[derive(serde::Deserialize, ToolSchema)]
struct WireShapeWithoutCustomDecoder {
    #[tool_schema(description = "Text value.", with = u64)]
    value: String,
}

fn main() {}
