use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
struct DuplicateAttribute {
    #[tool_schema(description = "The first spelling.", description = "The second.")]
    rust_name: String,
}

fn main() {}
