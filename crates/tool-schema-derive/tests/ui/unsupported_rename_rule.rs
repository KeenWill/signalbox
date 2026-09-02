use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
#[serde(rename_all = "Sentence case")]
struct UnsupportedRenameRule {
    #[tool_schema(description = "A named field.")]
    rust_name: String,
}

fn main() {}
