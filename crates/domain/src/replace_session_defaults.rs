//! Canonical session-defaults replacement command and terminal results.
//!
//! Session-defaults replacement (`docs/spec/sessions-and-transcript.md`)
//! admits one idempotent user command that installs a complete replacement
//! as the next immutable defaults version.
//! `docs/spec/identity-and-commands.md` fixes structural command equality
//! and typed applied-or-rejected results, while
//! `docs/spec/persistence-protocol.md` requires complete checked
//! reconstitution from durable facts. This module owns that pure domain
//! boundary. It performs no lookup, persistence, command claim, or
//! acknowledgement.

use crate::{
    DurableCommandId, ModelChangeAdjustment, ModelSettingsOverlay, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    VersionedSessionConfigurationDefaults,
};

/// The canonical caller payload for replacing one session's defaults.
///
/// The payload contains exactly the user-global command identity, target
/// session, expected current defaults version, and complete replacement
/// defaults. It contains no server-derived current version or installed
/// version.
///
/// # Comparison payload
///
/// Structural equality and hashing exclude `command_id` and server-normalized
/// settings/adjustments. They include the target, expected version, caller's
/// model/tool/prompt choices, and exact caller settings overlay. The command
/// identifier is looked up separately at the user-global durable-command
/// boundary.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaults {
    command_id: DurableCommandId,
    session: SessionId,
    expected_current_version: SessionConfigurationDefaultsVersion,
    replacement: SessionConfigurationDefaults,
    caller_model_settings: ModelSettingsOverlay,
    model_settings_adjustments: Box<[ModelChangeAdjustment]>,
}

impl ReplaceSessionDefaults {
    /// Constructs the complete canonical caller payload.
    pub fn new(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
    ) -> Self {
        let caller_model_settings = replacement.model_settings().precedence().session();
        Self::with_model_settings(
            command_id,
            session,
            expected_current_version,
            replacement,
            caller_model_settings,
        )
    }

    /// Constructs the payload while retaining the caller's exact settings
    /// overlay separately from any automatic installed adjustment.
    pub fn with_model_settings(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
    ) -> Self {
        Self::with_model_settings_adjustments(
            command_id,
            session,
            expected_current_version,
            replacement,
            caller_model_settings,
            Vec::new(),
        )
    }

    /// Constructs the payload with server-derived automatic adjustment
    /// evidence. Adjustments are excluded from caller-payload equality.
    pub fn with_model_settings_adjustments(
        command_id: DurableCommandId,
        session: SessionId,
        expected_current_version: SessionConfigurationDefaultsVersion,
        replacement: SessionConfigurationDefaults,
        caller_model_settings: ModelSettingsOverlay,
        model_settings_adjustments: Vec<ModelChangeAdjustment>,
    ) -> Self {
        Self {
            command_id,
            session,
            expected_current_version,
            replacement,
            caller_model_settings,
            model_settings_adjustments: model_settings_adjustments.into_boxed_slice(),
        }
    }

    /// Returns the user-global durable command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session identity.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the version the caller expects to be current.
    pub const fn expected_current_version(&self) -> SessionConfigurationDefaultsVersion {
        self.expected_current_version
    }

    /// Borrows the complete replacement defaults.
    pub const fn replacement(&self) -> &SessionConfigurationDefaults {
        &self.replacement
    }

    /// Returns the caller's provenance-preserving session contribution.
    pub const fn caller_model_settings(&self) -> ModelSettingsOverlay {
        self.caller_model_settings
    }

    /// Borrows ordered automatic adjustments derived for the installed epoch.
    pub fn model_settings_adjustments(&self) -> &[ModelChangeAdjustment] {
        &self.model_settings_adjustments
    }

