#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalGitFailure {
    Repository,
    Path,
    Operation,
    Encoding,
}
