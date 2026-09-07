use std::{error::Error, fmt};
use signalbox_derive::OperatorError;

trait Custom {}
#[derive(Debug)]
struct Value;
impl Custom for Value {}
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("value") }
}
impl Error for Value {}
#[derive(Debug)]
struct Wrapper<T>(T);
impl<T: Custom> fmt::Display for Wrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("wrapped") }
}
#[derive(Debug, OperatorError)]
#[operator(display_bound = "T: Custom")]
#[error("{value}")]
struct Failure<T> { value: Wrapper<T> }
fn main() {
    let failure = Failure { value: Wrapper(Value) };
    assert_eq!(failure.to_string(), "wrapped");
    assert!(failure.source().is_none());
}
