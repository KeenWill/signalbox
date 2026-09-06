//! Validated session placement and path-scoped conversation-read decisions.

use std::{
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    num::NonZeroU64,
};

use crate::{DurableCommandId, SessionId};

/// Maximum number of segments in one session placement path.
const MAX_SESSION_PLACEMENT_DEPTH: usize = 64;
/// Maximum ASCII bytes in one placement segment.
const MAX_SESSION_PLACEMENT_SEGMENT_BYTES: usize = 64;

/// One validated dotted placement path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionPlacementPath(String);

impl SessionPlacementPath {
    /// Maximum admitted path depth.
    pub const MAX_DEPTH: usize = MAX_SESSION_PLACEMENT_DEPTH;
    /// Maximum admitted ASCII bytes per segment.
    pub const MAX_SEGMENT_BYTES: usize = MAX_SESSION_PLACEMENT_SEGMENT_BYTES;
    /// Validates a nonempty dotted path of bounded ASCII label segments.
    pub fn try_new(value: String) -> Result<Self, SessionPlacementPathError> {
        if value.is_empty() {
            return Err(SessionPlacementPathError::Empty);
        }
        for (index, segment) in value.split('.').enumerate() {
            if index == MAX_SESSION_PLACEMENT_DEPTH {
                return Err(SessionPlacementPathError::TooDeep);
            }
            if segment.is_empty() {
                return Err(SessionPlacementPathError::EmptySegment);
            }
            if segment.len() > MAX_SESSION_PLACEMENT_SEGMENT_BYTES {
                return Err(SessionPlacementPathError::SegmentTooLong);
            }
            if !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(SessionPlacementPathError::MalformedSegment);
            }
        }
        Ok(Self(value))
    }

    /// Borrows the canonical dotted spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the number of path segments.
    pub fn depth(&self) -> usize {
        self.0.bytes().filter(|byte| *byte == b'.').count() + 1
    }

    fn parent_directory(&self) -> SessionPlacementDirectory {
        match self.0.rfind('.') {
            Some(boundary) => SessionPlacementDirectory(self.0[..=boundary].to_owned()),
            None => SessionPlacementDirectory(String::new()),
        }
    }
}

/// Why a placement path is not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPlacementPathError {
    Empty,
    EmptySegment,
    MalformedSegment,
    SegmentTooLong,
    TooDeep,
}

impl fmt::Display for SessionPlacementPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "session placement path is empty",
            Self::EmptySegment => "session placement path contains an empty segment",
            Self::MalformedSegment => "session placement segment is not an ASCII label",
            Self::SegmentTooLong => "session placement segment exceeds 64 bytes",
            Self::TooDeep => "session placement path exceeds 64 segments",
        })
    }
}

impl Error for SessionPlacementPathError {}

/// Explicit acknowledgement that a root-placed session has global read.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RootPlacementGlobalReadIntent {
    /// The caller explicitly accepts global conversation-read visibility.
    Acknowledged,
}

/// One session's opt-in placement decision.
///
/// Private fields keep the root-global-read acknowledgement inseparable from a
/// root path; callers construct each admitted shape through the methods below.
///
/// ```compile_fail
/// use signalbox_domain::{SessionPlacement, SessionPlacementPath};
///
/// fn forge_implicit_root(path: SessionPlacementPath) -> SessionPlacement {
///     SessionPlacement {
///         path: Some(path),
///         root_global_read_intent: false,
///     }
/// }
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionPlacement {
    path: Option<SessionPlacementPath>,
    root_global_read_intent: bool,
}

impl SessionPlacement {
    /// Constructs the legacy pathless decision with no read scope.
    pub const fn pathless() -> Self {
        Self {
            path: None,
            root_global_read_intent: false,
        }
    }

    /// Constructs a non-root placement and refuses an implicit global-read root.
    pub fn scoped(path: SessionPlacementPath) -> Result<Self, SessionPlacementError> {
        if path.depth() == 1 {
            Err(SessionPlacementError::RootRequiresGlobalReadIntent)
        } else {
            Ok(Self {
                path: Some(path),
                root_global_read_intent: false,
            })
        }
    }

    /// Constructs the loud root-global-read decision surface.
    pub fn root_global_read(
        path: SessionPlacementPath,
        intent: RootPlacementGlobalReadIntent,
    ) -> Result<Self, SessionPlacementError> {
        if path.depth() == 1 {
            let RootPlacementGlobalReadIntent::Acknowledged = intent;
            Ok(Self {
                path: Some(path),
                root_global_read_intent: true,
            })
        } else {
            Err(SessionPlacementError::GlobalReadIntentRequiresRoot)
        }
    }

