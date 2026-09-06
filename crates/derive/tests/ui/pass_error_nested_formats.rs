#![deny(warnings)]
use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("{}", format_args!("{value}"))]
struct Nested { value: String }

#[derive(Debug, OperatorError)]
#[error("{}", format_args!("{value:?}"))]
struct NestedDebug<T> { value: T }

#[derive(Debug, OperatorError)]
#[error("{}", format!("{type:width$.precision$}"))]
struct NestedSpec { r#type: f64, width: usize, precision: usize }

#[derive(Debug, OperatorError)]
#[error("{}", format_args!("{value}", value = "explicit"))]
struct Explicit { value: String }

#[derive(Debug)]
struct DebugOnly;

fn main() {
    assert_eq!(Nested { value: "detail".into() }.to_string(), "detail");
    assert_eq!(NestedDebug { value: DebugOnly }.to_string(), "DebugOnly");
    assert_eq!(NestedSpec { r#type: 1.25, width: 6, precision: 1 }.to_string(), "   1.2");
    let explicit = Explicit { value: "unused".into() };
    assert_eq!(explicit.to_string(), "explicit");
    assert_eq!(explicit.value, "unused");
}
