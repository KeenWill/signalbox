use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[operator(default_class = CallerOrHubBug)]
enum Failure {
    #[error("missing code")]
    Missing,
}

fn main() {}