    /// Borrows the dotted path, or `None` for legacy pathless behavior.
    pub fn path(&self) -> Option<&SessionPlacementPath> {
        self.path.as_ref()
    }

    /// Returns whether creation recorded explicit root-global-read intent.
    pub const fn records_root_global_read_intent(&self) -> bool {
        self.root_global_read_intent
    }

    /// Decides one cross-session read with exactly one path-prefix comparison.
    pub fn decide_cross_session_read(&self, target: &Self) -> SessionReadScopeDecision {
        let Some(requester_path) = self.path() else {
            return SessionReadScopeDecision::Allowed;
        };
        let directory = requester_path.parent_directory();
        let target_path = target.path().map_or("", SessionPlacementPath::as_str);
        if target_path.starts_with(directory.prefix()) {
            SessionReadScopeDecision::Allowed
        } else {
            SessionReadScopeDecision::Refused(SessionReadScopeRefusal {
                requesting_directory: directory,
                reason: SessionReadRefusalReason::OutsideRequestingDirectorySubtree,
            })
        }
    }
}

/// Why a placement decision is internally inconsistent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPlacementError {
    RootRequiresGlobalReadIntent,
    GlobalReadIntentRequiresRoot,
}

impl fmt::Display for SessionPlacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RootRequiresGlobalReadIntent => {
                "root session placement requires explicit global-read intent"
            }
            Self::GlobalReadIntentRequiresRoot => {
                "global-read intent is valid only for root session placement"
            }
        })
    }
}

impl Error for SessionPlacementError {}

/// The requesting session's parent directory; root is the empty prefix.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionPlacementDirectory(String);

impl SessionPlacementDirectory {
    /// Borrows the natural dotted directory spelling without a trailing dot.
    pub fn as_str(&self) -> &str {
        self.0.strip_suffix('.').unwrap_or(&self.0)
    }

    /// Borrows the comparison prefix, including a trailing dot when non-root.
    pub fn prefix(&self) -> &str {
        &self.0
    }

    /// Returns whether this is the root directory and therefore global read.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

/// Typed cross-session visibility outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionReadScopeDecision {
    Allowed,
    Refused(SessionReadScopeRefusal),
}

/// Evidence for a refused cross-session read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionReadScopeRefusal {
    requesting_directory: SessionPlacementDirectory,
    reason: SessionReadRefusalReason,
}

impl SessionReadScopeRefusal {
    pub const fn requesting_directory(&self) -> &SessionPlacementDirectory {
        &self.requesting_directory
    }

    pub const fn reason(&self) -> SessionReadRefusalReason {
        self.reason
    }
}

/// Closed reason for denying a scoped cross-session read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionReadRefusalReason {
    OutsideRequestingDirectorySubtree,
}

/// Positive immutable placement-history version.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionPlacementVersion(NonZeroU64);

impl SessionPlacementVersion {
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn as_u64(self) -> u64 {
        self.0.get()
    }

    pub const fn next(self) -> Option<Self> {
        match NonZeroU64::new(self.0.get().wrapping_add(1)) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// One immutable placement event selected as current.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VersionedSessionPlacement {
    version: SessionPlacementVersion,
    placement: SessionPlacement,
}

impl VersionedSessionPlacement {
    pub const fn initial(placement: SessionPlacement) -> Self {
        Self {
            version: SessionPlacementVersion::INITIAL,
            placement,
        }
    }

    pub const fn reconstitute(
        version: SessionPlacementVersion,
        placement: SessionPlacement,
    ) -> Self {
        Self { version, placement }
    }

    pub const fn version(&self) -> SessionPlacementVersion {
        self.version
    }
    pub const fn placement(&self) -> &SessionPlacement {
        &self.placement
    }
}

/// Durable command payload for appending one explicit placement update event.
#[derive(Clone, Debug)]
pub struct UpdateSessionPlacement {
    command_id: DurableCommandId,
    session: SessionId,
    expected_version: SessionPlacementVersion,
    replacement: SessionPlacement,
}

/// Comparison equality covers every caller field except the command identifier.
impl PartialEq for UpdateSessionPlacement {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.expected_version == other.expected_version
            && self.replacement == other.replacement
    }
}

