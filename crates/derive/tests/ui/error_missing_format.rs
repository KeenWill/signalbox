use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
enum Failure {
    Missing,
}

fn main() {}