    /// Prepares the authoritative rejection for a target proven absent by the
    /// transaction's complete session lookup.
    ///
    /// This value is a pre-commit candidate. It does not claim the command
    /// identity or prove that the rejection was recorded.
    pub const fn prepare_session_not_found(self) -> PreparedReplaceSessionDefaults {
        let result = ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::SessionNotFound(
                ReplaceSessionDefaultsSessionNotFound {
                    session: self.session,
                },
            ),
        );
        PreparedReplaceSessionDefaults {
            command: self,
            result,
        }
    }

    /// Prepares an applied or authoritative-rejection result against one
    /// complete current session snapshot.
    ///
    /// The committing transaction must still compare-and-set the expected
    /// version. A mismatched supplied session is a caller/adapter correlation
    /// error rather than a terminal command rejection.
    pub fn prepare_against(
        self,
        current: &Session,
    ) -> Result<PreparedReplaceSessionDefaults, ReplaceSessionDefaultsPreparationError> {
        if self.session != current.id() {
            return Err(ReplaceSessionDefaultsPreparationError {
                command: Box::new(self),
                provided_session: current.id(),
            });
        }

        let current_defaults = current.current_configuration_defaults();
        let current_version = current_defaults.version();

        if self.expected_current_version != current_version {
            let result = ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(
                    ReplaceSessionDefaultsCurrentVersionMismatch {
                        session: self.session,
                        expected: self.expected_current_version,
                        current: current_version,
                    },
                ),
            );
            return Ok(PreparedReplaceSessionDefaults {
                command: self,
                result,
            });
        }

        let Some(installed) = current_defaults.replace(self.replacement.clone()) else {
            let result = ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(
                    ReplaceSessionDefaultsVersionExhausted {
                        session: self.session,
                        current: current_version,
                    },
                ),
            );
            return Ok(PreparedReplaceSessionDefaults {
                command: self,
                result,
            });
        };

        let result = ReplaceSessionDefaultsResult::Applied(ReplaceSessionDefaultsAppliedResult {
            session: self.session,
            installed,
        });
        Ok(PreparedReplaceSessionDefaults {
            command: self,
            result,
        })
    }
}

/// docs/spec/identity-and-commands.md comparison equality covers every
/// caller field except the command identifier itself.
impl PartialEq for ReplaceSessionDefaults {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.expected_current_version == other.expected_current_version
            && self.replacement.model() == other.replacement.model()
            && self.replacement.dangerous_tool_auto_approval()
                == other.replacement.dangerous_tool_auto_approval()
            && self.replacement.system_prompt() == other.replacement.system_prompt()
            && self.caller_model_settings == other.caller_model_settings
    }
}

impl Eq for ReplaceSessionDefaults {}

impl std::hash::Hash for ReplaceSessionDefaults {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.expected_current_version.hash(state);
        self.replacement.model().hash(state);
        self.replacement.dangerous_tool_auto_approval().hash(state);
        self.replacement.system_prompt().hash(state);
        self.caller_model_settings.hash(state);
    }
}

/// The terminal typed result for one defaults-replacement command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ReplaceSessionDefaultsResult {
    /// The replacement was installed as the next immutable version.
    Applied(ReplaceSessionDefaultsAppliedResult),
    /// Authoritative state rejected the command without installing a version.
    Rejected(ReplaceSessionDefaultsRejectedResult),
}

/// The terminal applied result recorded with the replacement effects.
///
/// Private fields and the absence of a public constructor prevent a raw
/// session/version tuple from claiming application. Live preparation and
/// complete reconstitution are the only producers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionDefaultsAppliedResult {
    session: SessionId,
    installed: VersionedSessionConfigurationDefaults,
}

impl ReplaceSessionDefaultsAppliedResult {
    /// Returns the session whose current defaults pointer advanced.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the exact complete immutable defaults version installed.
    pub const fn installed(&self) -> &VersionedSessionConfigurationDefaults {
        &self.installed
    }
}

/// A closed authoritative rejection result for defaults replacement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReplaceSessionDefaultsRejectedResult {
    /// The target session did not exist in the handling transaction.
    SessionNotFound(ReplaceSessionDefaultsSessionNotFound),
    /// The authoritative current version differed from the caller's expected
    /// version.
    CurrentVersionMismatch(ReplaceSessionDefaultsCurrentVersionMismatch),
    /// The current ordinal had no representable successor.
    VersionExhausted(ReplaceSessionDefaultsVersionExhausted),
}

/// Typed facts for an authoritative missing-session rejection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionDefaultsSessionNotFound {
    session: SessionId,
}

impl ReplaceSessionDefaultsSessionNotFound {
    /// Returns the absent target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }
}

/// Typed facts for an authoritative current-version mismatch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionDefaultsCurrentVersionMismatch {
    session: SessionId,
    expected: SessionConfigurationDefaultsVersion,
    current: SessionConfigurationDefaultsVersion,
}

impl ReplaceSessionDefaultsCurrentVersionMismatch {
    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the version the caller expected to be current.
    pub const fn expected(&self) -> SessionConfigurationDefaultsVersion {
        self.expected
    }

    /// Returns the authoritative version observed by the handling transaction.
    pub const fn current(&self) -> SessionConfigurationDefaultsVersion {
        self.current
    }
}

/// Typed facts for an exhausted defaults-version ordinal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReplaceSessionDefaultsVersionExhausted {
    session: SessionId,
    current: SessionConfigurationDefaultsVersion,
}

impl ReplaceSessionDefaultsVersionExhausted {
    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the authoritative current version with no successor.
    pub const fn current(&self) -> SessionConfigurationDefaultsVersion {
        self.current
    }
}

