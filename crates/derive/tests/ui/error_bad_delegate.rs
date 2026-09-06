use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
enum Failure {
    #[error("bad delegate")]
    #[operator(delegate = absent, code = "failure")]
    Broken { detail: String },
}

fn main() {}
