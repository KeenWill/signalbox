//! Durable workspace records that scope operator-granted authority.
//!
//! A workspace is the unit an authority grant is scoped to. Before this module
//! the only thing available to scope a grant by was the root path itself, and a
//! path is not an identity: `/srv/workspace`, `/srv/workspace/.` and
//! `/srv/workspace/../workspace` are one directory under three spellings, so a
//! rule stated as "one grant per workspace" would have admitted three.
//!
//! The record fixes that by canonicalizing **once**, when the workspace is
//! minted, and identifying the result by a UUID from then on. Every later
//! comparison is between identities, never between paths, so no comparison can
//! be made to canonicalize a second time and no two spellings can survive as
//! two workspaces.

use std::error::Error;
use std::fmt;

/// Longest admitted workspace root in bytes.
///
/// The bound is smaller than a platform `PATH_MAX` because the durable record
/// indexes the root uniquely, and a PostgreSQL B-tree index tuple may not
/// exceed roughly a third of an 8 KiB page. A longer bound would let an
/// otherwise valid workspace fail on index insertion rather than here.
const MAX_WORKSPACE_ROOT_BYTES: usize = 1024;

/// Why one workspace root was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRootPathError {
    /// The path carried no bytes.
    Empty,
    /// The path carried an interior NUL.
    ContainsNull,
    /// The path exceeded its byte bound.
    TooLong {
        /// Byte length of the refused path.
        bytes: usize,
        /// Largest admitted byte length.
        maximum: usize,
    },
    /// The path did not begin at the filesystem root.
    NotAbsolute,
    /// The path carried an ASCII control byte.
    ContainsControlByte,
    /// The path was not in canonical form.
    ///
    /// An empty, `.`, or `..` component, or a trailing separator, means two
    /// spellings of one directory could be stored as two workspaces.
    NotCanonical,
    /// The path named the filesystem root itself.
    ///
    /// A workspace root must carry a final component, because the per-session
    /// derivation appends a suffix to it.
    NoFinalComponent,
}

impl fmt::Display for WorkspaceRootPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("workspace root was empty"),
            Self::ContainsNull => formatter.write_str("workspace root carried an interior NUL"),
            Self::TooLong { bytes, maximum } => write!(
                formatter,
                "workspace root used {bytes} bytes against a {maximum} byte bound"
            ),
            Self::NotAbsolute => formatter.write_str("workspace root was not absolute"),
            Self::ContainsControlByte => {
                formatter.write_str("workspace root carried an ASCII control byte")
            }
            Self::NotCanonical => formatter.write_str("workspace root was not in canonical form"),
            Self::NoFinalComponent => {
                formatter.write_str("workspace root named no final component")
            }
        }
    }
}

impl Error for WorkspaceRootPathError {}

/// One absolute workspace root in canonical form.
///
/// This type judges *form*, not the filesystem. Resolving symbolic links is
/// what `std::fs::canonicalize` does, it requires I/O, and this crate performs
/// none — so the resolution happens once at the boundary that mints a
/// workspace, exactly where the Git suites already canonicalize the root they
/// are constructed with, and its result is what reaches this constructor.
///
/// Restating the form here is what makes that single canonicalization binding.
/// A caller that skipped the resolution, or a row written by hand, would
/// otherwise put an aliasing spelling in the durable store and the identity
/// above it would silently stop being unique. The rules below are exactly the
/// properties a canonicalized absolute path has: it begins at the root, every
/// component is non-empty and is neither `.` nor `..`, and it carries no
/// trailing separator.
///
/// Accepted cost: a path whose *bytes* are canonical but whose components are
/// symbolic links is admitted, because only the filesystem can tell. The mint
/// boundary is where that is resolved, and this type cannot re-check it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRootPath(String);

