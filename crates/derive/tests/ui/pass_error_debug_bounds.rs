use std::{fmt, marker::PhantomData};
use signalbox_derive::OperatorError;

#[derive(Debug)]
struct DebugOnly;

#[derive(Debug, OperatorError)]
#[error("{value:?}")]
struct DebugField<T> { value: T }

#[derive(Debug, OperatorError)]
#[error("{:?}: {label}", value)]
struct Mixed<T, U> { value: T, label: U }

struct Wrapped<T>(PhantomData<T>);
impl<T> fmt::Debug for Wrapped<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("wrapped") }
}
struct NoFormatting;

#[derive(Debug, OperatorError)]
#[error("{value:?}")]
struct PerField<T> { value: Wrapped<T> }

#[derive(Debug, OperatorError)]
#[error("{value:x} {value:X} {value:b} {value:o} {value:e} {value:E}")]
struct Modes<T> { value: T }

fn main() {
    assert_eq!(DebugField { value: DebugOnly }.to_string(), "DebugOnly");
    assert_eq!(Mixed { value: DebugOnly, label: "detail" }.to_string(), "DebugOnly: detail");
    assert_eq!(PerField { value: Wrapped::<NoFormatting>(PhantomData) }.to_string(), "wrapped");
    assert_eq!(Modes { value: 10 }.to_string(), "a A 1010 12 1e1 1E1");
}