/// One sealed pre-commit handling candidate.
///
/// The command and terminal result are coupled so persistence cannot
/// accidentally record a result prepared for another payload. This value is
/// not evidence that the command or its effects committed.
#[derive(Clone, Debug)]
pub struct PreparedReplaceSessionDefaults {
    command: ReplaceSessionDefaults,
    result: ReplaceSessionDefaultsResult,
}

impl PreparedReplaceSessionDefaults {
    /// Borrows the exact canonical command.
    pub const fn command(&self) -> &ReplaceSessionDefaults {
        &self.command
    }

    /// Borrows the exact terminal result to record atomically.
    pub const fn result(&self) -> &ReplaceSessionDefaultsResult {
        &self.result
    }

    /// Consumes the candidate into its correlated transaction inputs.
    pub fn into_parts(self) -> (ReplaceSessionDefaults, ReplaceSessionDefaultsResult) {
        (self.command, self.result)
    }
}

/// A supplied current session belonged to another command target.
///
/// This is an adapter/caller correlation failure, not an authoritative
/// command rejection, and therefore does not claim the command identifier.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaultsPreparationError {
    command: Box<ReplaceSessionDefaults>,
    provided_session: SessionId,
}

impl ReplaceSessionDefaultsPreparationError {
    /// Borrows the unchanged canonical command.
    pub const fn command(&self) -> &ReplaceSessionDefaults {
        &self.command
    }

    /// Returns the different session supplied for preparation.
    pub const fn provided_session(&self) -> SessionId {
        self.provided_session
    }

    /// Returns both unchanged correlation inputs.
    pub fn into_parts(self) -> (ReplaceSessionDefaults, SessionId) {
        (*self.command, self.provided_session)
    }
}

#[derive(Clone, Debug)]
enum ReplaceSessionDefaultsReconstitutionFacts {
    Applied {
        result_session: SessionId,
        result_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    },
    RejectedSessionNotFound {
        result_session: SessionId,
    },
    RejectedCurrentVersionMismatch {
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
    },
    RejectedVersionExhausted {
        result_session: SessionId,
        result_current: SessionConfigurationDefaultsVersion,
    },
}

/// Complete checked inputs for reconstructing one recorded replacement.
///
/// Constructors accept only checked domain values. Persistence remains
/// responsible for decoding record encodings and selecting the constructor
/// matching the closed stored result discriminator.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaultsReconstitutionInput {
    command: ReplaceSessionDefaults,
    facts: ReplaceSessionDefaultsReconstitutionFacts,
}

