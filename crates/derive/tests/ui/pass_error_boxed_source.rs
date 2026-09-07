use std::error::Error;

use signalbox_derive::OperatorError;

#[derive(Debug, OperatorError)]
#[error("cause")]
struct Cause;

#[derive(Debug, OperatorError)]
#[error("failure: {source}")]
struct Boxed {
    #[source]
    source: Box<dyn Error>,
}

#[derive(Debug, OperatorError)]
#[error("failure: {0}")]
struct ThreadSafe(#[source] Box<dyn Error + Send + Sync>);

#[derive(Debug, OperatorError)]
#[error("failure: {0}")]
struct Concrete(#[source] Box<Cause>);

fn main() {
    let failure = Boxed {
        source: Box::new(Cause),
    };
    assert!(failure.source().unwrap().is::<Cause>());
    assert_eq!(failure.to_string(), "failure: cause");
    assert!(ThreadSafe(Box::new(Cause)).source().unwrap().is::<Cause>());
    assert!(
        Concrete(Box::new(Cause))
            .source()
            .unwrap()
            .is::<Box<Cause>>()
    );
}
