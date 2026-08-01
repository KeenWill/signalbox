use std::{error::Error, fmt};

use signalbox_tools_workspace::WorkspaceRootError;

/// Static suite or injected-repository construction failure.
#[derive(Debug)]
pub enum LocalGitToolsConstructionError {
    /// Static tool name failed compilation.
    Name,
    /// Static schema failed compilation.
    Schema,
    /// Static detail failed construction.
    ErrorDetail,
    /// The fixed catalog unexpectedly contained a duplicate.
    Duplicate,
    /// The injected root was invalid.
    Root(WorkspaceRootError),
    /// The repository layout escaped or did not match the injected root.
    Repository,
}

impl fmt::Display for LocalGitToolsConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("local Git tool construction failed")
    }
}

impl Error for LocalGitToolsConstructionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Name | Self::Schema | Self::ErrorDetail | Self::Duplicate | Self::Repository => {
                None
            }
        }
    }
}
