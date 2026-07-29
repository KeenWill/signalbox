use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
struct ContradictoryName {
    #[serde(rename = "wire_name")]
    #[tool_schema(description = "A named field.", name = "other_name")]
    rust_name: String,
}

fn main() {}
