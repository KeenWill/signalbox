//! PostgreSQL store for runner-protocol aggregates.
//!
//! SQL rows remain adapter-private. Loads join canonical enrollment,
//! registration, placement, grant, and lease evidence before invoking the
//! domain reconstitution gates.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    num::NonZeroU64,
    sync::Arc,
};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_domain::{
    CanonicalCloneUrlDigest, CredentialDispatchAuthorization, CredentialProfileGrant,
    CredentialProfileGrantReconstitutionInput, CredentialProfileGrantState, CredentialProfileName,
    CredentialProfilePolicy, CredentialToolApproval, EndedToolAttempt, PinnedRunnerPlacement,
    ProvisionedWorkspace, RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog,
    RunnerClaimedAttemptReplacement, RunnerCredentialGrantLineage, RunnerDomainError,
    RunnerEnrollment, RunnerEnrollmentId, RunnerEnrollmentReconstitutionInput,
    RunnerEnrollmentState, RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation,
    RunnerLeaseId, RunnerLeaseLoss, RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation,
    RunnerLeaseState, RunnerRepositoryEntry, RunnerSandboxProfile, RunnerSelector,
    RunnerToolDeclaration, RunnerToolEffectClass, RunnerToolModelDefinition,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides, RunnerWorkingDirectory, SessionId,
    SessionRunnerPin, SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, ToolAdmissibleLoci,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptEnd, ToolAttemptId, ToolDispatchGeneration, ToolEffectClass, ToolExecutionErrorKind,
    ToolName, ToolPermissionDefault, ToolRequestId, TurnAttemptId, TurnId,
    ValidatedRunnerRegistration, ValidatedRunnerRegistrationReconstitutionInput,
    WorkingDirectorySelection, WorkspaceBranchName, WorkspaceCapability, WorkspaceManifestId,
    WorkspaceRecovery, WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement,
    WorkspaceRevision,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::lock_inventory::{
    RUNNER_ENROLLMENT, RUNNER_GRANT, RUNNER_LEASE_ENROLLMENT_AUTHORITY,
    RUNNER_LEASE_GRANT_AUTHORITY, RUNNER_LEASE_HEAD, RUNNER_LEASE_PLACEMENT, RUNNER_PLACEMENT_HEAD,
    RUNNER_REGISTRATION_HEAD,
};

/// Adapter-owned positive revision of one validated registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerRegistrationRevision(NonZeroU64);

impl RunnerRegistrationRevision {
    /// Returns the first admitted registration revision.
    pub const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Admits one nonzero revision value.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive integer carried by this revision.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::try_from_u64(value),
            None => None,
        }
    }
}

/// One canonical validated registration plus its durable adapter revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredValidatedRunnerRegistration {
    revision: RunnerRegistrationRevision,
    registration: ValidatedRunnerRegistration,
}

impl StoredValidatedRunnerRegistration {
    /// Returns the durable adapter revision paired with the registration.
    pub const fn revision(&self) -> RunnerRegistrationRevision {
        self.revision
    }

    /// Returns the domain-validated registration snapshot.
    pub const fn registration(&self) -> &ValidatedRunnerRegistration {
        &self.registration
    }
}

/// One canonical placement record and its adapter event ordinal.
#[derive(Debug, Eq, PartialEq)]
pub struct StoredSessionRunnerPlacement {
    event_ordinal: u64,
    placement: SessionRunnerPlacement,
    registration: Option<StoredValidatedRunnerRegistration>,
    grant: Option<CredentialProfileGrant>,
}

impl StoredSessionRunnerPlacement {
    /// Returns the durable placement event ordinal.
    pub const fn event_ordinal(&self) -> u64 {
        self.event_ordinal
    }

    /// Returns the domain-reconstituted placement.
    pub const fn placement(&self) -> &SessionRunnerPlacement {
        &self.placement
    }

    /// Returns the registration snapshot pinned by this placement, if any.
    pub const fn registration(&self) -> Option<&StoredValidatedRunnerRegistration> {
        self.registration.as_ref()
    }

    /// Returns the credential grant pinned by this placement, if any.
    pub const fn grant(&self) -> Option<&CredentialProfileGrant> {
        self.grant.as_ref()
    }

    /// Separates the placement from its durable ordinal and pinned evidence.
    pub fn into_parts(
        self,
    ) -> (
        u64,
        SessionRunnerPlacement,
        Option<StoredValidatedRunnerRegistration>,
        Option<CredentialProfileGrant>,
    ) {
        (
            self.event_ordinal,
            self.placement,
            self.registration,
            self.grant,
        )
    }
}

/// PostgreSQL adapter for runner-protocol state.
struct RegistrationAuthority<'a> {
    stored: &'a StoredValidatedRunnerRegistration,
    catalog: &'a RunnerCatalog,
}

/// PostgreSQL adapter for runner enrollment, placement, grant, and lease state.
#[derive(Clone, Debug)]
pub struct RunnerProtocolStore {
    pool: PgPool,
    catalog: Arc<RunnerCatalog>,
}

impl RunnerProtocolStore {
    /// Uses the supplied pool and runner catalog for durable protocol state.
    pub fn new(pool: PgPool, catalog: RunnerCatalog) -> Self {
        Self {
            pool,
            catalog: Arc::new(catalog),
        }
    }

