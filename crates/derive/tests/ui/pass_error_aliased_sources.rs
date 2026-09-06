use std::error::Error;
use signalbox_derive::OperatorError;

type BoxError = Box<dyn Error>;
type SendError = Box<dyn Error + Send>;
type SyncError = Box<dyn Error + Sync>;
type ThreadError = Box<dyn Error + Send + Sync>;
type ConcreteError = Box<Cause>;

#[derive(Debug, OperatorError)]
#[error("cause")]
struct Cause;

#[derive(Debug, OperatorError)]
enum Aliased {
    #[error("{0}")]
    Plain(#[source] BoxError),
    #[error("{0}")]
    Send(#[source] SendError),
    #[error("{0}")]
    Sync(#[source] SyncError),
    #[error("{0}")]
    Thread(#[source] ThreadError),
    #[error("{0}")]
    Concrete(#[source] ConcreteError),
}

fn main() {
    for failure in [Aliased::Plain(Box::new(Cause)), Aliased::Send(Box::new(Cause)), Aliased::Sync(Box::new(Cause)), Aliased::Thread(Box::new(Cause))] {
        assert!(failure.source().unwrap().is::<Cause>());
    }
    assert!(Aliased::Concrete(Box::new(Cause)).source().unwrap().is::<ConcreteError>());
}