impl Eq for UpdateSessionPlacement {}

impl Hash for UpdateSessionPlacement {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.expected_version.hash(state);
        self.replacement.hash(state);
    }
}

impl UpdateSessionPlacement {
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_version: SessionPlacementVersion,
        replacement: SessionPlacement,
    ) -> Self {
        Self {
            command_id,
            session,
            expected_version,
            replacement,
        }
    }
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }
    pub const fn session(&self) -> SessionId {
        self.session
    }
    pub const fn expected_version(&self) -> SessionPlacementVersion {
        self.expected_version
    }
    pub const fn replacement(&self) -> &SessionPlacement {
        &self.replacement
    }
}

/// Kind of one immutable placement-history event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionPlacementEventKind {
    Created,
    Updated,
}

/// One immutable placement event with its exact command provenance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionPlacementEvent {
    session: SessionId,
    kind: SessionPlacementEventKind,
    placement: VersionedSessionPlacement,
    prior_version: Option<SessionPlacementVersion>,
    command_id: DurableCommandId,
}

impl SessionPlacementEvent {
    pub fn created(
        session: SessionId,
        placement: SessionPlacement,
        command_id: DurableCommandId,
    ) -> Self {
        Self {
            session,
            kind: SessionPlacementEventKind::Created,
            placement: VersionedSessionPlacement::initial(placement),
            prior_version: None,
            command_id,
        }
    }

    pub fn updated(
        session: SessionId,
        prior_version: SessionPlacementVersion,
        placement: SessionPlacement,
        command_id: DurableCommandId,
    ) -> Option<Self> {
        Some(Self {
            session,
            kind: SessionPlacementEventKind::Updated,
            placement: VersionedSessionPlacement::reconstitute(prior_version.next()?, placement),
            prior_version: Some(prior_version),
            command_id,
        })
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }
    pub const fn kind(&self) -> SessionPlacementEventKind {
        self.kind
    }
    pub const fn placement(&self) -> &VersionedSessionPlacement {
        &self.placement
    }
    pub const fn prior_version(&self) -> Option<SessionPlacementVersion> {
        self.prior_version
    }
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }
}

/// Typed terminal result recorded for an explicit placement update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateSessionPlacementResult {
    Applied(UpdateSessionPlacementApplied),
    Rejected(UpdateSessionPlacementRejection),
}

/// Sealed evidence that an update command produced its matching update event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateSessionPlacementApplied {
    event: SessionPlacementEvent,
}

impl UpdateSessionPlacementApplied {
    pub fn try_new(command: &UpdateSessionPlacement, event: SessionPlacementEvent) -> Option<Self> {
        let matches_command = event.kind() == SessionPlacementEventKind::Updated
            && event.session() == command.session()
            && event.command_id() == command.command_id()
            && event.prior_version() == Some(command.expected_version())
            && event.placement().placement() == command.replacement()
            && event.placement().version() == command.expected_version().next()?;
        matches_command.then_some(Self { event })
    }

    pub const fn event(&self) -> &SessionPlacementEvent {
        &self.event
    }
}

/// Closed authoritative rejection reason for a placement update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateSessionPlacementRejectionKind {
    SessionNotFound,
    CurrentVersionMismatch,
    VersionExhausted,
}

/// Sealed evidence for one rejected placement update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateSessionPlacementRejection {
    session: SessionId,
    expected: SessionPlacementVersion,
    current: Option<SessionPlacementVersion>,
    kind: UpdateSessionPlacementRejectionKind,
}

impl UpdateSessionPlacementRejection {
    pub const fn session_not_found(command: &UpdateSessionPlacement) -> Self {
        Self {
            session: command.session(),
            expected: command.expected_version(),
            current: None,
            kind: UpdateSessionPlacementRejectionKind::SessionNotFound,
        }
    }

    pub const fn current_version_mismatch(
        command: &UpdateSessionPlacement,
        current: SessionPlacementVersion,
    ) -> Option<Self> {
        if current.as_u64() == command.expected_version().as_u64() {
            return None;
        }
        Some(Self {
            session: command.session(),
            expected: command.expected_version(),
            current: Some(current),
            kind: UpdateSessionPlacementRejectionKind::CurrentVersionMismatch,
        })
    }