    /// Inserts one pristine active logical enrollment and its exact allowed
    /// classes. An enrollment that already issued a registration through the
    /// domain-only path is rejected: persisting only its enrollment rows
    /// would reload with no issued revision while the caller-held authority
    /// disagrees with canonical storage forever after.
    pub async fn insert_enrollment(
        &self,
        enrollment: &RunnerEnrollment,
    ) -> Result<(), RunnerProtocolStoreError> {
        if enrollment.state() != RunnerEnrollmentState::Active
            || enrollment.last_issued_registration_revision().is_some()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let classes: Vec<_> = enrollment.allowed_classes().collect();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO runner_enrollment_audit
                (enrollment_id, revision, runner_id,
                 authentication_reference_id, allowed_class_count, state_kind)
             VALUES ($1, 1, $2, $3, $4, 'active')",
        )
        .bind(enrollment.enrollment().into_uuid())
        .bind(enrollment.runner().into_uuid())
        .bind(enrollment.authentication().into_uuid())
        .bind(count_decimal(classes.len())?)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO runner_enrollment
                (enrollment_id, runner_id, authentication_reference_id,
                 allowed_class_count, revision, state_kind)
             VALUES ($1, $2, $3, $4, 1, 'active')",
        )
        .bind(enrollment.enrollment().into_uuid())
        .bind(enrollment.runner().into_uuid())
        .bind(enrollment.authentication().into_uuid())
        .bind(count_decimal(classes.len())?)
        .execute(&mut *transaction)
        .await?;
        for class in classes {
            sqlx::query(
                "INSERT INTO runner_enrollment_allowed_class
                    (enrollment_id, capability_class)
                 VALUES ($1, $2)",
            )
            .bind(enrollment.enrollment().into_uuid())
            .bind(class.as_str())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO runner_enrollment_audit_allowed_class
                    (enrollment_id, revision, capability_class)
                 VALUES ($1, 1, $2)",
            )
            .bind(enrollment.enrollment().into_uuid())
            .bind(class.as_str())
            .execute(&mut *transaction)
            .await?;
        }
        commit_mutation(transaction).await
    }

    /// Loads one enrollment through its canonical class and audit evidence.
    pub async fn load_enrollment(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerEnrollment>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = load_enrollment_in(transaction.as_mut(), enrollment).await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Applies terminal enrollment revocation under the enrollment row lock.
    pub async fn revoke_enrollment(
        &self,
        enrollment: &mut RunnerEnrollment,
    ) -> Result<bool, RunnerProtocolStoreError> {
        let enrollment_id = enrollment.enrollment();
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            transaction.rollback().await?;
            return Ok(false);
        }
        let canonical = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if canonical != *enrollment {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let runner = enrollment.runner();
        let authentication = enrollment.authentication();
        let classes: Vec<_> = enrollment.allowed_classes().cloned().collect();
        sqlx::query(
            "INSERT INTO runner_enrollment_audit
                (enrollment_id, revision, runner_id,
                 authentication_reference_id, allowed_class_count, state_kind)
             VALUES ($1, 2, $2, $3, $4, 'revoked')",
        )
        .bind(enrollment_id.into_uuid())
        .bind(runner.into_uuid())
        .bind(authentication.into_uuid())
        .bind(count_decimal(classes.len())?)
        .execute(&mut *transaction)
        .await?;
        for class in classes {
            sqlx::query(
                "INSERT INTO runner_enrollment_audit_allowed_class
                    (enrollment_id, revision, capability_class)
                 VALUES ($1, 2, $2)",
            )
            .bind(enrollment_id.into_uuid())
            .bind(class.as_str())
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "UPDATE runner_enrollment
                SET revision = 2, state_kind = 'revoked'
              WHERE enrollment_id = $1",
        )
        .bind(enrollment_id.into_uuid())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        enrollment
            .revoke_in_place()
            .map_err(RunnerProtocolStoreError::Domain)?;
        Ok(true)
    }

    /// Validates and appends one complete availability advertisement.
    pub async fn register(
        &self,
        enrollment: &RunnerEnrollment,
        advertisement: signalbox_domain::RunnerAdvertisement,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let enrollment_id = enrollment.enrollment();
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalEnrollment,
            ));
        }
        let canonical = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if canonical != *enrollment {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let pending = enrollment
            .prepare_registration(advertisement, &self.catalog)
            .map_err(RunnerProtocolStoreError::Domain)?;
        let previous: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let revision = match previous {
            Some(value) => decode_registration_revision(value)?.checked_next().ok_or(
                RunnerProtocolStoreError::Corruption(RunnerProtocolCorruption::GenerationExhausted),
            )?,
            None => RunnerRegistrationRevision::first(),
        };
        if pending.registration().revision().get() != revision.get() {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        insert_registration(&mut transaction, revision, pending.registration()).await?;
        sqlx::query(
            "INSERT INTO runner_current_registration
                (enrollment_id, registration_revision)
             VALUES ($1, $2)
             ON CONFLICT (enrollment_id)
             DO UPDATE SET registration_revision = EXCLUDED.registration_revision",
        )
        .bind(enrollment_id.into_uuid())
        .bind(Decimal::from(revision.get()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        let registration = pending.commit().map_err(RunnerProtocolStoreError::Domain)?;
        Ok(StoredValidatedRunnerRegistration {
            revision,
            registration,
        })
    }

    /// Loads one exact historical validated registration.
    pub async fn load_registration(
        &self,
        enrollment: &RunnerEnrollment,
        revision: RunnerRegistrationRevision,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = load_registration_in(
            transaction.as_mut(),
            enrollment.enrollment(),
            revision,
            Some(enrollment),
            &self.catalog,
        )
        .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Loads the current validated registration for an enrollment.
    pub async fn load_current_registration(
        &self,
        enrollment: &RunnerEnrollment,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.enrollment().into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let loaded = match revision {
            Some(revision) => {
                load_registration_in(
                    transaction.as_mut(),
                    enrollment.enrollment(),
                    decode_registration_revision(revision)?,
                    Some(enrollment),
                    &self.catalog,
                )
                .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Appends one domain-validated placement snapshot and optional grant.
    pub async fn store_placement(
        &self,
        placement: &SessionRunnerPlacement,
        registration: Option<&StoredValidatedRunnerRegistration>,
        grant: Option<&CredentialProfileGrant>,
    ) -> Result<(), RunnerProtocolStoreError> {
        validate_placement_snapshot(placement, registration, grant)?;

        let mut transaction = self.pool.begin().await?;
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(placement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = prior
            .as_ref()
            .map(|row| decode_u64(row.decode_column("event_ordinal")?))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), placement)?;
        // Both replacement events install successor authority, so the supplied
        // registration must still be the enrollment-owned current revision of
        // an active enrollment at commit time, verified under the enrollment
        // row lock: a replacement prepared before a concurrent revocation or
        // re-registration is rejected rather than committed as stale authority.
        if event_kind == "runner_replaced" || event_kind == "profile_replaced" {
            let registration = registration.ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
            let enrollment_id = registration.registration().enrollment();
            let locked = sqlx::query(RUNNER_ENROLLMENT)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            if locked.is_none() {
                return Err(RunnerProtocolStoreError::Corruption(
                    RunnerProtocolCorruption::MissingCanonicalEnrollment,
                ));
            }
            let state: String = sqlx::query_scalar(
                "SELECT state_kind
                   FROM runner_enrollment
                  WHERE enrollment_id = $1",
            )
            .bind(enrollment_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
            if state != "active" {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::EnrollmentRevoked,
                ));
            }
            let current: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            let current = current.ok_or(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalRegistration,
            ))?;
            if decode_registration_revision(current)?.get() != registration.revision().get() {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::RegistrationChanged,
                ));
            }
        }
        let grant_origin = placement_grant_origin(prior.as_ref(), event_ordinal, placement)?;
        // A profile replacement changes only profile axes: the placement
        // record carries the prior pinned registration snapshot forward even
        // though the domain validated the replacement against the
        // enrollment-owned current registration, which may have advanced to
        // an availability-equivalent revision since the pin.
        let registration_identity = if event_kind == "profile_replaced" {
            let prior_row = prior
                .as_ref()
                .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
            let prior_ordinal: Decimal = prior_row.decode_column("event_ordinal")?;
            let snapshot = sqlx::query(
                "SELECT registration_enrollment_id, registration_revision
                   FROM runner_session_placement_record
                  WHERE session_id = $1 AND event_ordinal = $2",
            )
            .bind(placement.session().into_uuid())
            .bind(prior_ordinal)
            .fetch_one(&mut *transaction)
            .await?;
            (
                snapshot.decode_column::<Option<Uuid>>("registration_enrollment_id")?,
                snapshot.decode_column::<Option<Decimal>>("registration_revision")?,
            )
        } else {
            stored_registration_identity(registration)
        };
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            placement,
            registration_identity,
            grant_origin,
        )
        .await?;
        if let (Some(grant), Some(registration)) = (grant, registration) {
            insert_grant_if_new(
                &mut transaction,
                prior.as_ref(),
                event_ordinal,
                placement,
                grant,
                RegistrationAuthority {
                    stored: registration,
                    catalog: &self.catalog,
                },
                grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO runner_current_session_placement
                (session_id, event_ordinal)
             VALUES ($1, $2)
             ON CONFLICT (session_id)
             DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }
    /// Atomically stores the first pinned placement, grant, and offered lease.
    pub async fn store_pin(
        &self,
        pin: &SessionRunnerPin,
        registration: &StoredValidatedRunnerRegistration,
    ) -> Result<(), RunnerProtocolStoreError> {
        validate_placement_snapshot(&pin.placement, Some(registration), pin.grant.as_ref())?;
        if pin.lease.state() != RunnerLeaseState::Offered {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(pin.placement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = prior
            .as_ref()
            .map(|row| decode_u64(row.decode_column("event_ordinal")?))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), &pin.placement)?;
        if event_kind != "pinned" {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let grant_origin = placement_grant_origin(prior.as_ref(), event_ordinal, &pin.placement)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            &pin.placement,
            stored_registration_identity(Some(registration)),
            grant_origin,
        )
        .await?;
        if let Some(grant) = pin.grant.as_ref() {
            insert_grant_if_new(
                &mut transaction,
                prior.as_ref(),
                event_ordinal,
                &pin.placement,
                grant,
                RegistrationAuthority {
                    stored: registration,
                    catalog: &self.catalog,
                },
                grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO runner_current_session_placement
                (session_id, event_ordinal)
             VALUES ($1, $2)
             ON CONFLICT (session_id)
             DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
        )
        .bind(pin.placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .execute(&mut *transaction)
        .await?;
        insert_lease_generation(&mut transaction, &pin.lease).await?;
        let correlation = pin.lease.correlation();
        sqlx::query(
            "INSERT INTO runner_lease_event
                (lease_id, generation, event_ordinal, state_kind)
             VALUES ($1, $2, 1, 'offered')",
        )
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO runner_current_lease_event
                (lease_id, generation, event_ordinal)
             VALUES ($1, $2, 1)",
        )
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await
    }

    /// Loads and reconstitutes the current placement and selected grant.
    pub async fn load_placement(
        &self,
        session: SessionId,
    ) -> Result<Option<StoredSessionRunnerPlacement>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT record.*
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let event_ordinal = decode_u64(row.decode_column("event_ordinal")?)?;
        let registration =
            load_placement_registration(transaction.as_mut(), &row, &self.catalog).await?;
        let grant = if registration.is_some() {
            load_grant_for_placement(transaction.as_mut(), &row, &self.catalog).await?
        } else {
            None
        };
        let pinned_profile =
            row.decode_column::<Option<String>>("pinned_credential_profile_name")?;
        let profileless_tombstone = grant.as_ref().filter(|grant| {
            grant.state() == CredentialProfileGrantState::Revoked && pinned_profile.is_none()
        });
        let placement = decode_placement(
            transaction.as_mut(),
            &row,
            registration
                .as_ref()
                .map(StoredValidatedRunnerRegistration::registration),
            profileless_tombstone,
        )
        .await?;
        transaction.commit().await?;
        Ok(Some(StoredSessionRunnerPlacement {
            event_ordinal,
            placement,
            registration,
            grant,
        }))
    }

    /// Appends the terminal revocation audit event for one current grant.
    pub async fn revoke_grant(
        &self,
        session: SessionId,
        runner: RunnerId,
        revision: RunnerGeneration,
    ) -> Result<Option<CredentialProfileGrant>, RunnerProtocolStoreError> {
        let placement = self
            .load_placement(session)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let Some(grant) = placement.grant else {
            return Ok(None);
        };
        if grant.runner() != runner || grant.revision() != revision {
            return Ok(None);
        }
        let revoked = grant.revoke().map_err(RunnerProtocolStoreError::Domain)?;
        let mut transaction = self.pool.begin().await?;
        let locked_placement = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(locked_placement) = locked_placement else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let locked_origin = locked_placement
            .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
        let locked_runner =
            locked_placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
        let locked_revision =
            locked_placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
        let locked_profile =
            locked_placement.decode_column::<Option<String>>("pinned_credential_profile_name")?;
        if locked_runner != Some(runner.into_uuid())
            || locked_revision != Some(Decimal::from(revision.get()))
            || locked_profile.as_deref() != Some(revoked.profile().as_str())
        {
            transaction.rollback().await?;
            return Ok(None);
        }
        let Some(locked_origin) = locked_origin else {
            return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into());
        };
        let locked: Option<String> = sqlx::query_scalar(RUNNER_GRANT)
            .bind(session.into_uuid())
            .bind(locked_origin)
            .bind(runner.into_uuid())
            .bind(Decimal::from(revision.get()))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(locked_profile) = locked else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if locked_profile != revoked.profile().as_str() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let already_revoked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_credential_grant_audit
                  WHERE session_id = $1
                    AND lineage_origin_event_ordinal = $2
                    AND runner_id = $3
                    AND grant_revision = $4
                    AND event_kind = 'revoked'
             )",
        )
        .bind(session.into_uuid())
        .bind(locked_origin)
        .bind(runner.into_uuid())
        .bind(Decimal::from(revision.get()))
        .fetch_one(&mut *transaction)
        .await?;
        if already_revoked {
            transaction.rollback().await?;
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO runner_credential_grant_audit
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, audit_ordinal,
                 event_kind, credential_profile_name)
             VALUES ($1, $2, $3, $4, 2, 'revoked', $5)",
        )
        .bind(session.into_uuid())
        .bind(locked_origin)
        .bind(runner.into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(revoked.profile().as_str())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(revoked))
    }

    /// Appends an offered lease generation or a later non-unclaimed state
    /// event. A claimed-retry successor lease is persisted only through
    /// [`Self::store_claimed_retry_replacement`], which commits it atomically
    /// with the fresh replacement attempt the schema requires.
    pub async fn store_lease(&self, lease: &RunnerLease) -> Result<(), RunnerProtocolStoreError> {
        if lease.state() == RunnerLeaseState::LostUnclaimed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(lease).await
    }

    /// Stores one sealed lease loss that does not claim independent no-execution proof.
    pub async fn store_lease_loss(
        &self,
        loss: &RunnerLeaseLoss,
    ) -> Result<(), RunnerProtocolStoreError> {
        if loss.no_execution_proof().is_some() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(loss.lost()).await
    }

    /// Durably reserves one retryable claimed loss for an exact replacement attempt.
    /// Replaying the same reservation is idempotent after an interrupted write sequence.
    pub async fn store_claimed_retry_attempt_authority(
        &self,
        loss: &RunnerLeaseLoss,
        replacement: &RunnerClaimedAttemptReplacement,
    ) -> Result<(), RunnerProtocolStoreError> {
        let source = loss.lost().correlation();
        if loss.retry().is_none()
            || !matches!(
                loss.lost().state(),
                RunnerLeaseState::LostExecutionPossible | RunnerLeaseState::LostClaimed
            )
            || replacement.source() != &source
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let replacement = replacement.replacement();
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO runner_claimed_retry_attempt_authority
                (source_lease_id, source_generation,
                 replacement_attempt_id, replacement_session_id,
                 replacement_turn_id, replacement_issuing_turn_attempt_id,
                 replacement_request_id, replacement_dispatch_generation)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (source_lease_id, source_generation) DO NOTHING",
        )
        .bind(source.lease.into_uuid())
        .bind(Decimal::from(source.generation.get()))
        .bind(replacement.attempt().into_uuid())
        .bind(replacement.session().into_uuid())
        .bind(replacement.turn().into_uuid())
        .bind(replacement.issuing_attempt().into_uuid())
        .bind(replacement.request().into_uuid())
        .bind(Decimal::from(replacement.generation().as_u64()))
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let reserved = sqlx::query(
                "SELECT replacement_attempt_id, replacement_session_id,
                        replacement_turn_id, replacement_issuing_turn_attempt_id,
                        replacement_request_id, replacement_dispatch_generation
                   FROM runner_claimed_retry_attempt_authority
                  WHERE source_lease_id = $1 AND source_generation = $2",
            )
            .bind(source.lease.into_uuid())
            .bind(Decimal::from(source.generation.get()))
            .fetch_optional(&mut *transaction)
            .await?;
            let exact = if let Some(row) = reserved {
                row.decode_column::<Uuid>("replacement_attempt_id")?
                    == replacement.attempt().into_uuid()
                    && row.decode_column::<Uuid>("replacement_session_id")?
                        == replacement.session().into_uuid()
                    && row.decode_column::<Uuid>("replacement_turn_id")?
                        == replacement.turn().into_uuid()
                    && row.decode_column::<Uuid>("replacement_issuing_turn_attempt_id")?
                        == replacement.issuing_attempt().into_uuid()
                    && row.decode_column::<Uuid>("replacement_request_id")?
                        == replacement.request().into_uuid()
                    && row.decode_column::<Decimal>("replacement_dispatch_generation")?
                        == Decimal::from(replacement.generation().as_u64())
            } else {
                false
            };
            if !exact {
                transaction.rollback().await?;
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::CorrelationMismatch,
                ));
            }
        }
        commit_mutation(transaction).await
    }

    /// Loads an exact claimed-retry reservation for crash-resumable replay.
    pub async fn load_claimed_retry_attempt_reservation(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<ToolAttemptDispatchCorrelation>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT replacement_attempt_id, replacement_session_id,
                    replacement_turn_id, replacement_issuing_turn_attempt_id,
                    replacement_request_id, replacement_dispatch_generation
               FROM runner_claimed_retry_attempt_authority
              WHERE source_lease_id = $1 AND source_generation = $2",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok::<_, RunnerProtocolStoreError>(ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: session_id(row.decode_column("replacement_session_id")?),
                    turn: TurnId::from_uuid(row.decode_column("replacement_turn_id")?),
                    issuing_attempt: TurnAttemptId::from_uuid(
                        row.decode_column("replacement_issuing_turn_attempt_id")?,
                    ),
                    request: ToolRequestId::from_uuid(row.decode_column("replacement_request_id")?),
                    attempt: tool_attempt_id(row.decode_column("replacement_attempt_id")?),
                    generation: decode_dispatch_generation(
                        row.decode_column("replacement_dispatch_generation")?,
                    )?,
                },
            ))
        })
        .transpose()
    }

    /// Atomically retires the in-flight source attempt to its effect-correct
    /// terminal history and persists the exact replacement attempt together
    /// with its successor lease generation, after `offer_retry` validated the
    /// private claimed-retry evidence. Committing all three in one
    /// transaction leaves only two durable claimed-retry states: the loss
    /// with its still-in-flight source (with or without the replayable
    /// reservation), or the complete consumed retry, whose successor lease is
    /// already offered. The schema rejects a replacement attempt committed
    /// without its successor generation, so a crash can no longer strand the
    /// retry between them, and a reloaded batch always carries either the
    /// live source the checked replacement requires or the retired identity
    /// inventory. The retired attempt is the exact predecessor the claimed
    /// replacement produced; the reservation and lease-generation triggers
    /// independently reject any other pairing.
    pub async fn store_claimed_retry_replacement(
        &self,
        retired: &EndedToolAttempt,
        retry: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if retry.state() != RunnerLeaseState::Offered
            || retry.generation() == RunnerGeneration::one()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let dispatch = retry.correlation().dispatch;
        let effect_matches = matches!(
            (retry.effect(), retired.effect_class()),
            (RunnerToolEffectClass::Pure, ToolEffectClass::EffectFree)
                | (
                    RunnerToolEffectClass::Idempotent,
                    ToolEffectClass::ExternalEffect
                )
        );
        if retired.session() != dispatch.session()
            || retired.turn() != dispatch.turn()
            || retired.issuing_attempt() != dispatch.issuing_attempt()
            || retired.request() != dispatch.request()
            || retired.attempt() == dispatch.attempt()
            || !effect_matches
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let (retired_disposition, retired_error) = match (retired.effect_class(), retired.end()) {
            (ToolEffectClass::EffectFree, ToolAttemptEnd::KnownFailed { error })
                if error.kind() == ToolExecutionErrorKind::CrashLost
                    && error.detail().is_none() =>
            {
                ("known_failed", Some("crash_lost"))
            }
            (ToolEffectClass::ExternalEffect, ToolAttemptEnd::Ambiguous) => ("ambiguous", None),
            _ => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::CorrelationMismatch,
                ));
            }
        };
        let mut transaction = self.pool.begin().await?;
        let retired_rows = sqlx::query(
            "UPDATE tool_attempt
                SET state_kind = 'terminal',
                    terminal_disposition_kind = $1,
                    error_kind = $2
              WHERE attempt_id = $3
                AND request_id = $4
                AND session_id = $5
                AND turn_id = $6
                AND issuing_turn_attempt_id = $7
                AND state_kind = 'in_flight'
                AND terminal_disposition_kind IS NULL",
        )
        .bind(retired_disposition)
        .bind(retired_error)
        .bind(retired.attempt().into_uuid())
        .bind(retired.request().into_uuid())
        .bind(retired.session().into_uuid())
        .bind(retired.turn().into_uuid())
        .bind(retired.issuing_attempt().into_uuid())
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if retired_rows != 1 {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::CrossWiredReference,
            ));
        }
        sqlx::query(
            "INSERT INTO tool_attempt
                (attempt_id, request_id, session_id, turn_id,
                 issuing_turn_attempt_id, effect_class, dispatch_generation,
                 state_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'in_flight')",
        )
        .bind(dispatch.attempt().into_uuid())
        .bind(dispatch.request().into_uuid())
        .bind(dispatch.session().into_uuid())
        .bind(dispatch.turn().into_uuid())
        .bind(dispatch.issuing_attempt().into_uuid())
        .bind(match retired.effect_class() {
            ToolEffectClass::EffectFree => "effect_free",
            ToolEffectClass::ExternalEffect => "external_effect",
        })
        .bind(Decimal::from(dispatch.generation().as_u64()))
        .execute(&mut *transaction)
        .await?;
        append_lease_event_in(&mut transaction, retry).await?;
        commit_mutation(transaction).await
    }

    async fn store_lease_without_proof(
        &self,
        lease: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if lease.state() == RunnerLeaseState::LostUnclaimed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        append_lease_event_in(&mut transaction, lease).await?;
        commit_mutation(transaction).await
    }

    /// Loads one exact lease generation and independently joined fence.
    pub async fn load_lease(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let lease = self
            .load_lease_in(&mut transaction, lease, generation)
            .await?;
        transaction.commit().await?;
        Ok(lease)
    }

    async fn load_lease_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT lease_generation.*, event.state_kind,
                    request.tool_name AS canonical_attempt_tool,
                    attempt.turn_id AS canonical_attempt_turn,
                    attempt.issuing_turn_attempt_id
                        AS canonical_issuing_attempt,
                    attempt.request_id AS canonical_attempt_request,
                    attempt.dispatch_generation
                        AS canonical_dispatch_generation,
                    placement.state_kind AS canonical_placement_state,
                    placement.pinned_runner_id AS canonical_placement_runner,
                    placement.registration_enrollment_id
                        AS canonical_registration_enrollment,
                    placement.registration_revision
                        AS canonical_registration_revision,
                    grant_tool.tool_name AS canonical_grant_tool,
                    grant_tool.approval_kind AS canonical_grant_approval
               FROM runner_lease_generation AS lease_generation
               JOIN runner_current_lease_event AS current_event
                 ON current_event.lease_id = lease_generation.lease_id
                AND current_event.generation = lease_generation.generation
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               LEFT JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = lease_generation.attempt_id
                AND attempt.session_id = lease_generation.session_id
               LEFT JOIN tool_request AS request
                 ON request.request_id = attempt.request_id
               LEFT JOIN runner_session_placement_record AS placement
                 ON placement.session_id = lease_generation.session_id
                AND placement.event_ordinal =
                    lease_generation.placement_event_ordinal
               LEFT JOIN runner_credential_grant_tool AS grant_tool
                 ON grant_tool.session_id = lease_generation.session_id
                AND grant_tool.lineage_origin_event_ordinal =
                    lease_generation.credential_grant_lineage_origin_ordinal
                AND grant_tool.runner_id = lease_generation.runner_id
                AND grant_tool.grant_revision =
                    lease_generation.credential_grant_revision
                AND grant_tool.credential_profile_name =
                    lease_generation.credential_profile_name
                AND grant_tool.tool_name = lease_generation.tool_name
              WHERE lease_generation.lease_id = $1
                AND lease_generation.generation = $2",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let registration = load_registration_in(
            transaction.as_mut(),
            runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
            decode_registration_revision(row.decode_column("registration_revision")?)?,
            None,
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let lease = decode_lease(&row, registration.registration())?;
        Ok(Some(lease))
    }

    /// Loads one durable loss generation and rebuilds its sealed retry authority.
    pub async fn load_lease_loss(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLeaseLoss>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let Some(loaded) = self
            .load_lease_in(&mut transaction, lease, generation)
            .await?
        else {
            transaction.commit().await?;
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT *
               FROM runner_lease_no_execution_proof
              WHERE lease_id = $1 AND generation = $2",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(transaction.as_mut())
        .await?;
        let no_execution = row
            .map(|row| {
                let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
                    ToolAttemptDispatchCorrelationReconstitutionInput {
                        session: session_id(row.decode_column("session_id")?),
                        turn: TurnId::from_uuid(row.decode_column("turn_id")?),
                        issuing_attempt: TurnAttemptId::from_uuid(
                            row.decode_column("issuing_turn_attempt_id")?,
                        ),
                        request: ToolRequestId::from_uuid(row.decode_column("request_id")?),
                        attempt: tool_attempt_id(row.decode_column("attempt_id")?),
                        generation: decode_dispatch_generation(
                            row.decode_column("dispatch_generation")?,
                        )?,
                    },
                );
                Ok::<_, RunnerProtocolStoreError>(RunnerLeaseCorrelation {
                    lease: runner_lease_id(row.decode_column("lease_id")?),
                    runner: runner_id(row.decode_column("runner_id")?),
                    tool: tool_name(row.decode_column("tool_name")?)?,
                    dispatch,
                    generation: decode_generation(row.decode_column("generation")?)?,
                })
            })
            .transpose()?;
        let retry_prepared: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_claimed_retry_attempt_authority AS authority
                   JOIN tool_attempt AS replacement
                     ON replacement.attempt_id = authority.replacement_attempt_id
                    AND replacement.session_id = authority.replacement_session_id
                    AND replacement.turn_id = authority.replacement_turn_id
                    AND replacement.issuing_turn_attempt_id =
                        authority.replacement_issuing_turn_attempt_id
                    AND replacement.request_id = authority.replacement_request_id
                    AND replacement.dispatch_generation =
                        authority.replacement_dispatch_generation
                  WHERE authority.source_lease_id = $1
                    AND authority.source_generation = $2
                 UNION ALL
                 SELECT 1
                   FROM runner_lease_generation
                  WHERE lease_id = $1 AND predecessor_generation = $2
             )",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_one(transaction.as_mut())
        .await?;
        transaction.commit().await?;
        let retry_preparation = match retry_prepared {
            true => RunnerLeaseRetryPreparation::Prepared,
            false => RunnerLeaseRetryPreparation::Available,
        };
        loaded
            .into_reconstituted_loss(no_execution, retry_preparation)
            .map(Some)
            .map_err(RunnerProtocolStoreError::Domain)
    }
}

