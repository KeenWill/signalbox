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
        formatter.write_str(match self {
            Self::Name => "local Git tool static name is invalid",
            Self::Schema => "local Git tool static schema is invalid",
            Self::ErrorDetail => "local Git tool static error detail is invalid",
            Self::Duplicate => "local Git tool catalog is duplicated",
            Self::Root(_) => "local Git tool root is invalid",
            Self::Repository => "local Git tool repository layout is invalid",
        })
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