    pub const fn version_exhausted(
        command: &UpdateSessionPlacement,
        current: SessionPlacementVersion,
    ) -> Option<Self> {
        if current.as_u64() != command.expected_version().as_u64() || current.next().is_some() {
            return None;
        }
        Some(Self {
            session: command.session(),
            expected: command.expected_version(),
            current: Some(current),
            kind: UpdateSessionPlacementRejectionKind::VersionExhausted,
        })
    }

    pub const fn session(self) -> SessionId {
        self.session
    }
    pub const fn expected_version(self) -> SessionPlacementVersion {
        self.expected
    }
    pub const fn current_version(self) -> Option<SessionPlacementVersion> {
        self.current
    }
    pub const fn kind(self) -> UpdateSessionPlacementRejectionKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLACEMENT_UPDATE_REPLACEMENT_PATH: &str = "projects.foo.session";

    fn scoped(value: &str) -> SessionPlacement {
        SessionPlacement::scoped(SessionPlacementPath::try_new(value.to_owned()).unwrap()).unwrap()
    }

    #[test]
    fn placement_path_rejects_empty_input() {
        assert_eq!(
            SessionPlacementPath::try_new(String::new()),
            Err(SessionPlacementPathError::Empty)
        );
    }

    #[test]
    fn placement_path_rejects_empty_segments() {
        assert_eq!(
            SessionPlacementPath::try_new("projects..foo".into()),
            Err(SessionPlacementPathError::EmptySegment)
        );
    }

    #[test]
    fn placement_path_rejects_malformed_segments() {
        assert_eq!(
            SessionPlacementPath::try_new("projects/foo".into()),
            Err(SessionPlacementPathError::MalformedSegment)
        );
    }

    #[test]
    fn placement_path_rejects_overlong_segments() {
        assert_eq!(
            SessionPlacementPath::try_new("x".repeat(65)),
            Err(SessionPlacementPathError::SegmentTooLong)
        );
    }

    #[test]
    fn placement_path_rejects_absurd_depth() {
        assert_eq!(
            SessionPlacementPath::try_new(vec!["x"; 65].join(".")),
            Err(SessionPlacementPathError::TooDeep)
        );
    }

    #[test]
    fn root_placement_requires_loud_global_read_intent() {
        let path = SessionPlacementPath::try_new("operator".into()).unwrap();
        assert_eq!(
            SessionPlacement::scoped(path.clone()),
            Err(SessionPlacementError::RootRequiresGlobalReadIntent)
        );
        assert!(
            SessionPlacement::root_global_read(path, RootPlacementGlobalReadIntent::Acknowledged)
                .unwrap()
                .records_root_global_read_intent()
        );
    }

    #[test]
    fn placement_errors_explain_the_required_root_intent_shape() {
        assert_eq!(
            SessionPlacementError::RootRequiresGlobalReadIntent.to_string(),
            "root session placement requires explicit global-read intent"
        );
        assert_eq!(
            SessionPlacementError::GlobalReadIntentRequiresRoot.to_string(),
            "global-read intent is valid only for root session placement"
        );
    }

    #[test]
    fn s36_prefix_rule_allows_siblings_and_descendants_but_not_ancestors_or_disjoint_paths() {
        let requester = scoped("projects.foo.reviews.pr123");
        let requesting_directory = requester.path().unwrap().parent_directory();
        let refusal = SessionReadScopeDecision::Refused(SessionReadScopeRefusal {
            requesting_directory,
            reason: SessionReadRefusalReason::OutsideRequestingDirectorySubtree,
        });
        assert_eq!(
            requester.decide_cross_session_read(&scoped("projects.foo.reviews.pr456")),
            SessionReadScopeDecision::Allowed
        );
        assert_eq!(
            requester.decide_cross_session_read(&scoped("projects.foo.reviews.pr456.followup")),
            SessionReadScopeDecision::Allowed
        );
        assert_eq!(
            requester.decide_cross_session_read(&scoped("projects.foo")),
            refusal
        );
        assert_eq!(
            requester.decide_cross_session_read(&scoped("projects.bar.reviews.pr1")),
            refusal
        );
    }

