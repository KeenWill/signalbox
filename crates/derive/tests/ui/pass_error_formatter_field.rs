use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("{formatter}: {__signalbox_formatter}")]
struct Named {
    formatter: &'static str,
    __signalbox_formatter: &'static str,
}

#[derive(Debug, OperatorError)]
#[error(transparent)]
struct Transparent {
    formatter: String,
}

fn main() {
    let failure = Named {
        formatter: "first",
        __signalbox_formatter: "second",
    };
    assert_eq!(failure.to_string(), "first: second");
    assert_eq!(
        Transparent {
            formatter: "detail".to_owned()
        }
        .to_string(),
        "detail"
    );
}
