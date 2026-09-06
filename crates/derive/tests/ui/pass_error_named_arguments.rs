#![deny(warnings)]

use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("{value:width$}", value = value.to_uppercase(), width = 4)]
struct Override {
    value: String,
    width: usize,
}

#[derive(Debug, OperatorError)]
#[error("{value}", value = "fixed")]
struct Constant {
    value: &'static str,
}

fn main() {
    let failure = Override {
        value: "ok".to_owned(),
        width: 1,
    };
    assert_eq!(failure.to_string(), "OK  ");
    assert_eq!(failure.width, 1);
    let constant = Constant { value: "ignored" };
    assert_eq!(constant.to_string(), "fixed");
    assert_eq!(constant.value, "ignored");
}
