use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("{type:match$}")]
struct Raw {
    r#type: &'static str,
    r#match: usize,
}

fn main() {
    let failure = Raw {
        r#type: "item",
        r#match: 6,
    };
    assert_eq!(failure.to_string(), "item  ");
}