impl ReplaceSessionDefaultsReconstitutionInput {
    /// Supplies the complete immutable result and installed-version facts for
    /// an applied command.
    ///
    /// The mutable current-defaults pointer is deliberately absent. A later
    /// successful command may advance it without invalidating this historical
    /// receipt.
    pub const fn applied(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_version: SessionConfigurationDefaultsVersion,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command,
            facts: ReplaceSessionDefaultsReconstitutionFacts::Applied {
                result_session,
                result_version,
                defaults_session,
                defaults_version,
                defaults,
            },
        }
    }

    /// Supplies the recorded target for a missing-session rejection.
    pub const fn rejected_session_not_found(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
    ) -> Self {
        Self {
            command,
            facts: ReplaceSessionDefaultsReconstitutionFacts::RejectedSessionNotFound {
                result_session,
            },
        }
    }

    /// Supplies the complete typed facts for a current-version rejection.
    pub const fn rejected_current_version_mismatch(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
    ) -> Self {
        Self {
            command,
            facts: ReplaceSessionDefaultsReconstitutionFacts::RejectedCurrentVersionMismatch {
                result_session,
                result_expected,
                result_current,
            },
        }
    }

    /// Supplies the complete typed facts for version exhaustion.
    pub const fn rejected_version_exhausted(
        command: ReplaceSessionDefaults,
        result_session: SessionId,
        result_current: SessionConfigurationDefaultsVersion,
    ) -> Self {
        Self {
            command,
            facts: ReplaceSessionDefaultsReconstitutionFacts::RejectedVersionExhausted {
                result_session,
                result_current,
            },
        }
    }

    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &ReplaceSessionDefaults {
        &self.command
    }

    /// Reconstructs the complete recorded handling without I/O, replaying an
    /// effect, or claiming command authority.
    pub fn reconstitute(
        self,
    ) -> Result<ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaultsReconstitutionError>
    {
        if let Some(failure) = self.validation_failure() {
            return Err(ReplaceSessionDefaultsReconstitutionError {
                input: Box::new(self),
                failure,
            });
        }

        let result = match self.facts {
            ReplaceSessionDefaultsReconstitutionFacts::Applied {
                result_session,
                result_version,
                defaults,
                ..
            } => ReplaceSessionDefaultsResult::Applied(ReplaceSessionDefaultsAppliedResult {
                session: result_session,
                installed: VersionedSessionConfigurationDefaults::reconstitute(
                    result_version,
                    defaults,
                ),
            }),
            ReplaceSessionDefaultsReconstitutionFacts::RejectedSessionNotFound {
                result_session,
            } => ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(
                    ReplaceSessionDefaultsSessionNotFound {
                        session: result_session,
                    },
                ),
            ),
            ReplaceSessionDefaultsReconstitutionFacts::RejectedCurrentVersionMismatch {
                result_session,
                result_expected,
                result_current,
            } => ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(
                    ReplaceSessionDefaultsCurrentVersionMismatch {
                        session: result_session,
                        expected: result_expected,
                        current: result_current,
                    },
                ),
            ),
            ReplaceSessionDefaultsReconstitutionFacts::RejectedVersionExhausted {
                result_session,
                result_current,
            } => ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(
                    ReplaceSessionDefaultsVersionExhausted {
                        session: result_session,
                        current: result_current,
                    },
                ),
            ),
        };

        Ok(ReconstitutedReplaceSessionDefaults {
            command: self.command,
            result,
        })
    }

    /// Applies every fail-closed agreement check without consuming the input.
    fn validation_failure(&self) -> Option<ReplaceSessionDefaultsReconstitutionFailure> {
        match &self.facts {
            ReplaceSessionDefaultsReconstitutionFacts::Applied {
                result_session,
                result_version,
                defaults_session,
                defaults_version,
                defaults,
            } => {
                if *result_session != self.command.session {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultSessionMismatch,
                    );
                }
                if *defaults_session != self.command.session {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::DefaultsSessionMismatch,
                    );
                }
                if result_version != defaults_version {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultVersionMismatch,
                    );
                }
                if self.command.expected_current_version.checked_next() != Some(*defaults_version) {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::InstalledVersionIsNotSuccessor,
                    );
                }
                if *defaults != self.command.replacement {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::StoredDefaultsMismatch,
                    );
                }
                None
            }
            ReplaceSessionDefaultsReconstitutionFacts::RejectedSessionNotFound {
                result_session,
            } => {
                if *result_session != self.command.session {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultSessionMismatch,
                    );
                }
                None
            }
            ReplaceSessionDefaultsReconstitutionFacts::RejectedCurrentVersionMismatch {
                result_session,
                result_expected,
                result_current,
            } => {
                if *result_session != self.command.session {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultSessionMismatch,
                    );
                }
                if *result_expected != self.command.expected_current_version {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultExpectedVersionMismatch,
                    );
                }
                if result_current == result_expected {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::RejectedVersionsAreEqual,
                    );
                }
                None
            }
            ReplaceSessionDefaultsReconstitutionFacts::RejectedVersionExhausted {
                result_session,
                result_current,
            } => {
                if *result_session != self.command.session {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultSessionMismatch,
                    );
                }
                if *result_current != self.command.expected_current_version {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultExpectedVersionMismatch,
                    );
                }
                if result_current.checked_next().is_some() {
                    return Some(
                        ReplaceSessionDefaultsReconstitutionFailure::ResultVersionIsNotExhausted,
                    );
                }
                None
            }
        }
    }
}

/// Why typed durable facts cannot reconstruct a recorded replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceSessionDefaultsReconstitutionFailure {
    /// The terminal result names a different target session.
    ResultSessionMismatch,
    /// The installed defaults record belongs to another session.
    DefaultsSessionMismatch,
    /// The applied result and installed record name different versions.
    ResultVersionMismatch,
    /// The installed version is not the checked successor of the expected
    /// version.
    InstalledVersionIsNotSuccessor,
    /// The stored installed value differs from the command replacement.
    StoredDefaultsMismatch,
    /// A rejected result repeats a different expected version from the
    /// command.
    ResultExpectedVersionMismatch,
    /// A recorded mismatch result claims equal expected and current versions.
    RejectedVersionsAreEqual,
    /// A recorded exhaustion result names a version with a successor.
    ResultVersionIsNotExhausted,
}

/// A failed reconstitution retaining every typed input unchanged.
#[derive(Clone, Debug)]
pub struct ReplaceSessionDefaultsReconstitutionError {
    input: Box<ReplaceSessionDefaultsReconstitutionInput>,
    failure: ReplaceSessionDefaultsReconstitutionFailure,
}

impl ReplaceSessionDefaultsReconstitutionError {
    /// Returns why the complete projection could not be reconstructed.
    pub const fn failure(&self) -> ReplaceSessionDefaultsReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &ReplaceSessionDefaultsReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        ReplaceSessionDefaultsReconstitutionInput,
        ReplaceSessionDefaultsReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete recorded replacement reconstructed from matching durable
/// facts.
///
/// This value authorizes no insert, pointer update, repair, or command claim.
#[derive(Clone, Debug)]
pub struct ReconstitutedReplaceSessionDefaults {
    command: ReplaceSessionDefaults,
    result: ReplaceSessionDefaultsResult,
}

impl ReconstitutedReplaceSessionDefaults {
    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &ReplaceSessionDefaults {
        &self.command
    }