impl WorkspaceRootPath {
    /// Admits one bounded absolute path already in canonical form.
    ///
    /// The control-byte test is ASCII-only, matching what the SQL predicate's
    /// `[:cntrl:]` class means under the `C` collation. A path may legitimately
    /// carry non-ASCII bytes, so narrowing to ASCII outright would be wrong;
    /// agreeing on which bytes are control is what keeps the durable store from
    /// holding a root this type would refuse.
    pub fn try_new(value: String) -> Result<Self, WorkspaceRootPathError> {
        if value.is_empty() {
            return Err(WorkspaceRootPathError::Empty);
        }
        if value.contains('\0') {
            return Err(WorkspaceRootPathError::ContainsNull);
        }
        if value.len() > MAX_WORKSPACE_ROOT_BYTES {
            return Err(WorkspaceRootPathError::TooLong {
                bytes: value.len(),
                maximum: MAX_WORKSPACE_ROOT_BYTES,
            });
        }
        let Some(components) = value.strip_prefix('/') else {
            return Err(WorkspaceRootPathError::NotAbsolute);
        };
        if value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(WorkspaceRootPathError::ContainsControlByte);
        }
        if components.is_empty() {
            return Err(WorkspaceRootPathError::NoFinalComponent);
        }
        if components
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err(WorkspaceRootPathError::NotCanonical);
        }
        Ok(Self(value))
    }

    /// Borrows the workspace root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned workspace root.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// How one workspace record came to exist.
///
/// The variants are the minting tiers the durable schema admits. They are
/// distinguished because they carry different authority: a workspace an
/// operator registered is a new authority scope and is a human act, while a
/// derived one is bookkeeping for a root the configured base already implies.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkspaceOrigin {
    /// An operator registered this workspace as a new authority scope.
    ///
    /// The record carries the durable command that registered it, so the
    /// human act behind a new scope is provable after the fact.
    OperatorRegistered,
    /// The daemon recorded a root its per-session derivation materialized.
    ///
    /// Authority still flows from the configured base and its fixed formula:
    /// this row is a record of what the formula produced, never an input to
    /// which roots the daemon may open. Nothing reads it to decide a binding.
    DaemonDerived,
}

impl WorkspaceOrigin {
    /// Returns whether this origin records a human act.
    ///
    /// Only an operator-registered workspace is a new authority scope, so this
    /// is the test a grant-minting path applies rather than matching the
    /// variant at each site.
    ///
    /// Both variants are enumerated rather than matched against one pattern.
    /// A further minting tier is committed, and an implicit wildcard would
    /// classify it as carrying no human act by default — the safe-looking
    /// answer that silently widens what may be minted without review.
    pub const fn is_operator_registered(self) -> bool {
        match self {
            Self::OperatorRegistered => true,
            Self::DaemonDerived => false,
        }
    }
}

/// One durable workspace an authority grant may be scoped to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
    id: crate::WorkspaceId,
    root: WorkspaceRootPath,
    origin: WorkspaceOrigin,
}

impl WorkspaceRecord {
    /// Binds one durable workspace identity to the root it was minted for.
    pub const fn new(
        id: crate::WorkspaceId,
        root: WorkspaceRootPath,
        origin: WorkspaceOrigin,
    ) -> Self {
        Self { id, root, origin }
    }

    /// Returns the durable workspace identity.
    pub const fn id(&self) -> crate::WorkspaceId {
        self.id
    }

    /// Borrows the canonical root this workspace was minted for.
    pub const fn root(&self) -> &WorkspaceRootPath {
        &self.root
    }

