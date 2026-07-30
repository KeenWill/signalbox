use signalbox_tool_schema_derive::ToolSchema;

struct Timestamp;

#[derive(ToolSchema)]
struct MissingSchemaImpl {
    #[tool_schema(description = "Unimplemented timestamp shape.")]
    timestamp: Timestamp,
}

fn main() {}
