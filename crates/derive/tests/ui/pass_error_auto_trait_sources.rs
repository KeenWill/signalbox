use std::error::Error;
use signalbox_derive::OperatorError;

type Unpinned = Box<dyn Error + Unpin>;
type ThreadUnpinned = Box<dyn Error + Send + Sync + Unpin>;
trait DetailedError: Error {}
type Detailed = Box<dyn DetailedError + Unpin>;

#[derive(Debug, OperatorError)]
#[error("cause")]
struct Cause;
impl DetailedError for Cause {}

#[derive(Debug, OperatorError)]
enum Wrapped {
    #[error("{0}")]
    Unpinned(#[source] Unpinned),
    #[error("{0}")]
    ThreadUnpinned(#[source] ThreadUnpinned),
    #[error("{0}")]
    Detailed(#[source] Detailed),
}

fn main() {
    assert!(Wrapped::Unpinned(Box::new(Cause)).source().unwrap().is::<Cause>());
    assert!(Wrapped::ThreadUnpinned(Box::new(Cause)).source().unwrap().is::<Cause>());
    assert!(Wrapped::Detailed(Box::new(Cause)).source().unwrap().is::<Cause>());
}