    /// Returns how this workspace came to exist.
    pub const fn origin(&self) -> WorkspaceOrigin {
        self.origin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/srv/signalbox/workspace";

    #[track_caller]
    fn assert_root_is_refused(candidate: &str, expected: &WorkspaceRootPathError) {
        assert_eq!(
            WorkspaceRootPath::try_new(candidate.to_owned()).as_ref(),
            Err(expected)
        );
    }

    #[track_caller]
    fn assert_root_is_admitted(candidate: &str) {
        assert_eq!(
            WorkspaceRootPath::try_new(candidate.to_owned())
                .as_ref()
                .map(WorkspaceRootPath::as_str),
            Ok(candidate)
        );
    }

    #[test]
    fn an_absolute_canonical_root_is_admitted() {
        let root = WorkspaceRootPath::try_new(ROOT.to_owned()).expect("root is admitted");

        assert_eq!(root.as_str(), ROOT);
    }

    #[test]
    fn a_root_carrying_non_ascii_bytes_is_admitted() {
        assert_root_is_admitted("/srv/proyectos/año/workspace");
        assert_root_is_admitted("/srv/workspace.sessions");
    }

    #[test]
    fn a_relative_root_is_refused() {
        assert_root_is_refused("workspace", &WorkspaceRootPathError::NotAbsolute);
        assert_root_is_refused("./workspace", &WorkspaceRootPathError::NotAbsolute);
    }

    /// The aliasing spellings a path-keyed grant would have admitted as
    /// distinct workspaces. Each one names the same directory as [`ROOT`], and
    /// each must die here rather than at a comparison that tried to normalize
    /// them later.
    #[test]
    fn a_root_aliasing_another_spelling_is_refused() {
        assert_root_is_refused(
            "/srv/signalbox/workspace/.",
            &WorkspaceRootPathError::NotCanonical,
        );
        assert_root_is_refused(
            "/srv/signalbox/nested/../workspace",
            &WorkspaceRootPathError::NotCanonical,
        );
        assert_root_is_refused(
            "/srv//signalbox/workspace",
            &WorkspaceRootPathError::NotCanonical,
        );
        assert_root_is_refused(
            "/srv/signalbox/workspace/",
            &WorkspaceRootPathError::NotCanonical,
        );
        assert_root_is_refused("/..", &WorkspaceRootPathError::NotCanonical);
    }

    #[test]
    fn the_filesystem_root_is_refused() {
        assert_root_is_refused("/", &WorkspaceRootPathError::NoFinalComponent);
    }

    #[test]
    fn a_root_carrying_a_control_byte_is_refused() {
        assert_root_is_refused(
            "/srv/work\u{7}space",
            &WorkspaceRootPathError::ContainsControlByte,
        );
    }

    #[test]
    fn an_empty_root_is_refused() {
        assert_eq!(
            WorkspaceRootPath::try_new(String::new()),
            Err(WorkspaceRootPathError::Empty)
        );
    }

    #[test]
    fn a_root_carrying_an_interior_null_is_refused() {
        assert_eq!(
            WorkspaceRootPath::try_new("/srv/work\0space".to_owned()),
            Err(WorkspaceRootPathError::ContainsNull)
        );
    }

    #[test]
    fn a_root_beyond_the_indexed_bound_is_refused() {
        let root = format!("/{}", "a".repeat(MAX_WORKSPACE_ROOT_BYTES));

        assert_eq!(
            WorkspaceRootPath::try_new(root),
            Err(WorkspaceRootPathError::TooLong {
                bytes: MAX_WORKSPACE_ROOT_BYTES + 1,
                maximum: MAX_WORKSPACE_ROOT_BYTES,
            })
        );
    }

    #[test]
    fn only_an_operator_registered_workspace_is_a_new_authority_scope() {
        assert!(WorkspaceOrigin::OperatorRegistered.is_operator_registered());
        assert!(!WorkspaceOrigin::DaemonDerived.is_operator_registered());
    }

    #[test]
    fn a_record_carries_the_identity_its_root_was_minted_under() {
        let id = crate::WorkspaceId::from_uuid(uuid::Uuid::from_u128(7));
        let root = WorkspaceRootPath::try_new(ROOT.to_owned()).expect("root is admitted");

        let record = WorkspaceRecord::new(id, root, WorkspaceOrigin::DaemonDerived);

        assert_eq!(record.id(), id);
        assert_eq!(record.root().as_str(), ROOT);
        assert_eq!(record.origin(), WorkspaceOrigin::DaemonDerived);
    }
}
