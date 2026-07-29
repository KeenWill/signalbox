use signalbox_tool_schema_derive::ToolSchema;

#[derive(ToolSchema)]
struct MissingDescription {
    absent: String,
}

fn main() {}