    /// Borrows the reconstructed terminal result.
    pub const fn result(&self) -> &ReplaceSessionDefaultsResult {
        &self.result
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use expect_test::expect;

    use super::{
        ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaults,
        ReplaceSessionDefaultsReconstitutionError, ReplaceSessionDefaultsReconstitutionFailure,
        ReplaceSessionDefaultsReconstitutionInput, ReplaceSessionDefaultsRejectedResult,
        ReplaceSessionDefaultsResult,
    };
    use signalbox_expect_table::table;

    use crate::test_support::{command_id, direct, session_id};
    use crate::{
        DangerousToolAutoApproval, FastModeOverlay, FastModeSupport, ModelCapabilities,
        ModelChangeAdjustment, ModelSelectionRequest, ModelSettingsOverlay,
        ModelSettingsPrecedence, ReasoningLevel, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionReconstitutionInput, SettingOverlay, TranscriptAncestry,
    };

    fn defaults(value: u128) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(value)))
    }

    fn version(value: u64) -> SessionConfigurationDefaultsVersion {
        SessionConfigurationDefaultsVersion::try_from_u64(value)
            .expect("test versions are positive")
    }

    fn session(id: crate::SessionId, current: u64) -> crate::Session {
        SessionReconstitutionInput::new(
            id,
            id,
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            id,
            version(current),
            id,
            version(current),
            defaults(1),
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: id,
                current_pointer_version: crate::SessionPlacementVersion::INITIAL,
                selected_event_session: id,
                selected_event: crate::VersionedSessionPlacement::initial(
                    crate::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("test session projection is complete")
    }

    /// The canonical replacement command for `target`, expecting the given
    /// current version; the command identity and replacement defaults are
    /// canonical, and tests that care about them assert through the command's
    /// own accessors.
    fn command_expecting(target: crate::SessionId, expected: u64) -> ReplaceSessionDefaults {
        ReplaceSessionDefaults::new(command_id(1), target, version(expected), defaults(2))
    }

    /// The complete stored facts backing one applied replacement, mirroring
    /// [`ReplaceSessionDefaultsReconstitutionInput::applied`] field for field
    /// so a test perturbs exactly the named facts it cares about
    /// (TS-4, TS-5).
    #[derive(Clone)]
    struct AppliedFacts {
        result_session: crate::SessionId,
        result_version: SessionConfigurationDefaultsVersion,
        defaults_session: crate::SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    }

    impl AppliedFacts {
        /// The canonical stored facts matching an applied `command`: the
        /// command's target session owns the checked-successor version
        /// holding exactly the command's replacement.
        fn matching(command: &ReplaceSessionDefaults) -> Self {
            let installed = command
                .expected_current_version()
                .checked_next()
                .expect("test expected versions have a successor");
            Self {
                result_session: command.session(),
                result_version: installed,
                defaults_session: command.session(),
                defaults_version: installed,
                defaults: command.replacement().clone(),
            }
        }

        fn reconstitute(
            self,
            command: ReplaceSessionDefaults,
        ) -> Result<ReconstitutedReplaceSessionDefaults, ReplaceSessionDefaultsReconstitutionError>
        {
            ReplaceSessionDefaultsReconstitutionInput::applied(
                command,
                self.result_session,
                self.result_version,
                self.defaults_session,
                self.defaults_version,
                self.defaults,
            )
            .reconstitute()
        }
    }

    /// S01 / INV-012: comparison excludes command identity and includes the
    /// stable target, expected version, and caller-owned replacement fields.
    #[test]
    fn s01_inv012_comparison_payload_is_structural() {
        let target = session_id(1);
        let baseline = ReplaceSessionDefaults::new(command_id(1), target, version(1), defaults(2));
        let different_command_id =
            ReplaceSessionDefaults::new(command_id(2), target, version(1), defaults(2));
        let different_target =
            ReplaceSessionDefaults::new(command_id(1), session_id(2), version(1), defaults(2));
        let different_expected_version =
            ReplaceSessionDefaults::new(command_id(1), target, version(2), defaults(2));
        let different_replacement =
            ReplaceSessionDefaults::new(command_id(1), target, version(1), defaults(3));

        assert_eq!(baseline, different_command_id);
        assert_ne!(baseline, different_target);
        assert_ne!(baseline, different_expected_version);
        assert_ne!(baseline, different_replacement);
    }

    /// S37 / INV-012 / INV-053: server-derived adjustment evidence travels
    /// with a new command but does not alter caller-payload replay equality.
    #[test]
    fn s37_inv012_inv053_adjustment_evidence_is_not_caller_payload() {
        let target = session_id(1);
        let adjustment = ModelChangeAdjustment::ReasoningLevelClamped {
            from: ReasoningLevel::High,
            to: ReasoningLevel::Low,
        };
        let with_adjustment = ReplaceSessionDefaults::with_model_settings_adjustments(
            command_id(1),
            target,
            version(1),
            defaults(2),
            ModelSettingsOverlay::inherit_all(),
            vec![adjustment],
        );
        let replay = ReplaceSessionDefaults::with_model_settings(
            command_id(1),
            target,
            version(1),
            defaults(2),
            ModelSettingsOverlay::inherit_all(),
        );

        assert_eq!(with_adjustment, replay);
        assert_eq!(with_adjustment.model_settings_adjustments(), [adjustment]);
    }

    /// S37 / INV-012 / INV-053: replay equality follows the stable caller
    /// overlay instead of a server-normalized installed settings snapshot.
    #[test]
    fn s37_inv012_inv053_server_normalized_settings_are_not_caller_payload() {
        let target = session_id(1);
        let selection = direct(2);
        let capabilities = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::Low, ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        );
        let low = capabilities
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::Low),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the fixture level is supported");
        let high = capabilities
            .validate_precedence(
                selection,
                ModelSettingsPrecedence::new(
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::new(
                        SettingOverlay::Value(ReasoningLevel::High),
                        FastModeOverlay::Inherit,
                        SettingOverlay::Inherit,
                    ),
                    ModelSettingsOverlay::inherit_all(),
                    ModelSettingsOverlay::inherit_all(),
                ),
            )
            .expect("the fixture level is supported");
        let low_replacement = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Direct(selection),
            DangerousToolAutoApproval::Disabled,
            None,
            low,
        )
        .expect("the low settings match the direct replacement");
        let high_replacement = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Direct(selection),
            DangerousToolAutoApproval::Disabled,
            None,
            high,
        )
        .expect("the high settings match the direct replacement");
        assert_ne!(
            low_replacement.model_settings(),
            high_replacement.model_settings(),
        );
        let caller = ModelSettingsOverlay::inherit_all();
        let recorded = ReplaceSessionDefaults::with_model_settings(
            command_id(1),
            target,
            version(1),
            low_replacement,
            caller,
        );
        let replay = ReplaceSessionDefaults::with_model_settings(
            command_id(1),
            target,
            version(1),
            high_replacement,
            caller,
        );

        assert_eq!(recorded, replay);
    }

    /// S01 / INV-008: matching current state installs one complete immutable
    /// successor without changing the source session snapshot.
    #[test]
    fn s01_inv008_matching_version_prepares_complete_successor() {
        let target = session_id(1);
        let current = session(target, 1);
        let replacement = command_expecting(target, 1);
        let prepared = replacement
            .clone()
            .prepare_against(&current)
            .expect("the supplied session matches");

        let ReplaceSessionDefaultsResult::Applied(applied) = prepared.result() else {
            panic!("matching authoritative state must prepare application");
        };
        assert_eq!(applied.session(), target);
        assert_eq!(applied.installed().version(), version(2));
        assert_eq!(applied.installed().defaults(), replacement.replacement());
        assert_eq!(
            current.current_configuration_defaults().version(),
            version(1)
        );
    }

    /// S01 / INV-008 / INV-012: stale current state is a typed terminal
    /// rejection retaining both compared versions.
    #[test]
    fn s01_inv008_inv012_stale_version_prepares_authoritative_rejection() {
        let target = session_id(1);
        let prepared = command_expecting(target, 1)
            .prepare_against(&session(target, 2))
            .expect("the supplied session matches");

        let ReplaceSessionDefaultsResult::Rejected(
            ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(mismatch),
        ) = prepared.result()
        else {
            panic!("stale current state must prepare a typed rejection");
        };
        assert_eq!(mismatch.session(), target);
        assert_eq!(mismatch.expected(), version(1));
        assert_eq!(mismatch.current(), version(2));
    }

    /// S01 / INV-012: absence and ordinal exhaustion are distinct
    /// authoritative results, while a cross-wired session is not.
    #[test]
    fn s01_inv012_missing_exhausted_and_cross_wired_are_distinct() {
        let target = session_id(1);
        let missing = command_expecting(target, 1).prepare_session_not_found();
        assert!(matches!(
            missing.result(),
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(_)
            )
        ));

        let exhausted_version = SessionConfigurationDefaultsVersion::try_from_u64(u64::MAX)
            .expect("the maximum positive ordinal is valid");
        let exhausted_command =
            ReplaceSessionDefaults::new(command_id(2), target, exhausted_version, defaults(2));
        let exhausted = exhausted_command
            .prepare_against(&session(target, u64::MAX))
            .expect("the supplied session matches");
        assert!(matches!(
            exhausted.result(),
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(_)
            )
        ));

        let error = command_expecting(target, 1)
            .prepare_against(&session(session_id(2), 1))
            .expect_err("another session is an adapter correlation failure");
        assert_eq!(error.command().session(), target);
        assert_eq!(error.provided_session(), session_id(2));
    }

    /// S01 / INV-002 / INV-008 / INV-012: complete applied effect facts
    /// reconstruct exactly one correlated typed result.
    #[test]
    fn s01_inv002_inv008_inv012_applied_reconstitution_checks_complete_effects() {
        let target = session_id(1);
        let command = command_expecting(target, 1);
        let reconstructed = AppliedFacts::matching(&command)
            .reconstitute(command.clone())
            .expect("matching complete facts reconstruct");

        assert_eq!(reconstructed.command(), &command);
        let ReplaceSessionDefaultsResult::Applied(applied) = reconstructed.result() else {
            panic!("applied facts must reconstruct an applied result");
        };
        assert_eq!(applied.session(), target);
        assert_eq!(applied.installed().version(), version(2));
        assert_eq!(applied.installed().defaults(), command.replacement());
    }

    /// S01 / INV-008 / INV-012: equal replay of an earlier applied command
    /// remains valid after a later command advances the mutable current
    /// pointer.
    #[test]
    fn s01_inv008_inv012_historical_applied_receipt_ignores_later_current_pointer() {
        let target = session_id(1);
        let historical_command = command_expecting(target, 1);
        let current_after_later_replacement = session(target, 3);
        assert_eq!(
            current_after_later_replacement
                .current_configuration_defaults()
                .version(),
            version(3)
        );

        let historical_receipt = AppliedFacts::matching(&historical_command)
            .reconstitute(historical_command)
            .expect("later pointer movement cannot invalidate immutable history");

        let ReplaceSessionDefaultsResult::Applied(applied) = historical_receipt.result() else {
            panic!("historical applied facts must reconstruct their recorded result");
        };
        assert_eq!(applied.installed().version(), version(2));
    }

    /// S01 / INV-002 / INV-012: a cross-wired owner, non-successor, or
    /// mismatched replacement fails closed instead of constructing authority.
    #[test]
    fn s01_inv002_inv012_applied_reconstitution_fails_closed() {
        let target = session_id(1);
        let another_session = session_id(2);
        let command = command_expecting(target, 1);
        let matching = AppliedFacts::matching(&command);

        let cross_wired_result = AppliedFacts {
            result_session: another_session,
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("a cross-wired result session must fail")
        .failure();
        assert_eq!(
            cross_wired_result,
            ReplaceSessionDefaultsReconstitutionFailure::ResultSessionMismatch
        );

        let cross_wired_defaults_owner = AppliedFacts {
            defaults_session: another_session,
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("a cross-wired defaults owner must fail")
        .failure();
        assert_eq!(
            cross_wired_defaults_owner,
            ReplaceSessionDefaultsReconstitutionFailure::DefaultsSessionMismatch
        );

        let torn_result_version = AppliedFacts {
            result_version: version(3),
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("the result and selected record must name one version")
        .failure();
        assert_eq!(
            torn_result_version,
            ReplaceSessionDefaultsReconstitutionFailure::ResultVersionMismatch
        );

        let skipped_successor = AppliedFacts {
            result_version: version(3),
            defaults_version: version(3),
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("an installed version must be the checked successor")
        .failure();
        assert_eq!(
            skipped_successor,
            ReplaceSessionDefaultsReconstitutionFailure::InstalledVersionIsNotSuccessor
        );

        let replaced_defaults = AppliedFacts {
            defaults: defaults(3),
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("stored defaults must match the command replacement")
        .failure();
        assert_eq!(
            replaced_defaults,
            ReplaceSessionDefaultsReconstitutionFailure::StoredDefaultsMismatch
        );

        // S34 / INV-046: a stored install diverging from the command's
        // replacement only in its optional system prompt is the same
        // fail-closed defaults mismatch.
        let prompt_diverged = AppliedFacts {
            defaults: SessionConfigurationDefaults::complete(
                command.replacement().model(),
                command.replacement().dangerous_tool_auto_approval(),
                Some(
                    crate::SessionSystemPrompt::try_new(String::from("exact session instructions"))
                        .expect("test prompt is admissible"),
                ),
            ),
            ..matching.clone()
        }
        .reconstitute(command.clone())
        .expect_err("a prompt-only divergence must fail closed")
        .failure();
        assert_eq!(
            prompt_diverged,
            ReplaceSessionDefaultsReconstitutionFailure::StoredDefaultsMismatch
        );

        /// One fail-closed perturbation and the typed failure it produced,
        /// rendered as a snapshot row supplementing the targeted asserts
        /// above (TS-10, TS-12), rendered as one opaque Debug value.
        #[derive(Debug)]
        #[allow(
            dead_code,
            reason = "the table renderer reads every field through the Debug derive"
        )]
        struct PerturbedFactRow {
            perturbed_stored_fact: &'static str,
            failure: ReplaceSessionDefaultsReconstitutionFailure,
        }

        expect![[r#"
            ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ value                                                                                                                    │
            ├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ PerturbedFactRow { perturbed_stored_fact: "result session cross-wired", failure: ResultSessionMismatch }                 │
            │ PerturbedFactRow { perturbed_stored_fact: "defaults owner cross-wired", failure: DefaultsSessionMismatch }               │
            │ PerturbedFactRow { perturbed_stored_fact: "result and installed versions torn", failure: ResultVersionMismatch }         │
            │ PerturbedFactRow { perturbed_stored_fact: "installed version skips successor", failure: InstalledVersionIsNotSuccessor } │
            │ PerturbedFactRow { perturbed_stored_fact: "stored replacement differs", failure: StoredDefaultsMismatch }                │
            └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            PerturbedFactRow {
                perturbed_stored_fact: "result session cross-wired",
                failure: cross_wired_result,
            },
            PerturbedFactRow {
                perturbed_stored_fact: "defaults owner cross-wired",
                failure: cross_wired_defaults_owner,
            },
            PerturbedFactRow {
                perturbed_stored_fact: "result and installed versions torn",
                failure: torn_result_version,
            },
            PerturbedFactRow {
                perturbed_stored_fact: "installed version skips successor",
                failure: skipped_successor,
            },
            PerturbedFactRow {
                perturbed_stored_fact: "stored replacement differs",
                failure: replaced_defaults,
            },
        ]));
    }

    /// S01 / INV-002 / INV-012: each rejected record validates its command
    /// correlation and semantic predicate before reconstruction.
    #[test]
    fn s01_inv002_inv012_rejected_reconstitution_is_checked() {
        let target = session_id(1);
        let command = command_expecting(target, 1);

        let mismatch =
            ReplaceSessionDefaultsReconstitutionInput::rejected_current_version_mismatch(
                command.clone(),
                target,
                version(1),
                version(2),
            )
            .reconstitute()
            .expect("a genuine mismatch reconstructs");
        assert!(matches!(
            mismatch.result(),
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::CurrentVersionMismatch(_)
            )
        ));

        let missing = ReplaceSessionDefaultsReconstitutionInput::rejected_session_not_found(
            command.clone(),
            target,
        )
        .reconstitute()
        .expect("a correlated missing-session result reconstructs");
        assert!(matches!(
            missing.result(),
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::SessionNotFound(_)
            )
        ));

        let equal_versions =
            ReplaceSessionDefaultsReconstitutionInput::rejected_current_version_mismatch(
                command.clone(),
                target,
                version(1),
                version(1),
            )
            .reconstitute()
            .expect_err("equal versions are not a mismatch");
        assert_eq!(
            equal_versions.failure(),
            ReplaceSessionDefaultsReconstitutionFailure::RejectedVersionsAreEqual
        );

        let wrong_expected =
            ReplaceSessionDefaultsReconstitutionInput::rejected_current_version_mismatch(
                command.clone(),
                target,
                version(2),
                version(3),
            )
            .reconstitute()
            .expect_err("the rejection must repeat the command's expected version");
        assert_eq!(
            wrong_expected.failure(),
            ReplaceSessionDefaultsReconstitutionFailure::ResultExpectedVersionMismatch
        );

        let not_exhausted = ReplaceSessionDefaultsReconstitutionInput::rejected_version_exhausted(
            command.clone(),
            target,
            version(1),
        )
        .reconstitute()
        .expect_err("a version with a successor is not exhausted");
        assert_eq!(
            not_exhausted.failure(),
            ReplaceSessionDefaultsReconstitutionFailure::ResultVersionIsNotExhausted
        );

        let exhausted_version = version(u64::MAX);
        let exhausted_command =
            ReplaceSessionDefaults::new(command_id(2), target, exhausted_version, defaults(2));
        let exhausted = ReplaceSessionDefaultsReconstitutionInput::rejected_version_exhausted(
            exhausted_command,
            target,
            exhausted_version,
        )
        .reconstitute()
        .expect("a correlated exhausted-version result reconstructs");
        assert!(matches!(
            exhausted.result(),
            ReplaceSessionDefaultsResult::Rejected(
                ReplaceSessionDefaultsRejectedResult::VersionExhausted(_)
            )
        ));
    }
}
