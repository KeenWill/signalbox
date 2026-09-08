#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalGitFailure {
    Repository,
    Path,
    Operation,
    Encoding,
    Ambiguous,
}

impl LocalGitFailure {
    pub(super) const fn operation_class(self) -> Self {
        match self {
            Self::Ambiguous => Self::Ambiguous,
            _ => Self::Operation,
        }
    }

    pub(super) fn after_rollback(self, rollback: Result<(), Self>) -> Self {
        match rollback {
            Ok(()) => self,
            Err(_) => Self::Ambiguous,
        }
    }
}
