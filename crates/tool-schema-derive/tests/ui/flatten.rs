use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
struct Inner {
    #[tool_schema(description = "Nested value.")]
    value: String,
}

#[derive(ToolSchema)]
struct Flattened {
    #[serde(flatten)]
    #[tool_schema(description = "Flattened value.")]
    inner: Inner,
}

fn main() {}
