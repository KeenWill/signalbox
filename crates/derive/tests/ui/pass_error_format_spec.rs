use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("{value:width$.precision$}")]
struct Named {
    value: f64,
    width: usize,
    precision: usize,
}

#[derive(Debug, OperatorError)]
#[error("{0:1$.2$}")]
struct Tuple(f64, usize, usize);

#[derive(Debug, OperatorError)]
#[error("{value:0width$}")]
struct ZeroPadded {
    value: u32,
    width: usize,
}

fn main() {
    let named = Named {
        value: 1.25,
        width: 6,
        precision: 2,
    };
    assert_eq!(named.to_string(), "  1.25");
    assert_eq!(Tuple(1.25, 6, 2).to_string(), "  1.25");
    assert_eq!(ZeroPadded { value: 7, width: 3 }.to_string(), "007");
}