async fn load_enrollment_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
) -> Result<Option<RunnerEnrollment>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT enrollment.enrollment_id, enrollment.runner_id,
                enrollment.authentication_reference_id,
                enrollment.allowed_class_count, enrollment.state_kind,
                audit.runner_id AS audit_runner_id,
                audit.authentication_reference_id AS audit_authentication_reference_id,
                audit.allowed_class_count AS audit_allowed_class_count,
                audit.state_kind AS audit_state_kind,
                current_registration.registration_revision
           FROM runner_enrollment AS enrollment
           LEFT JOIN runner_enrollment_audit AS audit
             ON audit.enrollment_id = enrollment.enrollment_id
            AND audit.revision = enrollment.revision
           LEFT JOIN runner_current_registration AS current_registration
             ON current_registration.enrollment_id = enrollment.enrollment_id
          WHERE enrollment.enrollment_id = $1",
    )
    .bind(enrollment.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let class_rows = sqlx::query(
        "SELECT capability_class
           FROM runner_enrollment_allowed_class
          WHERE enrollment_id = $1
          ORDER BY capability_class",
    )
    .bind(enrollment.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let audit_class_rows = sqlx::query(
        "SELECT audited.capability_class
           FROM runner_enrollment AS enrollment
           JOIN runner_enrollment_audit_allowed_class AS audited
             ON audited.enrollment_id = enrollment.enrollment_id
            AND audited.revision = enrollment.revision
          WHERE enrollment.enrollment_id = $1
          ORDER BY audited.capability_class",
    )
    .bind(enrollment.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if Decimal::from(class_rows.len()) != row.decode_column::<Decimal>("allowed_class_count")? {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let audit_count = row
        .decode_column::<Option<Decimal>>("audit_allowed_class_count")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    if Decimal::from(audit_class_rows.len()) != audit_count {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let classes = decode_classes(&class_rows)?;
    let audit_classes = decode_classes(&audit_class_rows)?;
    let state = decode_enrollment_state(row.decode_column("state_kind")?)?;
    let audit_state = row
        .decode_column::<Option<String>>("audit_state_kind")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    let audit_state = decode_enrollment_state(&audit_state)?;
    let registration_revision = row
        .decode_column::<Option<Decimal>>("registration_revision")?
        .map(decode_generation)
        .transpose()?;
    RunnerEnrollment::reconstitute(RunnerEnrollmentReconstitutionInput {
        enrollment,
        recorded_enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
        runner: runner_id(row.decode_column("runner_id")?),
        recorded_runner: runner_id(
            row.decode_column::<Option<Uuid>>("audit_runner_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        authentication: runner_authentication_id(row.decode_column("authentication_reference_id")?),
        recorded_authentication: runner_authentication_id(
            row.decode_column::<Option<Uuid>>("audit_authentication_reference_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        allowed_classes: classes,
        recorded_allowed_classes: audit_classes,
        state,
        recorded_state: audit_state,
        registration_revision,
        recorded_registration_revision: registration_revision,
    })
    .map(Some)
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn insert_registration(
    transaction: &mut Transaction<'_, Postgres>,
    revision: RunnerRegistrationRevision,
    registration: &ValidatedRunnerRegistration,
) -> Result<(), RunnerProtocolStoreError> {
    let classes: Vec<_> = registration.classes().collect();
    let tools: Vec<_> = registration.tools().collect();
    let profiles: Vec<_> = registration.profiles().collect();
    let workspaces: Vec<_> = registration.workspaces().collect();
    let sandboxes: Vec<_> = registration.sandboxes().collect();
    let repositories: Vec<_> = registration.repositories().collect();
    sqlx::query(
        "INSERT INTO runner_registration
            (enrollment_id, registration_revision, runner_id,
             authentication_reference_id, class_count, tool_count,
             profile_count, workspace_count, repository_count, sandbox_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(registration.enrollment().into_uuid())
    .bind(Decimal::from(revision.get()))
    .bind(registration.runner().into_uuid())
    .bind(registration.authentication().into_uuid())
    .bind(count_decimal(classes.len())?)
    .bind(count_decimal(tools.len())?)
    .bind(count_decimal(profiles.len())?)
    .bind(count_decimal(workspaces.len())?)
    .bind(count_decimal(repositories.len())?)
    .bind(count_decimal(sandboxes.len())?)
    .execute(&mut **transaction)
    .await?;
    for class in classes {
        sqlx::query(
            "INSERT INTO runner_registration_class
                (enrollment_id, registration_revision, capability_class)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(class.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    for tool in tools {
        let loci = encode_loci(tool.loci())?;
        sqlx::query(
            "INSERT INTO runner_registration_tool
                (enrollment_id, registration_revision, tool_name,
                 model_description, model_input_schema, permission_kind,
                 effect_class, loci_kind, selector_kind, selector_runner_id,
                 selector_capability_class)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(tool.name().as_str())
        .bind(tool.model().description())
        .bind(tool.model().input_schema().as_str())
        .bind(encode_permission(tool.permission()))
        .bind(encode_effect(tool.effect()))
        .bind(loci.kind)
        .bind(loci.selector_kind)
        .bind(loci.selector_runner)
        .bind(loci.selector_class)
        .execute(&mut **transaction)
        .await?;
    }
    for profile in profiles {
        let approvals: Vec<_> = profile.approvals().collect();
        sqlx::query(
            "INSERT INTO runner_registration_profile
                (enrollment_id, registration_revision,
                 credential_profile_name, approval_count)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(profile.name().as_str())
        .bind(count_decimal(approvals.len())?)
        .execute(&mut **transaction)
        .await?;
        for (tool, approval) in approvals {
            sqlx::query(
                "INSERT INTO runner_registration_profile_approval
                    (enrollment_id, registration_revision,
                     credential_profile_name, tool_name, approval_kind)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(registration.enrollment().into_uuid())
            .bind(Decimal::from(revision.get()))
            .bind(profile.name().as_str())
            .bind(tool.as_str())
            .bind(encode_approval(approval))
            .execute(&mut **transaction)
            .await?;
        }
    }
    for workspace in workspaces {
        sqlx::query(
            "INSERT INTO runner_registration_workspace
                (enrollment_id, registration_revision, workspace_kind)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(encode_workspace(workspace))
        .execute(&mut **transaction)
        .await?;
    }
    for sandbox in sandboxes {
        sqlx::query(
            "INSERT INTO runner_registration_sandbox
                (enrollment_id, registration_revision, sandbox_profile)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(encode_sandbox(sandbox))
        .execute(&mut **transaction)
        .await?;
    }
    for repository in repositories {
        sqlx::query(
            "INSERT INTO runner_registration_repository
                (enrollment_id, registration_revision, repository_key,
                 credential_profile_name)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(repository.key().as_str())
        .bind(
            repository
                .credential_profile()
                .map(CredentialProfileName::as_str),
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn load_registration_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    revision: RunnerRegistrationRevision,
    authority: Option<&RunnerEnrollment>,
    catalog: &RunnerCatalog,
) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT *
           FROM runner_registration
          WHERE enrollment_id = $1
            AND registration_revision = $2",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let canonical = load_enrollment_in(connection, enrollment)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
    let authority = authority.unwrap_or(&canonical);
    if canonical != *authority {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorruptStoredFacts,
        ));
    }
    let class_rows = sqlx::query(
        "SELECT capability_class
           FROM runner_registration_class
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY capability_class",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let tool_rows = sqlx::query(
        "SELECT *
           FROM runner_registration_tool
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY tool_name",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let profile_rows = sqlx::query(
        "SELECT *
           FROM runner_registration_profile
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY credential_profile_name",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let workspace_rows = sqlx::query(
        "SELECT workspace_kind
           FROM runner_registration_workspace
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY workspace_kind",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let sandbox_rows = sqlx::query(
        "SELECT sandbox_profile
           FROM runner_registration_sandbox
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY sandbox_profile",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let repository_rows = sqlx::query(
        "SELECT repository_key, credential_profile_name
           FROM runner_registration_repository
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY repository_key",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    require_count(&row, "class_count", class_rows.len())?;
    require_count(&row, "tool_count", tool_rows.len())?;
    require_count(&row, "profile_count", profile_rows.len())?;
    require_count(&row, "workspace_count", workspace_rows.len())?;
    require_count(&row, "sandbox_count", sandbox_rows.len())?;
    require_count(&row, "repository_count", repository_rows.len())?;
    let classes = decode_classes(&class_rows)?;
    let tools = tool_rows
        .iter()
        .map(decode_tool_declaration)
        .collect::<Result<Vec<_>, _>>()?;
    let mut profiles = Vec::with_capacity(profile_rows.len());
    for profile in profile_rows {
        let name = profile_name(profile.decode_column("credential_profile_name")?)?;
        let approval_rows = sqlx::query(
            "SELECT tool_name, approval_kind
               FROM runner_registration_profile_approval
              WHERE enrollment_id = $1
                AND registration_revision = $2
                AND credential_profile_name = $3
              ORDER BY tool_name",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(name.as_str())
        .fetch_all(&mut *connection)
        .await?;
        require_count(&profile, "approval_count", approval_rows.len())?;
        let approvals = approval_rows
            .iter()
            .map(|row| {
                Ok((
                    tool_name(row.decode_column("tool_name")?)?,
                    decode_approval(row.decode_column("approval_kind")?)?,
                ))
            })
            .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
        profiles.push(
            CredentialProfilePolicy::try_new(name, approvals)
                .map_err(RunnerProtocolStoreError::Domain)?,
        );
    }
    let workspaces = workspace_rows
        .iter()
        .map(|row| decode_workspace(row.decode_column("workspace_kind")?))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let sandboxes = sandbox_rows
        .iter()
        .map(|row| decode_sandbox(row.decode_column("sandbox_profile")?))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let repositories = repository_rows
        .iter()
        .map(|row| {
            Ok(RunnerRepositoryEntry::new(
                repository_key(row.decode_column("repository_key")?)?,
                row.decode_column::<Option<String>>("credential_profile_name")?
                    .map(profile_name)
                    .transpose()?,
            ))
        })
        .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
    let registration = ValidatedRunnerRegistration::reconstitute(
        authority,
        catalog,
        ValidatedRunnerRegistrationReconstitutionInput {
            enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
            revision: RunnerGeneration::try_from_u64(revision.get())
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
            runner: runner_id(row.decode_column("runner_id")?),
            authentication: runner_authentication_id(
                row.decode_column("authentication_reference_id")?,
            ),
            classes,
            tools,
            profiles,
            workspaces,
            sandboxes,
            repositories,
        },
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    Ok(Some(StoredValidatedRunnerRegistration {
        revision,
        registration,
    }))
}

fn classify_placement_event(
    prior: Option<&PgRow>,
    placement: &SessionRunnerPlacement,
) -> Result<&'static str, RunnerProtocolStoreError> {
    let Some(prior) = prior else {
        if matches!(placement.state(), SessionRunnerPlacementState::Unpinned)
            && placement.revision() == RunnerGeneration::one()
        {
            return Ok("created");
        }
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ));
    };
    let prior_revision = decode_generation(prior.decode_column("placement_revision")?)?;
    let prior_state: String = prior.decode_column("state_kind")?;
    match (prior_state.as_str(), placement.state()) {
        ("unpinned", SessionRunnerPlacementState::Pinned(_))
            if placement.revision() == prior_revision =>
        {
            Ok("pinned")
        }
        ("pinned", SessionRunnerPlacementState::RunnerLost(_))
            if placement.revision() == prior_revision =>
        {
            Ok("runner_lost")
        }
        ("runner_lost", SessionRunnerPlacementState::Pinned(_))
            if placement.revision()
                == prior_revision
                    .checked_next()
                    .ok_or(RunnerProtocolCorruption::GenerationExhausted)? =>
        {
            Ok("runner_replaced")
        }
        ("pinned", SessionRunnerPlacementState::Pinned(_))
            if placement.revision()
                == prior_revision
                    .checked_next()
                    .ok_or(RunnerProtocolCorruption::GenerationExhausted)? =>
        {
            Ok("profile_replaced")
        }
        _ => Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
    }
}

fn placement_grant_origin(
    prior: Option<&PgRow>,
    event_ordinal: u64,
    placement: &SessionRunnerPlacement,
) -> Result<Option<Decimal>, RunnerProtocolStoreError> {
    let lineage = match placement.state() {
        SessionRunnerPlacementState::Unpinned => None,
        SessionRunnerPlacementState::Pinned(pinned)
        | SessionRunnerPlacementState::RunnerLost(pinned) => pinned.grant_lineage,
    };
    let Some(lineage) = lineage else {
        return Ok(None);
    };
    if let Some(prior) = prior {
        let prior_origin =
            prior.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
        let prior_runner = prior.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
        let prior_revision = prior.decode_column::<Option<Decimal>>("credential_grant_revision")?;
        match (prior_origin, prior_runner, prior_revision) {
            (Some(origin), Some(runner), Some(revision)) => {
                let revision = decode_generation(revision)?;
                let same_grant =
                    revision == lineage.revision && runner_id(runner) == lineage.runner;
                let successor = revision.checked_next() == Some(lineage.revision);
                if same_grant || successor {
                    return Ok(Some(origin));
                }
            }
            (None, None, None) => {}
            _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
        }
    }
    if lineage.revision == RunnerGeneration::one() {
        Ok(Some(Decimal::from(event_ordinal)))
    } else {
        Err(RunnerProtocolCorruption::MissingCanonicalGrant.into())
    }
}

fn stored_registration_identity(
    registration: Option<&StoredValidatedRunnerRegistration>,
) -> (Option<Uuid>, Option<Decimal>) {
    registration
        .map(|registration| {
            (
                Some(registration.registration.enrollment().into_uuid()),
                Some(Decimal::from(registration.revision.get())),
            )
        })
        .unwrap_or((None, None))
}

async fn insert_placement_record(
    transaction: &mut Transaction<'_, Postgres>,
    event_ordinal: u64,
    event_kind: &str,
    placement: &SessionRunnerPlacement,
    registration_identity: (Option<Uuid>, Option<Decimal>),
    grant_origin: Option<Decimal>,
) -> Result<(), RunnerProtocolStoreError> {
    let request = placement.request();
    let (selector_kind, selector_runner, selector_class) = encode_selector(&request.selector);
    let (directory_kind, requested_directory) = encode_directory(&request.working_directory);
    let (workspace_kind, requested_repository) = encode_workspace_requirement(&request.workspace);
    let state = encode_placement_state(placement.state());
    let permission_overrides: Vec<_> = request.permission_overrides.iter().collect();
    let (registration_enrollment, registration_revision) = registration_identity;
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal, credential_grant_revision)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
             $12, $13, $14, $15, $16, $17, $18, $19, $20, $21,
             $22, $23, $24, $25, $26, $27, $28, $29, $30, $31,
             $32, $33, $34
         )",
    )
    .bind(placement.session().into_uuid())
    .bind(Decimal::from(event_ordinal))
    .bind(Decimal::from(placement.revision().get()))
    .bind(event_kind)
    .bind(selector_kind)
    .bind(selector_runner)
    .bind(selector_class)
    .bind(directory_kind)
    .bind(requested_directory)
    .bind(
        request
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
    )
    .bind(workspace_kind)
    .bind(requested_repository)
    .bind(encode_sandbox(request.sandbox))
    .bind(count_decimal(permission_overrides.len())?)
    .bind(state.kind)
    .bind(state.pinned_runner)
    .bind(state.pinned_directory)
    .bind(state.pinned_profile)
    .bind(registration_enrollment)
    .bind(registration_revision)
    .bind(count_decimal(state.tools.len())?)
    .bind(state.workspace_repository)
    .bind(state.workspace_directory)
    .bind(state.workspace_manifest)
    .bind(state.workspace_clone_url_digest)
    .bind(state.workspace_credential_profile)
    .bind(state.workspace_sandbox)
    .bind(state.workspace_relative_path)
    .bind(state.workspace_recovery_kind)
    .bind(state.workspace_branch_name)
    .bind(state.workspace_revision)
    .bind(
        state
            .grant_lineage
            .map(|lineage| lineage.runner.into_uuid()),
    )
    .bind(grant_origin)
    .bind(
        state
            .grant_lineage
            .map(|lineage| Decimal::from(lineage.revision.get())),
    )
    .execute(&mut **transaction)
    .await?;
    for tool in state.tools {
        sqlx::query(
            "INSERT INTO runner_session_placement_tool
                (session_id, event_ordinal, tool_name, runner_required)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(tool.as_str())
        .bind(state.runner_required_tools.contains(tool))
        .execute(&mut **transaction)
        .await?;
    }
    for (tool, permission) in permission_overrides {
        sqlx::query(
            "INSERT INTO runner_session_placement_permission_override
                (session_id, event_ordinal, tool_name, permission_kind)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(tool.as_str())
        .bind(encode_permission_override(permission))
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn insert_grant_if_new(
    transaction: &mut Transaction<'_, Postgres>,
    prior_placement: Option<&PgRow>,
    placement_event: u64,
    placement: &SessionRunnerPlacement,
    grant: &CredentialProfileGrant,
    authority: RegistrationAuthority<'_>,
    grant_origin: Decimal,
) -> Result<(), RunnerProtocolStoreError> {
    let registration = authority.stored;
    let catalog = authority.catalog;
    let historical_registration;
    let tombstone = matches!(
        placement.state(),
        SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned)
            if pinned.credential_profile.is_none()
    );
    let grant_registration = if !tombstone {
        registration
    } else {
        let prior_revision = grant
            .revision()
            .get()
            .checked_sub(1)
            .filter(|revision| *revision > 0)
            .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        let row = sqlx::query(
            "SELECT registration_enrollment_id, registration_revision
               FROM runner_credential_grant
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(prior_revision))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        historical_registration = load_registration_in(
            transaction.as_mut(),
            runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
            decode_registration_revision(row.decode_column("registration_revision")?)?,
            None,
            catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        &historical_registration
    };
    let (grant_sandbox, grant_permission_overrides) = if tombstone {
        let prior = prior_placement.ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let prior_record = sqlx::query(
            "SELECT *
               FROM runner_session_placement_record
              WHERE session_id = $1 AND event_ordinal = $2",
        )
        .bind(placement.session().into_uuid())
        .bind(prior.decode_column::<Decimal>("event_ordinal")?)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        (
            decode_sandbox(prior_record.decode_column("requested_sandbox_profile")?)?,
            load_permission_overrides(transaction.as_mut(), &prior_record).await?,
        )
    } else {
        (
            placement.request().sandbox,
            placement.request().permission_overrides.clone(),
        )
    };
    CredentialProfileGrant::reconstitute(
        grant_input(grant),
        grant.session(),
        grant_registration.registration(),
        grant_sandbox,
        &grant_permission_overrides,
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM runner_credential_grant
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
         )",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .fetch_one(&mut **transaction)
    .await?;
    if exists {
        let row = sqlx::query(
            "SELECT grant_record.*,
                    EXISTS (
                        SELECT 1
                          FROM runner_credential_grant_audit AS audit
                         WHERE audit.session_id = grant_record.session_id
                           AND audit.lineage_origin_event_ordinal =
                                grant_record.lineage_origin_event_ordinal
                           AND audit.runner_id = grant_record.runner_id
                           AND audit.grant_revision =
                                grant_record.grant_revision
                           AND audit.event_kind = 'revoked'
                    ) AS revoked
               FROM runner_credential_grant AS grant_record
              WHERE grant_record.session_id = $1
                AND grant_record.lineage_origin_event_ordinal = $2
                AND grant_record.runner_id = $3
                AND grant_record.grant_revision = $4",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_one(&mut **transaction)
        .await?;
        let tool_rows = sqlx::query(
            "SELECT tool_name, approval_kind
               FROM runner_credential_grant_tool
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
              ORDER BY tool_name",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_all(&mut **transaction)
        .await?;
        require_count(&row, "tool_count", tool_rows.len())?;
        let mut approvals = BTreeMap::new();
        for tool_row in tool_rows {
            approvals.insert(
                tool_name(tool_row.decode_column("tool_name")?)?,
                decode_approval(tool_row.decode_column("approval_kind")?)?,
            );
        }
        let expected_approvals: BTreeMap<_, _> = grant
            .approvals()
            .map(|(tool, approval)| (tool.clone(), approval))
            .collect();
        let stored_state =
            match decode_stored_grant_revocation(row.decode_column::<bool>("revoked")?) {
                StoredGrantRevocation::Active => CredentialProfileGrantState::Active,
                StoredGrantRevocation::Revoked => CredentialProfileGrantState::Revoked,
            };
        if row.decode_column::<String>("credential_profile_name")? != grant.profile().as_str()
            || runner_enrollment_id(row.decode_column("registration_enrollment_id")?)
                != grant_registration.registration.enrollment()
            || decode_registration_revision(row.decode_column("registration_revision")?)?
                != grant_registration.revision
            || approvals != expected_approvals
            || stored_state != grant.state()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        return Ok(());
    }
    let tools: Vec<_> = grant.approvals().collect();
    let prior = grant
        .revision()
        .get()
        .checked_sub(1)
        .filter(|value| *value > 0)
        .map(Decimal::from);
    let prior_runner: Option<Uuid> = match (prior, prior_placement) {
        (Some(expected_revision), Some(prior_placement)) => {
            let runner = prior_placement
                .decode_column::<Option<Uuid>>("credential_grant_runner_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let origin = prior_placement
                .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let revision = prior_placement
                .decode_column::<Option<Decimal>>("credential_grant_revision")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            if origin != grant_origin || revision != expected_revision {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            Some(runner)
        }
        (Some(_), None) => return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into()),
        (None, _) => None,
    };
    sqlx::query(
        "INSERT INTO runner_credential_grant
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, credential_profile_name,
             registration_enrollment_id, registration_revision,
             placement_event_ordinal, prior_runner_id,
             prior_grant_revision, tool_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(grant.profile().as_str())
    .bind(grant_registration.registration.enrollment().into_uuid())
    .bind(Decimal::from(grant_registration.revision.get()))
    .bind(Decimal::from(placement_event))
    .bind(prior_runner)
    .bind(prior)
    .bind(count_decimal(tools.len())?)
    .execute(&mut **transaction)
    .await?;
    for (tool, approval) in tools {
        sqlx::query(
            "INSERT INTO runner_credential_grant_tool
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, credential_profile_name,
                 tool_name, approval_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .bind(grant.profile().as_str())
        .bind(tool.as_str())
        .bind(encode_approval(approval))
        .execute(&mut **transaction)
        .await?;
    }
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         VALUES ($1, $2, $3, $4, 1, $5, $6)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(if grant.revision() == RunnerGeneration::one() {
        "issued"
    } else {
        "replaced"
    })
    .bind(grant.profile().as_str())
    .execute(&mut **transaction)
    .await?;
    if grant.state() == CredentialProfileGrantState::Revoked {
        sqlx::query(
            "INSERT INTO runner_credential_grant_audit
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, audit_ordinal,
                 event_kind, credential_profile_name)
             VALUES ($1, $2, $3, $4, 2, 'revoked', $5)",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .bind(grant.profile().as_str())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn load_placement_registration(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
    let enrollment = row.decode_column::<Option<Uuid>>("registration_enrollment_id")?;
    let revision = row.decode_column::<Option<Decimal>>("registration_revision")?;
    match (enrollment, revision) {
        (None, None) => Ok(None),
        (Some(enrollment), Some(revision)) => load_registration_in(
            connection,
            runner_enrollment_id(enrollment),
            decode_registration_revision(revision)?,
            None,
            catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)
        .map(Some)
        .map_err(Into::into),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn load_permission_overrides(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<RunnerToolPermissionOverrides, RunnerProtocolStoreError> {
    let session = row.decode_column::<Uuid>("session_id")?;
    let event = row.decode_column::<Decimal>("event_ordinal")?;
    let override_rows = sqlx::query(
        "SELECT tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1 AND event_ordinal = $2
          ORDER BY tool_name",
    )
    .bind(session)
    .bind(event)
    .fetch_all(&mut *connection)
    .await?;
    require_count(row, "permission_override_count", override_rows.len())?;
    RunnerToolPermissionOverrides::try_new(
        override_rows
            .iter()
            .map(|override_row| {
                Ok((
                    tool_name(override_row.decode_column("tool_name")?)?,
                    decode_permission_override(override_row.decode_column("permission_kind")?)?,
                ))
            })
            .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn decode_placement(
    connection: &mut PgConnection,
    row: &PgRow,
    registration: Option<&ValidatedRunnerRegistration>,
    profileless_tombstone: Option<&CredentialProfileGrant>,
) -> Result<SessionRunnerPlacement, RunnerProtocolStoreError> {
    let session = session_id(row.decode_column("session_id")?);
    let event = row.decode_column::<Decimal>("event_ordinal")?;
    let placement_revision = decode_generation(row.decode_column("placement_revision")?)?;
    let permission_overrides = load_permission_overrides(connection, row).await?;
    let request = SessionRunnerPlacementRequest {
        selector: decode_selector(row)?,
        working_directory: decode_directory(row)?,
        credential_profile: row
            .decode_column::<Option<String>>("requested_credential_profile_name")?
            .map(profile_name)
            .transpose()?,
        workspace: decode_workspace_requirement(row)?,
        sandbox: decode_sandbox(row.decode_column("requested_sandbox_profile")?)?,
        permission_overrides: permission_overrides.clone(),
    };
    let state_kind: String = row.decode_column("state_kind")?;
    let state = if state_kind == "unpinned" {
        SessionRunnerPlacementState::Unpinned
    } else {
        let tool_rows = sqlx::query(
            "SELECT tool_name, runner_required
               FROM runner_session_placement_tool
              WHERE session_id = $1 AND event_ordinal = $2
              ORDER BY tool_name",
        )
        .bind(session.into_uuid())
        .bind(event)
        .fetch_all(&mut *connection)
        .await?;
        require_count(row, "pinned_tool_count", tool_rows.len())?;
        let tools = tool_rows
            .iter()
            .map(|row| tool_name(row.decode_column("tool_name")?))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let mut runner_required_tools = BTreeSet::new();
        for row in &tool_rows {
            match decode_stored_runner_requirement(row.decode_column::<bool>("runner_required")?) {
                StoredRunnerRequirement::Optional => {}
                StoredRunnerRequirement::Required => {
                    runner_required_tools.insert(tool_name(row.decode_column("tool_name")?)?);
                }
            }
        }
        let runner = row
            .decode_column::<Option<Uuid>>("pinned_runner_id")?
            .map(runner_id)
            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        let directory = row
            .decode_column::<Option<String>>("pinned_working_directory")?
            .map(working_directory)
            .transpose()?
            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        let workspace = decode_provisioned_workspace(row, session, placement_revision, runner)?;
        let pinned = PinnedRunnerPlacement {
            runner,
            working_directory: directory,
            credential_profile: row
                .decode_column::<Option<String>>("pinned_credential_profile_name")?
                .map(profile_name)
                .transpose()?,
            grant_lineage: decode_grant_lineage(row)?,
            tools,
            runner_required_tools,
            workspace,
            sandbox: request.sandbox,
            permission_overrides,
        };
        match state_kind.as_str() {
            "pinned" => SessionRunnerPlacementState::Pinned(pinned),
            "runner_lost" => SessionRunnerPlacementState::RunnerLost(pinned),
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        }
    };
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session,
            revision: placement_revision,
            request,
            state,
        },
        session,
        registration,
        profileless_tombstone,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

fn decode_provisioned_workspace(
    row: &PgRow,
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
) -> Result<Option<ProvisionedWorkspace>, RunnerProtocolStoreError> {
    let repository = row.decode_column::<Option<String>>("workspace_repository_key")?;
    let directory = row.decode_column::<Option<String>>("workspace_working_directory")?;
    let manifest = row.decode_column::<Option<Uuid>>("workspace_manifest_id")?;
    let clone_url_digest = row.decode_column::<Option<String>>("workspace_clone_url_digest")?;
    let credential_profile =
        row.decode_column::<Option<String>>("workspace_credential_profile_name")?;
    let sandbox = row.decode_column::<Option<String>>("workspace_sandbox_profile")?;
    let relative_path = row.decode_column::<Option<String>>("workspace_relative_path")?;
    let recovery_kind = row.decode_column::<Option<String>>("workspace_recovery_kind")?;
    let branch_name = row.decode_column::<Option<String>>("workspace_branch_name")?;
    let revision = row.decode_column::<Option<String>>("workspace_revision")?;
    let any_present = repository.is_some()
        || directory.is_some()
        || manifest.is_some()
        || clone_url_digest.is_some()
        || credential_profile.is_some()
        || sandbox.is_some()
        || relative_path.is_some()
        || recovery_kind.is_some()
        || branch_name.is_some()
        || revision.is_some();
    if !any_present {
        return Ok(None);
    }
    let recovery = match (recovery_kind.as_deref(), branch_name, revision) {
        (None, None, None) => None,
        (Some("commit"), None, Some(revision)) => Some(WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new(revision)
                .map_err(RunnerProtocolStoreError::Domain)?,
        }),
        (Some("branch"), Some(name), Some(revision)) => Some(WorkspaceRecovery::Branch {
            name: WorkspaceBranchName::try_new(name).map_err(RunnerProtocolStoreError::Domain)?,
            revision: WorkspaceRevision::try_new(revision)
                .map_err(RunnerProtocolStoreError::Domain)?,
        }),
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    Ok(Some(ProvisionedWorkspace {
        session,
        placement_revision,
        runner,
        repository: repository.map(repository_key).transpose()?,
        canonical_clone_url_digest: clone_url_digest
            .map(CanonicalCloneUrlDigest::try_new)
            .transpose()
            .map_err(RunnerProtocolStoreError::Domain)?,
        credential_profile: credential_profile.map(profile_name).transpose()?,
        sandbox: decode_sandbox(sandbox.ok_or(RunnerProtocolCorruption::IncompleteInventory)?)?,
        working_directory: working_directory(
            directory.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        )?,
        relative_path: WorkspaceRelativePath::try_new(
            relative_path.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        )
        .map_err(RunnerProtocolStoreError::Domain)?,
        manifest_id: WorkspaceManifestId::from_uuid(
            manifest.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        ),
        recovery,
    }))
}

fn decode_grant_lineage(
    placement: &PgRow,
) -> Result<Option<RunnerCredentialGrantLineage>, RunnerProtocolStoreError> {
    let origin =
        placement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let revision = placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let runner = placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    match (origin, runner, revision) {
        (None, None, None) => Ok(None),
        (Some(_), Some(runner), Some(revision)) => Ok(Some(RunnerCredentialGrantLineage {
            runner: runner_id(runner),
            revision: decode_generation(revision)?,
        })),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn load_grant_for_placement(
    connection: &mut PgConnection,
    placement: &PgRow,
    catalog: &RunnerCatalog,
) -> Result<Option<CredentialProfileGrant>, RunnerProtocolStoreError> {
    let origin =
        placement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let revision = placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let runner = placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    if origin.is_none() && revision.is_none() && runner.is_none() {
        return Ok(None);
    }
    let (Some(origin), Some(revision), Some(runner)) = (origin, revision, runner) else {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    };
    let session = session_id(placement.decode_column("session_id")?);
    let revision = decode_generation(revision)?;
    let row = sqlx::query(
        "SELECT grant_record.*,
                EXISTS (
                    SELECT 1
                      FROM runner_credential_grant_audit AS audit
                     WHERE audit.session_id = grant_record.session_id
                       AND audit.lineage_origin_event_ordinal =
                            grant_record.lineage_origin_event_ordinal
                       AND audit.runner_id = grant_record.runner_id
                       AND audit.grant_revision =
                            grant_record.grant_revision
                       AND audit.event_kind = 'revoked'
                ) AS revoked
           FROM runner_credential_grant AS grant_record
          WHERE grant_record.session_id = $1
            AND grant_record.lineage_origin_event_ordinal = $2
            AND grant_record.runner_id = $3
            AND grant_record.grant_revision = $4",
    )
    .bind(session.into_uuid())
    .bind(origin)
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
    let profile = row.decode_column::<String>("credential_profile_name")?;
    let revoked = decode_stored_grant_revocation(row.decode_column::<bool>("revoked")?);
    let pinned_profile =
        placement.decode_column::<Option<String>>("pinned_credential_profile_name")?;
    if pinned_profile
        .as_ref()
        .is_some_and(|pinned| pinned != &profile)
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let grant_event = row.decode_column::<Decimal>("placement_event_ordinal")?;
    let policy_event = if pinned_profile.is_none() && revoked == StoredGrantRevocation::Revoked {
        grant_event
            .checked_sub(Decimal::from(1_u64))
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?
    } else {
        grant_event
    };
    let policy_placement = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(policy_event)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let grant_sandbox =
        decode_sandbox(policy_placement.decode_column("requested_sandbox_profile")?)?;
    let grant_permission_overrides =
        load_permission_overrides(connection, &policy_placement).await?;
    let grant_registration = load_registration_in(
        connection,
        runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
        decode_registration_revision(row.decode_column("registration_revision")?)?,
        None,
        catalog,
    )
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let tool_rows = sqlx::query(
        "SELECT tool_name, approval_kind
           FROM runner_credential_grant_tool
          WHERE session_id = $1
            AND lineage_origin_event_ordinal = $2
            AND runner_id = $3
            AND grant_revision = $4
          ORDER BY tool_name",
    )
    .bind(session.into_uuid())
    .bind(origin)
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    require_count(&row, "tool_count", tool_rows.len())?;
    let mut tools = BTreeSet::new();
    let mut approvals = BTreeMap::new();
    for tool_row in tool_rows {
        let tool = tool_name(tool_row.decode_column("tool_name")?)?;
        tools.insert(tool.clone());
        approvals.insert(
            tool,
            decode_approval(tool_row.decode_column("approval_kind")?)?,
        );
    }
    CredentialProfileGrant::reconstitute(
        CredentialProfileGrantReconstitutionInput {
            session,
            runner: runner_id(runner),
            revision,
            profile: profile_name(profile)?,
            tools,
            approvals,
            state: match revoked {
                StoredGrantRevocation::Active => CredentialProfileGrantState::Active,
                StoredGrantRevocation::Revoked => CredentialProfileGrantState::Revoked,
            },
        },
        session,
        grant_registration.registration(),
        grant_sandbox,
        &grant_permission_overrides,
    )
    .map(Some)
    .map_err(RunnerProtocolStoreError::Domain)
}

/// Appends one lease generation or state event inside the caller's
/// transaction, exactly as the standalone lease store does.
async fn append_lease_event_in(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let current_event = sqlx::query(RUNNER_LEASE_HEAD)
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .fetch_optional(&mut **transaction)
        .await?;
    let event_ordinal = match current_event {
        None => {
            if lease.state() != RunnerLeaseState::Offered {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ));
            }
            insert_lease_generation(transaction, lease).await?;
            1
        }
        Some(row) => {
            require_stored_lease_identity(&row, lease)?;
            decode_u64(row.decode_column("event_ordinal")?)?
                .checked_add(1)
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?
        }
    };
    let state = encode_lease_state(lease.state());
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .bind(state)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, $2, $3)
         ON CONFLICT (lease_id, generation)
         DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_lease_generation(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let canonical_dispatch = sqlx::query(
        "SELECT session_id, turn_id, issuing_turn_attempt_id,
                request_id, dispatch_generation
           FROM tool_attempt
          WHERE attempt_id = $1",
    )
    .bind(correlation.dispatch.attempt().into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    if canonical_dispatch.decode_column::<Uuid>("session_id")?
        != correlation.dispatch.session().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("turn_id")?
            != correlation.dispatch.turn().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("issuing_turn_attempt_id")?
            != correlation.dispatch.issuing_attempt().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("request_id")?
            != correlation.dispatch.request().into_uuid()
        || canonical_dispatch.decode_column::<Decimal>("dispatch_generation")?
            != Decimal::from(correlation.dispatch.generation().as_u64())
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let placement = sqlx::query(RUNNER_LEASE_PLACEMENT)
        .bind(lease.session().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let placement_runner = placement
        .decode_column::<Option<Uuid>>("pinned_runner_id")?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
    if placement_runner != lease.runner().into_uuid() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let enrollment = placement
        .decode_column::<Option<Uuid>>("registration_enrollment_id")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let enrollment_state: Option<String> = sqlx::query_scalar(RUNNER_LEASE_ENROLLMENT_AUTHORITY)
        .bind(enrollment)
        .fetch_optional(&mut **transaction)
        .await?;
    if enrollment_state.is_none() {
        return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
    }
    let authorization = lease.credential_authorization();
    let authorization_origin = match authorization {
        Some(_) => Some(
            placement
                .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
        ),
        None => None,
    };
    if let Some(authorization) = authorization {
        let profile: Option<String> = sqlx::query_scalar(RUNNER_LEASE_GRANT_AUTHORITY)
            .bind(authorization.session.into_uuid())
            .bind(authorization_origin)
            .bind(authorization.runner.into_uuid())
            .bind(Decimal::from(authorization.grant_revision.get()))
            .fetch_optional(&mut **transaction)
            .await?;
        if profile.as_deref() != Some(authorization.profile.as_str()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
    }
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
             $15
         )",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(lease.session().into_uuid())
    .bind(lease.runner().into_uuid())
    .bind(lease.tool().as_str())
    .bind(encode_effect(lease.effect()))
    .bind(placement.decode_column::<Decimal>("event_ordinal")?)
    .bind(
        placement
            .decode_column::<Option<Uuid>>("registration_enrollment_id")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )
    .bind(
        placement
            .decode_column::<Option<Decimal>>("registration_revision")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )
    .bind(authorization.map(|authorization| authorization.profile.as_str()))
    .bind(authorization_origin)
    .bind(authorization.map(|authorization| Decimal::from(authorization.grant_revision.get())))
    .bind(authorization.map(|authorization| encode_approval(authorization.approval)))
    .bind(
        correlation
            .generation
            .get()
            .checked_sub(1)
            .filter(|value| *value > 0)
            .map(Decimal::from),
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn decode_lease(
    row: &PgRow,
    registration: &ValidatedRunnerRegistration,
) -> Result<RunnerLease, RunnerProtocolStoreError> {
    let lease = runner_lease_id(row.decode_column("lease_id")?);
    let attempt = tool_attempt_id(row.decode_column("attempt_id")?);
    let session = session_id(row.decode_column("session_id")?);
    let runner = runner_id(row.decode_column("runner_id")?);
    let tool = tool_name(row.decode_column("tool_name")?)?;
    let generation = decode_generation(row.decode_column("generation")?)?;
    let canonical_tool = row
        .decode_column::<Option<String>>("canonical_attempt_tool")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session,
            turn: TurnId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_attempt_turn")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            issuing_attempt: TurnAttemptId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_issuing_attempt")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            request: ToolRequestId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_attempt_request")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            attempt,
            generation: decode_dispatch_generation(
                row.decode_column::<Option<Decimal>>("canonical_dispatch_generation")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            )?,
        },
    );
    let canonical_runner = row
        .decode_column::<Option<Uuid>>("canonical_placement_runner")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_placement_state = row
        .decode_column::<Option<String>>("canonical_placement_state")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_registration_enrollment = row
        .decode_column::<Option<Uuid>>("canonical_registration_enrollment")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let canonical_registration_revision = row
        .decode_column::<Option<Decimal>>("canonical_registration_revision")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    if canonical_placement_state != "pinned"
        || canonical_runner != runner.into_uuid()
        || canonical_registration_enrollment
            != row.decode_column::<Uuid>("registration_enrollment_id")?
        || canonical_registration_revision
            != row.decode_column::<Decimal>("registration_revision")?
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let authorization = match (
        row.decode_column::<Option<String>>("credential_profile_name")?,
        row.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?,
        row.decode_column::<Option<Decimal>>("credential_grant_revision")?,
        row.decode_column::<Option<String>>("credential_approval_kind")?,
    ) {
        (None, None, None, None) => {
            if row
                .decode_column::<Option<String>>("canonical_grant_tool")?
                .is_some()
                || row
                    .decode_column::<Option<String>>("canonical_grant_approval")?
                    .is_some()
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            None
        }
        (Some(profile), Some(_), Some(grant_revision), Some(approval)) => {
            let canonical_grant_tool = row
                .decode_column::<Option<String>>("canonical_grant_tool")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let canonical_grant_approval = row
                .decode_column::<Option<String>>("canonical_grant_approval")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            if canonical_grant_tool != tool.as_str() || canonical_grant_approval != approval {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            Some(CredentialDispatchAuthorization {
                session,
                runner,
                grant_revision: decode_generation(grant_revision)?,
                profile: profile_name(profile)?,
                tool: tool.clone(),
                approval: decode_approval(approval)?,
            })
        }
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease,
            dispatch,
            runner,
            tool: tool.clone(),
            effect: decode_effect(row.decode_column("effect_class")?)?,
            credential_authorization: authorization.clone(),
            generation,
            state: decode_lease_state(row.decode_column("state_kind")?)?,
            recorded_correlation: RunnerLeaseCorrelation {
                lease,
                runner: runner_id(canonical_runner),
                tool: tool_name(canonical_tool)?,
                dispatch,
                generation,
            },
            recorded_session: session,
            recorded_effect: decode_effect(row.decode_column("effect_class")?)?,
            recorded_credential_authorization: authorization.clone(),
            recorded_state: decode_lease_state(row.decode_column("state_kind")?)?,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

fn validate_placement_snapshot(
    placement: &SessionRunnerPlacement,
    registration: Option<&StoredValidatedRunnerRegistration>,
    grant: Option<&CredentialProfileGrant>,
) -> Result<(), RunnerProtocolStoreError> {
    let profileless_tombstone = match (placement.state(), grant) {
        (
            SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned),
            Some(grant),
        ) if pinned.credential_profile.is_none()
            && grant.state() == CredentialProfileGrantState::Revoked =>
        {
            Some(grant)
        }
        _ => None,
    };
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session: placement.session(),
            revision: placement.revision(),
            request: placement.request().clone(),
            state: placement.state().clone(),
        },
        placement.session(),
        registration.map(StoredValidatedRunnerRegistration::registration),
        profileless_tombstone,
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    if grant.is_some() && registration.is_none() {
        return Err(RunnerProtocolStoreError::Corruption(
            RunnerProtocolCorruption::MissingCanonicalRegistration,
        ));
    }
    let binding_matches = match (placement.state(), grant) {
        (SessionRunnerPlacementState::Unpinned, None) => true,
        (
            SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned),
            Some(grant),
        ) => match pinned.credential_profile.as_ref() {
            Some(profile) => {
                profile == grant.profile()
                    && placement.session() == grant.session()
                    && pinned.runner == grant.runner()
                    && pinned.grant_lineage == Some(grant.lineage())
            }
            None => {
                placement.session() == grant.session()
                    && grant.state() == CredentialProfileGrantState::Revoked
                    && grant.revision() != RunnerGeneration::one()
                    && pinned.grant_lineage == Some(grant.lineage())
            }
        },
        (
            SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned),
            None,
        ) => pinned.credential_profile.is_none() && pinned.grant_lineage.is_none(),
        (SessionRunnerPlacementState::Unpinned, Some(_)) => false,
    };
    if !binding_matches {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorruptStoredFacts,
        ));
    }
    Ok(())
}

fn grant_input(grant: &CredentialProfileGrant) -> CredentialProfileGrantReconstitutionInput {
    CredentialProfileGrantReconstitutionInput {
        session: grant.session(),
        runner: grant.runner(),
        revision: grant.revision(),
        profile: grant.profile().clone(),
        tools: grant.tools().cloned().collect(),
        approvals: grant
            .approvals()
            .map(|(tool, approval)| (tool.clone(), approval))
            .collect(),
        state: grant.state(),
    }
}

fn require_stored_lease_identity(
    row: &PgRow,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let stored_authorization = match (
        row.decode_column::<Option<String>>("credential_profile_name")?,
        row.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?,
        row.decode_column::<Option<Decimal>>("credential_grant_revision")?,
        row.decode_column::<Option<String>>("credential_approval_kind")?,
    ) {
        (None, None, None, None) => None,
        (Some(profile), Some(_), Some(revision), Some(approval)) => {
            Some(CredentialDispatchAuthorization {
                session: lease.session(),
                runner: lease.runner(),
                grant_revision: decode_generation(revision)?,
                profile: profile_name(profile)?,
                tool: lease.tool().clone(),
                approval: decode_approval(approval)?,
            })
        }
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    if row.decode_column::<Uuid>("attempt_id")? != correlation.dispatch.attempt().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_session")?
            != correlation.dispatch.session().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_turn")?
            != correlation.dispatch.turn().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_issuing_attempt")?
            != correlation.dispatch.issuing_attempt().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_request")?
            != correlation.dispatch.request().into_uuid()
        || row.decode_column::<Decimal>("canonical_dispatch_generation")?
            != Decimal::from(correlation.dispatch.generation().as_u64())
        || row.decode_column::<Uuid>("session_id")? != lease.session().into_uuid()
        || row.decode_column::<Uuid>("runner_id")? != correlation.runner.into_uuid()
        || row.decode_column::<String>("tool_name")? != correlation.tool.as_str()
        || decode_effect(row.decode_column("effect_class")?)? != lease.effect()
        || stored_authorization.as_ref() != lease.credential_authorization()
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

struct EncodedPlacementState<'a> {
    kind: &'static str,
    pinned_runner: Option<Uuid>,
    pinned_directory: Option<&'a str>,
    pinned_profile: Option<&'a str>,
    grant_lineage: Option<RunnerCredentialGrantLineage>,
    tools: Vec<&'a ToolName>,
    runner_required_tools: BTreeSet<&'a ToolName>,
    workspace_repository: Option<&'a str>,
    workspace_directory: Option<&'a str>,
    workspace_manifest: Option<Uuid>,
    workspace_clone_url_digest: Option<&'a str>,
    workspace_credential_profile: Option<&'a str>,
    workspace_sandbox: Option<&'static str>,
    workspace_relative_path: Option<&'a str>,
    workspace_recovery_kind: Option<&'static str>,
    workspace_branch_name: Option<&'a str>,
    workspace_revision: Option<&'a str>,
}

fn encode_placement_state(state: &SessionRunnerPlacementState) -> EncodedPlacementState<'_> {
    let (state_kind, pinned) = match state {
        SessionRunnerPlacementState::Unpinned => {
            return EncodedPlacementState {
                kind: "unpinned",
                pinned_runner: None,
                pinned_directory: None,
                pinned_profile: None,
                grant_lineage: None,
                tools: Vec::new(),
                runner_required_tools: BTreeSet::new(),
                workspace_repository: None,
                workspace_directory: None,
                workspace_manifest: None,
                workspace_clone_url_digest: None,
                workspace_credential_profile: None,
                workspace_sandbox: None,
                workspace_relative_path: None,
                workspace_recovery_kind: None,
                workspace_branch_name: None,
                workspace_revision: None,
            };
        }
        SessionRunnerPlacementState::Pinned(pinned) => ("pinned", pinned),
        SessionRunnerPlacementState::RunnerLost(pinned) => ("runner_lost", pinned),
    };
    let workspace = pinned.workspace.as_ref();
    let (workspace_recovery_kind, workspace_branch_name, workspace_revision) = workspace
        .and_then(|workspace| workspace.recovery.as_ref())
        .map(encode_workspace_recovery)
        .unwrap_or((None, None, None));
    EncodedPlacementState {
        kind: state_kind,
        pinned_runner: Some(pinned.runner.into_uuid()),
        pinned_directory: Some(pinned.working_directory.as_str()),
        pinned_profile: pinned
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
        grant_lineage: pinned.grant_lineage,
        tools: pinned.tools.iter().collect(),
        runner_required_tools: pinned.runner_required_tools.iter().collect(),
        workspace_repository: workspace
            .and_then(|workspace| workspace.repository.as_ref())
            .map(WorkspaceRepositoryKey::as_str),
        workspace_directory: workspace.map(|workspace| workspace.working_directory.as_str()),
        workspace_manifest: workspace.map(|workspace| workspace.manifest_id.into_uuid()),
        workspace_clone_url_digest: workspace
            .and_then(|workspace| workspace.canonical_clone_url_digest.as_ref())
            .map(CanonicalCloneUrlDigest::as_str),
        workspace_credential_profile: workspace
            .and_then(|workspace| workspace.credential_profile.as_ref())
            .map(CredentialProfileName::as_str),
        workspace_sandbox: workspace.map(|workspace| encode_sandbox(workspace.sandbox)),
        workspace_relative_path: workspace.map(|workspace| workspace.relative_path.as_str()),
        workspace_recovery_kind,
        workspace_branch_name,
        workspace_revision,
    }
}

fn encode_workspace_recovery(
    recovery: &WorkspaceRecovery,
) -> (Option<&'static str>, Option<&str>, Option<&str>) {
    match recovery {
        WorkspaceRecovery::Commit { revision } => (Some("commit"), None, Some(revision.as_str())),
        WorkspaceRecovery::Branch { name, revision } => {
            (Some("branch"), Some(name.as_str()), Some(revision.as_str()))
        }
    }
}

fn encode_selector(selector: &RunnerSelector) -> (&'static str, Option<Uuid>, Option<&str>) {
    match selector {
        RunnerSelector::Identity(runner) => ("identity", Some(runner.into_uuid()), None),
        RunnerSelector::CapabilityClass(class) => ("capability_class", None, Some(class.as_str())),
    }
}

fn encode_directory(selection: &WorkingDirectorySelection) -> (&'static str, Option<&str>) {
    match selection {
        WorkingDirectorySelection::RunnerDefault => ("runner_default", None),
        WorkingDirectorySelection::Exact(directory) => ("exact", Some(directory.as_str())),
    }
}

fn encode_workspace_requirement(
    requirement: &WorkspaceRequirement,
) -> (&'static str, Option<&str>) {
    match requirement {
        WorkspaceRequirement::None => ("none", None),
        WorkspaceRequirement::RepositoryWorktree { repository } => {
            ("repository_worktree", Some(repository.as_str()))
        }
    }
}

fn decode_selector(row: &PgRow) -> Result<RunnerSelector, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("selector_kind")?;
    match kind.as_str() {
        "identity" => row
            .decode_column::<Option<Uuid>>("selector_runner_id")?
            .map(runner_id)
            .map(RunnerSelector::Identity)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        "capability_class" => row
            .decode_column::<Option<String>>("selector_capability_class")?
            .map(capability_class)
            .transpose()?
            .map(RunnerSelector::CapabilityClass)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_directory(row: &PgRow) -> Result<WorkingDirectorySelection, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("directory_selection_kind")?;
    match kind.as_str() {
        "runner_default" => Ok(WorkingDirectorySelection::RunnerDefault),
        "exact" => row
            .decode_column::<Option<String>>("requested_working_directory")?
            .map(working_directory)
            .transpose()?
            .map(WorkingDirectorySelection::Exact)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_workspace_requirement(
    row: &PgRow,
) -> Result<WorkspaceRequirement, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("workspace_requirement_kind")?;
    match kind.as_str() {
        "none" => Ok(WorkspaceRequirement::None),
        "repository_worktree" => row
            .decode_column::<Option<String>>("requested_repository_key")?
            .map(repository_key)
            .transpose()?
            .map(|repository| WorkspaceRequirement::RepositoryWorktree { repository })
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

struct EncodedLoci<'a> {
    kind: &'static str,
    selector_kind: &'static str,
    selector_runner: Option<Uuid>,
    selector_class: Option<&'a str>,
}

fn encode_loci(loci: &ToolAdmissibleLoci) -> Result<EncodedLoci<'_>, RunnerProtocolStoreError> {
    match loci {
        ToolAdmissibleLoci::DaemonOnly => Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
        ToolAdmissibleLoci::RunnerOnly { selector } => {
            let (kind, runner, class) = encode_selector(selector);
            Ok(EncodedLoci {
                kind: "runner_only",
                selector_kind: kind,
                selector_runner: runner,
                selector_class: class,
            })
        }
        ToolAdmissibleLoci::DaemonOrRunner { selector } => {
            let (kind, runner, class) = encode_selector(selector);
            Ok(EncodedLoci {
                kind: "daemon_or_runner",
                selector_kind: kind,
                selector_runner: runner,
                selector_class: class,
            })
        }
    }
}

fn decode_tool_declaration(row: &PgRow) -> Result<RunnerToolDeclaration, RunnerProtocolStoreError> {
    let selector = decode_selector(row)?;
    let loci: String = row.decode_column("loci_kind")?;
    let loci = match loci.as_str() {
        "runner_only" => ToolAdmissibleLoci::RunnerOnly { selector },
        "daemon_or_runner" => ToolAdmissibleLoci::DaemonOrRunner { selector },
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let stored_schema: String = row.decode_column("model_input_schema")?;
    let model = RunnerToolModelDefinition::try_new(
        row.decode_column("model_description")?,
        stored_schema.clone(),
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    if model.input_schema().as_str() != stored_schema {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    Ok(RunnerToolDeclaration::new(
        tool_name(row.decode_column("tool_name")?)?,
        model,
        decode_permission(row.decode_column("permission_kind")?)?,
        decode_effect(row.decode_column("effect_class")?)?,
        loci,
    ))
}

const fn encode_permission_override(permission: RunnerToolPermissionOverride) -> &'static str {
    match permission {
        RunnerToolPermissionOverride::Auto => "auto",
        RunnerToolPermissionOverride::Confirm => "confirm",
    }
}

fn decode_permission_override(
    value: String,
) -> Result<RunnerToolPermissionOverride, RunnerProtocolStoreError> {
    match value.as_str() {
        "auto" => Ok(RunnerToolPermissionOverride::Auto),
        "confirm" => Ok(RunnerToolPermissionOverride::Confirm),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_permission(permission: ToolPermissionDefault) -> &'static str {
    match permission {
        ToolPermissionDefault::Auto => "auto",
        ToolPermissionDefault::Confirm => "confirm",
    }
}

fn decode_permission(value: String) -> Result<ToolPermissionDefault, RunnerProtocolStoreError> {
    match value.as_str() {
        "auto" => Ok(ToolPermissionDefault::Auto),
        "confirm" => Ok(ToolPermissionDefault::Confirm),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_effect(effect: RunnerToolEffectClass) -> &'static str {
    match effect {
        RunnerToolEffectClass::Pure => "pure",
        RunnerToolEffectClass::Idempotent => "idempotent",
        RunnerToolEffectClass::SideEffecting => "side_effecting",
    }
}

fn decode_effect(value: String) -> Result<RunnerToolEffectClass, RunnerProtocolStoreError> {
    match value.as_str() {
        "pure" => Ok(RunnerToolEffectClass::Pure),
        "idempotent" => Ok(RunnerToolEffectClass::Idempotent),
        "side_effecting" => Ok(RunnerToolEffectClass::SideEffecting),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_approval(approval: CredentialToolApproval) -> &'static str {
    match approval {
        CredentialToolApproval::Automatic => "automatic",
        CredentialToolApproval::SessionPolicy => "session_policy",
    }
}

fn decode_approval(value: String) -> Result<CredentialToolApproval, RunnerProtocolStoreError> {
    match value.as_str() {
        "automatic" => Ok(CredentialToolApproval::Automatic),
        "session_policy" => Ok(CredentialToolApproval::SessionPolicy),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_sandbox(sandbox: RunnerSandboxProfile) -> &'static str {
    match sandbox {
        RunnerSandboxProfile::Ambient => "ambient",
        RunnerSandboxProfile::WorkspaceRestricted => "workspace_restricted",
    }
}

fn decode_sandbox(value: String) -> Result<RunnerSandboxProfile, RunnerProtocolStoreError> {
    match value.as_str() {
        "ambient" => Ok(RunnerSandboxProfile::Ambient),
        "workspace_restricted" => Ok(RunnerSandboxProfile::WorkspaceRestricted),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_workspace(workspace: WorkspaceCapability) -> &'static str {
    match workspace {
        WorkspaceCapability::WorktreePerSession => "worktree_per_session",
    }
}

fn decode_workspace(value: String) -> Result<WorkspaceCapability, RunnerProtocolStoreError> {
    match value.as_str() {
        "worktree_per_session" => Ok(WorkspaceCapability::WorktreePerSession),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_lease_state(state: RunnerLeaseState) -> &'static str {
    match state {
        RunnerLeaseState::Offered => "offered",
        RunnerLeaseState::Claimed => "claimed",
        RunnerLeaseState::Completed => "completed",
        RunnerLeaseState::LostUnclaimed => "lost_unclaimed",
        RunnerLeaseState::LostClaimed => "lost_claimed",
        RunnerLeaseState::LostExecutionPossible => "lost_execution_possible",
    }
}

fn decode_lease_state(value: String) -> Result<RunnerLeaseState, RunnerProtocolStoreError> {
    match value.as_str() {
        "offered" => Ok(RunnerLeaseState::Offered),
        "claimed" => Ok(RunnerLeaseState::Claimed),
        "completed" => Ok(RunnerLeaseState::Completed),
        "lost_unclaimed" => Ok(RunnerLeaseState::LostUnclaimed),
        "lost_claimed" => Ok(RunnerLeaseState::LostClaimed),
        "lost_execution_possible" => Ok(RunnerLeaseState::LostExecutionPossible),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_enrollment_state(value: &str) -> Result<RunnerEnrollmentState, RunnerProtocolStoreError> {
    match value {
        "active" => Ok(RunnerEnrollmentState::Active),
        "revoked" => Ok(RunnerEnrollmentState::Revoked),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_classes(
    rows: &[PgRow],
) -> Result<BTreeSet<RunnerCapabilityClass>, RunnerProtocolStoreError> {
    rows.iter()
        .map(|row| capability_class(row.decode_column("capability_class")?))
        .collect()
}

fn capability_class(value: String) -> Result<RunnerCapabilityClass, RunnerProtocolStoreError> {
    RunnerCapabilityClass::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn profile_name(value: String) -> Result<CredentialProfileName, RunnerProtocolStoreError> {
    CredentialProfileName::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn working_directory(value: String) -> Result<RunnerWorkingDirectory, RunnerProtocolStoreError> {
    RunnerWorkingDirectory::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn repository_key(value: String) -> Result<WorkspaceRepositoryKey, RunnerProtocolStoreError> {
    WorkspaceRepositoryKey::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn tool_name(value: String) -> Result<ToolName, RunnerProtocolStoreError> {
    ToolName::try_new(value).map_err(|_| RunnerProtocolCorruption::InvalidEncoding.into())
}

fn require_count(
    row: &PgRow,
    column: &'static str,
    actual: usize,
) -> Result<(), RunnerProtocolStoreError> {
    if row.decode_column::<Decimal>(column)? == Decimal::from(actual) {
        Ok(())
    } else {
        Err(RunnerProtocolCorruption::IncompleteInventory.into())
    }
}

fn count_decimal(value: usize) -> Result<Decimal, RunnerProtocolStoreError> {
    let value = u64::try_from(value).map_err(|_| RunnerProtocolCorruption::GenerationExhausted)?;
    Ok(Decimal::from(value))
}

fn decode_u64(value: Decimal) -> Result<u64, RunnerProtocolStoreError> {
    value
        .to_u64()
        .filter(|decoded| Decimal::from(*decoded) == value)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_generation(value: Decimal) -> Result<RunnerGeneration, RunnerProtocolStoreError> {
    RunnerGeneration::try_from_u64(decode_u64(value)?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_dispatch_generation(
    value: Decimal,
) -> Result<ToolDispatchGeneration, RunnerProtocolStoreError> {
    let value = decode_u64(value)?;
    ToolDispatchGeneration::try_from_u64(value)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_registration_revision(
    value: Decimal,
) -> Result<RunnerRegistrationRevision, RunnerProtocolStoreError> {
    RunnerRegistrationRevision::try_from_u64(decode_u64(value)?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

async fn begin_repeatable_read(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn commit_mutation(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), RunnerProtocolStoreError> {
    transaction
        .commit()
        .await
        .map_err(classify_mutating_commit_error)
}

fn classify_mutating_commit_error(error: sqlx::Error) -> RunnerProtocolStoreError {
    if crate::commit_failure_is_ambiguous(&error) {
        RunnerProtocolStoreError::CommitAmbiguous(error)
    } else {
        RunnerProtocolStoreError::Database(error)
    }
}

trait RunnerProtocolRow {
    fn decode_column<'row, T>(
        &'row self,
        column: &'static str,
    ) -> Result<T, RunnerProtocolStoreError>
    where
        T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>;
}

impl RunnerProtocolRow for PgRow {
    fn decode_column<'row, T>(
        &'row self,
        column: &'static str,
    ) -> Result<T, RunnerProtocolStoreError>
    where
        T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>,
    {
        match Row::try_get(self, column) {
            Ok(value) => Ok(value),
            Err(sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_)) => {
                Err(RunnerProtocolCorruption::InvalidColumn(column).into())
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredGrantRevocation {
    Active,
    Revoked,
}

const fn decode_stored_grant_revocation(value: bool) -> StoredGrantRevocation {
    match value {
        false => StoredGrantRevocation::Active,
        true => StoredGrantRevocation::Revoked,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredRunnerRequirement {
    Optional,
    Required,
}

const fn decode_stored_runner_requirement(value: bool) -> StoredRunnerRequirement {
    match value {
        false => StoredRunnerRequirement::Optional,
        true => StoredRunnerRequirement::Required,
    }
}

const fn runner_enrollment_id(value: Uuid) -> RunnerEnrollmentId {
    RunnerEnrollmentId::from_uuid(value)
}

const fn runner_id(value: Uuid) -> RunnerId {
    RunnerId::from_uuid(value)
}

const fn runner_authentication_id(value: Uuid) -> RunnerAuthenticationId {
    RunnerAuthenticationId::from_uuid(value)
}

const fn runner_lease_id(value: Uuid) -> RunnerLeaseId {
    RunnerLeaseId::from_uuid(value)
}

const fn tool_attempt_id(value: Uuid) -> ToolAttemptId {
    ToolAttemptId::from_uuid(value)
}

const fn session_id(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

/// A durable runner-protocol shape that cannot reconstruct domain state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerProtocolCorruption {
    /// Canonical enrollment state is absent.
    MissingCanonicalEnrollment,
    /// Canonical audit evidence is absent.
    MissingCanonicalAudit,
    /// Canonical registration state is absent.
    MissingCanonicalRegistration,
    /// Canonical placement state is absent.
    MissingCanonicalPlacement,
    /// Canonical credential-grant state is absent.
    MissingCanonicalGrant,
    /// Canonical tool-attempt state is absent.
    MissingCanonicalAttempt,
    /// A declared count disagrees with its durable members.
    IncompleteInventory,
    /// Correlated durable records identify different domain values.
    CrossWiredReference,
    /// A projected column cannot decode to its expected Rust type.
    InvalidColumn(&'static str),
    /// A stored scalar cannot construct its closed domain value.
    InvalidEncoding,
    /// A durable generation cannot advance without overflow.
    GenerationExhausted,
}

impl fmt::Display for RunnerProtocolCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCanonicalEnrollment => {
                formatter.write_str("canonical runner enrollment is missing")
            }
            Self::MissingCanonicalAudit => {
                formatter.write_str("canonical runner audit evidence is missing")
            }
            Self::MissingCanonicalRegistration => {
                formatter.write_str("canonical runner registration is missing")
            }
            Self::MissingCanonicalPlacement => {
                formatter.write_str("canonical runner placement is missing")
            }
            Self::MissingCanonicalGrant => {
                formatter.write_str("canonical credential grant is missing")
            }
            Self::MissingCanonicalAttempt => {
                formatter.write_str("canonical physical tool attempt is missing")
            }
            Self::IncompleteInventory => {
                formatter.write_str("stored runner inventory is incomplete")
            }
            Self::CrossWiredReference => {
                formatter.write_str("stored runner references are cross-wired")
            }
            Self::InvalidColumn(column) => {
                write!(
                    formatter,
                    "stored runner column {column} has an invalid value"
                )
            }
            Self::InvalidEncoding => formatter.write_str("stored runner encoding is invalid"),
            Self::GenerationExhausted => {
                formatter.write_str("stored runner generation is exhausted")
            }
        }
    }
}

impl Error for RunnerProtocolCorruption {}

/// A database, durable-shape, or domain-admission failure.
#[derive(Debug)]
pub enum RunnerProtocolStoreError {
    /// PostgreSQL failed before a commit could have succeeded.
    Database(sqlx::Error),
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(sqlx::Error),
    /// Durable records cannot reconstruct the admitted runner state.
    Corruption(RunnerProtocolCorruption),
    /// Complete values fail a domain-owned runner transition or invariant.
    Domain(RunnerDomainError),
}

impl fmt::Display for RunnerProtocolStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "runner-protocol database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "runner-protocol commit outcome is ambiguous: {error}"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::Domain(error) => write!(formatter, "runner-protocol domain failure: {error:?}"),
        }
    }
}

impl Error for RunnerProtocolStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::Domain(_) => None,
        }
    }
}

impl From<sqlx::Error> for RunnerProtocolStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RunnerProtocolCorruption> for RunnerProtocolStoreError {
    fn from(error: RunnerProtocolCorruption) -> Self {
        Self::Corruption(error)
    }
}