    #[test]
    fn pathless_keeps_legacy_reads_and_root_reads_every_placement() {
        let scoped = scoped("projects.foo.session");
        assert_eq!(
            SessionPlacement::pathless().decide_cross_session_read(&scoped),
            SessionReadScopeDecision::Allowed
        );
        let root = SessionPlacement::root_global_read(
            SessionPlacementPath::try_new("operator".into()).unwrap(),
            RootPlacementGlobalReadIntent::Acknowledged,
        )
        .unwrap();
        assert_eq!(
            root.decide_cross_session_read(&scoped),
            SessionReadScopeDecision::Allowed
        );
        assert_eq!(
            root.decide_cross_session_read(&SessionPlacement::pathless()),
            SessionReadScopeDecision::Allowed
        );
    }

    #[test]
    fn placement_update_event_preserves_prior_version_and_command_history() {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(1));
        let command = DurableCommandId::from_uuid(uuid::Uuid::from_u128(2));
        let replacement = scoped(PLACEMENT_UPDATE_REPLACEMENT_PATH);
        let event = SessionPlacementEvent::updated(
            session,
            SessionPlacementVersion::INITIAL,
            replacement.clone(),
            command,
        )
        .unwrap();

        assert_eq!(event.session(), session);
        assert_eq!(
            event.prior_version(),
            Some(SessionPlacementVersion::INITIAL)
        );
        assert_eq!(
            event.placement().version(),
            SessionPlacementVersion::try_from_u64(2)
                .expect("fixture successor version is positive")
        );
        assert_eq!(event.placement().placement(), &replacement);
        assert_eq!(event.command_id(), command);
    }

    const PLACEMENT_UPDATE_SESSION_SEED: u128 = 3;
    const PLACEMENT_UPDATE_COMMAND_SEED: u128 = 4;
    const FOREIGN_PLACEMENT_UPDATE_COMMAND_SEED: u128 = 5;

    struct PlacementUpdateFixture {
        session: SessionId,
        command_id: DurableCommandId,
        replacement: SessionPlacement,
        command: UpdateSessionPlacement,
    }

    fn placement_update_fixture() -> PlacementUpdateFixture {
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(PLACEMENT_UPDATE_SESSION_SEED));
        let command_id =
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(PLACEMENT_UPDATE_COMMAND_SEED));
        let replacement = scoped(PLACEMENT_UPDATE_REPLACEMENT_PATH);
        let command = UpdateSessionPlacement::new(
            command_id,
            session,
            SessionPlacementVersion::INITIAL,
            replacement.clone(),
        );
        PlacementUpdateFixture {
            session,
            command_id,
            replacement,
            command,
        }
    }

    #[test]
    fn placement_update_applied_evidence_rejects_a_created_event() {
        let fixture = placement_update_fixture();
        let created = SessionPlacementEvent::created(
            fixture.session,
            fixture.replacement,
            fixture.command_id,
        );

        assert_eq!(
            UpdateSessionPlacementApplied::try_new(&fixture.command, created),
            None
        );
    }

    #[test]
    fn placement_update_applied_evidence_rejects_foreign_command_provenance() {
        let fixture = placement_update_fixture();
        let foreign = SessionPlacementEvent::updated(
            fixture.session,
            SessionPlacementVersion::INITIAL,
            fixture.replacement,
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(
                FOREIGN_PLACEMENT_UPDATE_COMMAND_SEED,
            )),
        )
        .expect("fixture prior version has a successor");

        assert_eq!(
            UpdateSessionPlacementApplied::try_new(&fixture.command, foreign),
            None
        );
    }

    #[test]
    fn placement_update_applied_evidence_accepts_the_matching_update() {
        let fixture = placement_update_fixture();
        let applied = SessionPlacementEvent::updated(
            fixture.session,
            SessionPlacementVersion::INITIAL,
            fixture.replacement,
            fixture.command_id,
        )
        .expect("fixture prior version has a successor");

        assert_eq!(
            UpdateSessionPlacementApplied::try_new(&fixture.command, applied.clone())
                .expect("matching event produces sealed evidence")
                .event(),
            &applied
        );
    }

    #[test]
    fn placement_update_mismatch_evidence_rejects_the_expected_version() {
        let fixture = placement_update_fixture();

        assert_eq!(
            UpdateSessionPlacementRejection::current_version_mismatch(
                &fixture.command,
                SessionPlacementVersion::INITIAL,
            ),
            None
        );
    }

    #[test]
    fn placement_update_exhaustion_evidence_rejects_a_nonmaximum_version() {
        let fixture = placement_update_fixture();

        assert_eq!(
            UpdateSessionPlacementRejection::version_exhausted(
                &fixture.command,
                SessionPlacementVersion::INITIAL,
            ),
            None
        );
    }
}
