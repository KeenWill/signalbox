use signalbox_tools_workspace::WorkspaceRootError;

#[derive(signalbox_derive::OperatorError)]
/// Static suite or injected-repository construction failure.
#[derive(Debug)]
pub enum LocalGitToolsConstructionError {
    #[error("local Git tool static name is invalid")]
    /// Static tool name failed compilation.
    Name,
    #[error("local Git tool static schema is invalid")]
    /// Static schema failed compilation.
    Schema,
    #[error("local Git tool static error detail is invalid")]
    /// Static detail failed construction.
    ErrorDetail,
    #[error("local Git tool catalog is duplicated")]
    /// The fixed catalog unexpectedly contained a duplicate.
    Duplicate,
    #[error("local Git tool root is invalid")]
    /// The injected root was invalid.
    Root(#[source] WorkspaceRootError),
    #[error("local Git tool repository layout is invalid")]
    /// The repository layout escaped or did not match the injected root.
    Repository,
}
