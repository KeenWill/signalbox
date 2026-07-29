use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
struct UnsupportedType {
    #[tool_schema(description = "An unsupported tuple.")]
    coordinates: (i64, i64),
}

fn main() {}
