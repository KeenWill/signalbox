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
};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_domain::{
    CredentialDispatchAuthorization, CredentialProfileGrant,
    CredentialProfileGrantReconstitutionInput, CredentialProfileGrantState, CredentialProfileName,
    CredentialProfilePolicy, CredentialToolApproval, PinnedRunnerPlacement, ProvisionedWorkspace,
    RunnerAuthenticationId, RunnerCapabilityClass, RunnerDomainError, RunnerEnrollment,
    RunnerEnrollmentId, RunnerEnrollmentReconstitutionInput, RunnerEnrollmentState,
    RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseId,
    RunnerLeaseReconstitutionInput, RunnerLeaseState, RunnerSelector, RunnerToolDeclaration,
    RunnerToolEffectClass, RunnerToolModelDefinition, RunnerWorkingDirectory, SessionId,
    SessionRunnerPin, SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, ToolAdmissibleLoci,
    ToolAttemptDispatchCorrelation, ToolAttemptDispatchCorrelationReconstitutionInput,
    ToolAttemptId, ToolDispatchGeneration, ToolName, ToolPermissionDefault, ToolRequestId,
    TurnAttemptId, TurnId, ValidatedRunnerRegistration,
    ValidatedRunnerRegistrationReconstitutionInput, WorkingDirectorySelection, WorkspaceCapability,
    WorkspaceRepositoryKey, WorkspaceRequirement,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::lock_inventory::{
    RUNNER_ENROLLMENT, RUNNER_LEASE_HEAD, RUNNER_LEASE_PLACEMENT, RUNNER_PLACEMENT_HEAD,
    RUNNER_REGISTRATION_HEAD,
};

/// Adapter-owned positive revision of one validated registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerRegistrationRevision(NonZeroU64);

impl RunnerRegistrationRevision {
    pub const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

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
    pub const fn revision(&self) -> RunnerRegistrationRevision {
        self.revision
    }

    pub const fn registration(&self) -> &ValidatedRunnerRegistration {
        &self.registration
    }
}

/// One canonical placement record and its adapter event ordinal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSessionRunnerPlacement {
    event_ordinal: u64,
    placement: SessionRunnerPlacement,
    grant: Option<CredentialProfileGrant>,
}

impl StoredSessionRunnerPlacement {
    pub const fn event_ordinal(&self) -> u64 {
        self.event_ordinal
    }

    pub const fn placement(&self) -> &SessionRunnerPlacement {
        &self.placement
    }

    pub const fn grant(&self) -> Option<&CredentialProfileGrant> {
        self.grant.as_ref()
    }
}

/// PostgreSQL adapter for runner-protocol state.
#[derive(Clone, Debug)]
pub struct RunnerProtocolStore {
    pool: PgPool,
}

impl RunnerProtocolStore {
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts one active logical enrollment and its exact allowed classes.
    pub async fn insert_enrollment(
        &self,
        enrollment: &RunnerEnrollment,
    ) -> Result<(), RunnerProtocolStoreError> {
        if enrollment.state() != RunnerEnrollmentState::Active {
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
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerEnrollment>, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            transaction.rollback().await?;
            return Ok(None);
        }
        let current = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let runner = current.runner();
        let authentication = current.authentication();
        let classes: Vec<_> = current.allowed_classes().cloned().collect();
        let revoked = current.revoke().map_err(RunnerProtocolStoreError::Domain)?;
        sqlx::query(
            "INSERT INTO runner_enrollment_audit
                (enrollment_id, revision, runner_id,
                 authentication_reference_id, allowed_class_count, state_kind)
             VALUES ($1, 2, $2, $3, $4, 'revoked')",
        )
        .bind(enrollment.into_uuid())
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
            .bind(enrollment.into_uuid())
            .bind(class.as_str())
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "UPDATE runner_enrollment
                SET revision = 2, state_kind = 'revoked'
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.into_uuid())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(revoked))
    }

    /// Validates and appends one complete availability advertisement.
    pub async fn register(
        &self,
        enrollment: RunnerEnrollmentId,
        advertisement: signalbox_domain::RunnerAdvertisement,
        catalog: &signalbox_domain::RunnerCatalog,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalEnrollment,
            ));
        }
        let canonical = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let registration = canonical
            .register(advertisement, catalog)
            .map_err(RunnerProtocolStoreError::Domain)?;
        let previous: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let revision = match previous {
            Some(value) => decode_registration_revision(value)?.checked_next().ok_or(
                RunnerProtocolStoreError::Corruption(RunnerProtocolCorruption::GenerationExhausted),
            )?,
            None => RunnerRegistrationRevision::first(),
        };
        insert_registration(&mut transaction, revision, &registration).await?;
        sqlx::query(
            "INSERT INTO runner_current_registration
                (enrollment_id, registration_revision)
             VALUES ($1, $2)
             ON CONFLICT (enrollment_id)
             DO UPDATE SET registration_revision = EXCLUDED.registration_revision",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(revision.get()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(StoredValidatedRunnerRegistration {
            revision,
            registration,
        })
    }

    /// Loads one exact historical validated registration.
    pub async fn load_registration(
        &self,
        enrollment: RunnerEnrollmentId,
        revision: RunnerRegistrationRevision,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = load_registration_in(transaction.as_mut(), enrollment, revision).await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Loads the current validated registration for an enrollment.
    pub async fn load_current_registration(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let loaded = match revision {
            Some(revision) => {
                load_registration_in(
                    transaction.as_mut(),
                    enrollment,
                    decode_registration_revision(revision)?,
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
    ) -> Result<StoredSessionRunnerPlacement, RunnerProtocolStoreError> {
        validate_placement_snapshot(placement, registration, grant)?;

        let mut transaction = self.pool.begin().await?;
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(placement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = prior
            .as_ref()
            .map(|row| decode_u64(row.get("event_ordinal")))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), placement)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            placement,
            registration,
            grant,
        )
        .await?;
        if let (Some(grant), Some(registration)) = (grant, registration) {
            insert_grant_if_new(&mut transaction, event_ordinal, grant, registration).await?;
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
        Ok(StoredSessionRunnerPlacement {
            event_ordinal,
            placement: placement.clone(),
            grant: grant.cloned(),
        })
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
            .map(|row| decode_u64(row.get("event_ordinal")))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), &pin.placement)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            &pin.placement,
            Some(registration),
            pin.grant.as_ref(),
        )
        .await?;
        if let Some(grant) = pin.grant.as_ref() {
            insert_grant_if_new(&mut transaction, event_ordinal, grant, registration).await?;
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
        let event_ordinal = decode_u64(row.get("event_ordinal"))?;
        let registration = load_placement_registration(transaction.as_mut(), &row).await?;
        let placement = decode_placement(
            transaction.as_mut(),
            &row,
            registration
                .as_ref()
                .map(StoredValidatedRunnerRegistration::registration),
        )
        .await?;
        let grant = match registration.as_ref() {
            Some(registration) => {
                load_grant_for_placement(transaction.as_mut(), &row, registration.registration())
                    .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(Some(StoredSessionRunnerPlacement {
            event_ordinal,
            placement,
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
        sqlx::query(
            "INSERT INTO runner_credential_grant_audit
                (session_id, runner_id, grant_revision, audit_ordinal,
                 event_kind, credential_profile_name)
             VALUES ($1, $2, $3, 2, 'revoked', $4)",
        )
        .bind(session.into_uuid())
        .bind(runner.into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(revoked.profile().as_str())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(revoked))
    }

    /// Appends an offered lease generation or a later state event.
    pub async fn store_lease(&self, lease: &RunnerLease) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let correlation = lease.correlation();
        let current_event = sqlx::query(RUNNER_LEASE_HEAD)
            .bind(correlation.lease.into_uuid())
            .bind(Decimal::from(correlation.generation.get()))
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = match current_event {
            None => {
                if lease.state() != RunnerLeaseState::Offered {
                    return Err(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::InvalidState,
                    ));
                }
                insert_lease_generation(&mut transaction, lease).await?;
                1
            }
            Some(row) => {
                require_stored_lease_identity(&row, lease)?;
                decode_u64(row.get("event_ordinal"))?
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
        .execute(&mut *transaction)
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
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await
    }

    /// Loads one exact lease generation and independently joined fence.
    pub async fn load_lease(
        &self,
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
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode_lease(&row)).transpose()
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
                audit.state_kind AS audit_state_kind
           FROM runner_enrollment AS enrollment
           LEFT JOIN runner_enrollment_audit AS audit
             ON audit.enrollment_id = enrollment.enrollment_id
            AND audit.revision = enrollment.revision
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
    if Decimal::from(class_rows.len()) != row.get::<Decimal, _>("allowed_class_count") {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let audit_count = row
        .try_get::<Option<Decimal>, _>("audit_allowed_class_count")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    if Decimal::from(audit_class_rows.len()) != audit_count {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let classes = decode_classes(&class_rows)?;
    let audit_classes = decode_classes(&audit_class_rows)?;
    let state = decode_enrollment_state(row.get("state_kind"))?;
    let audit_state = row
        .try_get::<Option<String>, _>("audit_state_kind")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    let audit_state = decode_enrollment_state(&audit_state)?;
    RunnerEnrollment::reconstitute(RunnerEnrollmentReconstitutionInput {
        enrollment,
        recorded_enrollment: runner_enrollment_id(row.get("enrollment_id")),
        runner: runner_id(row.get("runner_id")),
        recorded_runner: runner_id(
            row.try_get::<Option<Uuid>, _>("audit_runner_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        authentication: runner_authentication_id(row.get("authentication_reference_id")),
        recorded_authentication: runner_authentication_id(
            row.try_get::<Option<Uuid>, _>("audit_authentication_reference_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        allowed_classes: classes,
        recorded_allowed_classes: audit_classes,
        state,
        recorded_state: audit_state,
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
    sqlx::query(
        "INSERT INTO runner_registration
            (enrollment_id, registration_revision, runner_id,
             authentication_reference_id, class_count, tool_count,
             profile_count, workspace_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(registration.enrollment().into_uuid())
    .bind(Decimal::from(revision.get()))
    .bind(registration.runner().into_uuid())
    .bind(registration.authentication().into_uuid())
    .bind(count_decimal(classes.len())?)
    .bind(count_decimal(tools.len())?)
    .bind(count_decimal(profiles.len())?)
    .bind(count_decimal(workspaces.len())?)
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
    Ok(())
}

async fn load_registration_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    revision: RunnerRegistrationRevision,
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
    require_count(&row, "class_count", class_rows.len())?;
    require_count(&row, "tool_count", tool_rows.len())?;
    require_count(&row, "profile_count", profile_rows.len())?;
    require_count(&row, "workspace_count", workspace_rows.len())?;
    let classes = decode_classes(&class_rows)?;
    let tools = tool_rows
        .iter()
        .map(decode_tool_declaration)
        .collect::<Result<Vec<_>, _>>()?;
    let mut profiles = Vec::with_capacity(profile_rows.len());
    for profile in profile_rows {
        let name = profile_name(profile.get("credential_profile_name"))?;
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
                    tool_name(row.get("tool_name"))?,
                    decode_approval(row.get("approval_kind"))?,
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
        .map(|row| decode_workspace(row.get("workspace_kind")))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let registration = ValidatedRunnerRegistration::reconstitute(
        &canonical,
        ValidatedRunnerRegistrationReconstitutionInput {
            enrollment: runner_enrollment_id(row.get("enrollment_id")),
            runner: runner_id(row.get("runner_id")),
            authentication: runner_authentication_id(row.get("authentication_reference_id")),
            classes,
            tools,
            profiles,
            workspaces,
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
    let prior_revision = decode_generation(prior.get("placement_revision"))?;
    let prior_state: String = prior.get("state_kind");
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

async fn insert_placement_record(
    transaction: &mut Transaction<'_, Postgres>,
    event_ordinal: u64,
    event_kind: &str,
    placement: &SessionRunnerPlacement,
    registration: Option<&StoredValidatedRunnerRegistration>,
    grant: Option<&CredentialProfileGrant>,
) -> Result<(), RunnerProtocolStoreError> {
    let request = placement.request();
    let (selector_kind, selector_runner, selector_class) = encode_selector(&request.selector);
    let (directory_kind, requested_directory) = encode_directory(&request.working_directory);
    let (workspace_kind, requested_repository) = encode_workspace_requirement(&request.workspace);
    let state = encode_placement_state(placement.state());
    let (registration_enrollment, registration_revision) = registration
        .map(|registration| {
            (
                Some(registration.registration.enrollment().into_uuid()),
                Some(Decimal::from(registration.revision.get())),
            )
        })
        .unwrap_or((None, None));
    sqlx::query(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, state_kind, pinned_runner_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, credential_grant_revision)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
             $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22
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
    .bind(state.kind)
    .bind(state.pinned_runner)
    .bind(state.pinned_directory)
    .bind(state.pinned_profile)
    .bind(registration_enrollment)
    .bind(registration_revision)
    .bind(count_decimal(state.tools.len())?)
    .bind(state.workspace_repository)
    .bind(state.workspace_directory)
    .bind(grant.map(|grant| Decimal::from(grant.revision().get())))
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
    Ok(())
}

async fn insert_grant_if_new(
    transaction: &mut Transaction<'_, Postgres>,
    placement_event: u64,
    grant: &CredentialProfileGrant,
    registration: &StoredValidatedRunnerRegistration,
) -> Result<(), RunnerProtocolStoreError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM runner_credential_grant
              WHERE session_id = $1
                AND runner_id = $2
                AND grant_revision = $3
         )",
    )
    .bind(grant.session().into_uuid())
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
                           AND audit.runner_id = grant_record.runner_id
                           AND audit.grant_revision =
                                grant_record.grant_revision
                           AND audit.event_kind = 'revoked'
                    ) AS revoked
               FROM runner_credential_grant AS grant_record
              WHERE grant_record.session_id = $1
                AND grant_record.runner_id = $2
                AND grant_record.grant_revision = $3",
        )
        .bind(grant.session().into_uuid())
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_one(&mut **transaction)
        .await?;
        let tool_rows = sqlx::query(
            "SELECT tool_name, approval_kind
               FROM runner_credential_grant_tool
              WHERE session_id = $1
                AND runner_id = $2
                AND grant_revision = $3
              ORDER BY tool_name",
        )
        .bind(grant.session().into_uuid())
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_all(&mut **transaction)
        .await?;
        require_count(&row, "tool_count", tool_rows.len())?;
        let mut approvals = BTreeMap::new();
        for tool_row in tool_rows {
            approvals.insert(
                tool_name(tool_row.get("tool_name"))?,
                decode_approval(tool_row.get("approval_kind"))?,
            );
        }
        let expected_approvals: BTreeMap<_, _> = grant
            .approvals()
            .map(|(tool, approval)| (tool.clone(), approval))
            .collect();
        let stored_state = if row.get::<bool, _>("revoked") {
            CredentialProfileGrantState::Revoked
        } else {
            CredentialProfileGrantState::Active
        };
        if row.get::<String, _>("credential_profile_name") != grant.profile().as_str()
            || runner_enrollment_id(row.get("registration_enrollment_id"))
                != registration.registration.enrollment()
            || decode_registration_revision(row.get("registration_revision"))?
                != registration.revision
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
    let prior_runner: Option<Uuid> = match prior {
        Some(prior) => Some(
            sqlx::query_scalar(
                "SELECT runner_id
                   FROM runner_credential_grant
                  WHERE session_id = $1 AND grant_revision = $2",
            )
            .bind(grant.session().into_uuid())
            .bind(prior)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
        ),
        None => None,
    };
    sqlx::query(
        "INSERT INTO runner_credential_grant
            (session_id, runner_id, grant_revision,
             credential_profile_name, registration_enrollment_id,
             registration_revision, placement_event_ordinal,
             prior_runner_id, prior_grant_revision, tool_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(grant.profile().as_str())
    .bind(registration.registration.enrollment().into_uuid())
    .bind(Decimal::from(registration.revision.get()))
    .bind(Decimal::from(placement_event))
    .bind(prior_runner)
    .bind(prior)
    .bind(count_decimal(tools.len())?)
    .execute(&mut **transaction)
    .await?;
    for (tool, approval) in tools {
        sqlx::query(
            "INSERT INTO runner_credential_grant_tool
                (session_id, runner_id, grant_revision,
                 credential_profile_name, tool_name, approval_kind)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(grant.session().into_uuid())
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
            (session_id, runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         VALUES ($1, $2, $3, 1, $4, $5)",
    )
    .bind(grant.session().into_uuid())
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
    Ok(())
}

async fn load_placement_registration(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
    let enrollment = row.try_get::<Option<Uuid>, _>("registration_enrollment_id")?;
    let revision = row.try_get::<Option<Decimal>, _>("registration_revision")?;
    match (enrollment, revision) {
        (None, None) => Ok(None),
        (Some(enrollment), Some(revision)) => load_registration_in(
            connection,
            runner_enrollment_id(enrollment),
            decode_registration_revision(revision)?,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)
        .map(Some)
        .map_err(Into::into),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn decode_placement(
    connection: &mut PgConnection,
    row: &PgRow,
    registration: Option<&ValidatedRunnerRegistration>,
) -> Result<SessionRunnerPlacement, RunnerProtocolStoreError> {
    let session = session_id(row.get("session_id"));
    let event = row.get::<Decimal, _>("event_ordinal");
    let request = SessionRunnerPlacementRequest {
        selector: decode_selector(row)?,
        working_directory: decode_directory(row)?,
        credential_profile: row
            .try_get::<Option<String>, _>("requested_credential_profile_name")?
            .map(profile_name)
            .transpose()?,
        workspace: decode_workspace_requirement(row)?,
    };
    let state_kind: String = row.get("state_kind");
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
            .map(|row| tool_name(row.get("tool_name")))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let runner_required_tools = tool_rows
            .iter()
            .filter(|row| row.get::<bool, _>("runner_required"))
            .map(|row| tool_name(row.get("tool_name")))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let runner = row
            .try_get::<Option<Uuid>, _>("pinned_runner_id")?
            .map(runner_id)
            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        let directory = row
            .try_get::<Option<String>, _>("pinned_working_directory")?
            .map(working_directory)
            .transpose()?
            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        let workspace = match (
            row.try_get::<Option<String>, _>("workspace_repository_key")?,
            row.try_get::<Option<String>, _>("workspace_working_directory")?,
        ) {
            (None, None) => None,
            (Some(repository), Some(workspace_directory)) => Some(ProvisionedWorkspace {
                session,
                runner,
                repository: repository_key(repository)?,
                working_directory: working_directory(workspace_directory)?,
            }),
            _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
        };
        let pinned = PinnedRunnerPlacement {
            runner,
            working_directory: directory,
            credential_profile: row
                .try_get::<Option<String>, _>("pinned_credential_profile_name")?
                .map(profile_name)
                .transpose()?,
            tools,
            runner_required_tools,
            workspace,
        };
        match state_kind.as_str() {
            "pinned" => SessionRunnerPlacementState::Pinned(pinned),
            "runner_lost" => SessionRunnerPlacementState::RunnerLost(pinned),
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        }
    };
    SessionRunnerPlacement::reconstitute_from_facts(
        SessionRunnerPlacementReconstitutionInput {
            session,
            revision: decode_generation(row.get("placement_revision"))?,
            request,
            state,
        },
        registration,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn load_grant_for_placement(
    connection: &mut PgConnection,
    placement: &PgRow,
    registration: &ValidatedRunnerRegistration,
) -> Result<Option<CredentialProfileGrant>, RunnerProtocolStoreError> {
    let revision = placement.try_get::<Option<Decimal>, _>("credential_grant_revision")?;
    let runner = placement.try_get::<Option<Uuid>, _>("pinned_runner_id")?;
    let profile = placement.try_get::<Option<String>, _>("pinned_credential_profile_name")?;
    if revision.is_none() && profile.is_none() {
        return Ok(None);
    }
    let (Some(revision), Some(runner), Some(profile)) = (revision, runner, profile) else {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    };
    let session = session_id(placement.get("session_id"));
    let revision = decode_generation(revision)?;
    let row = sqlx::query(
        "SELECT grant_record.*,
                EXISTS (
                    SELECT 1
                      FROM runner_credential_grant_audit AS audit
                     WHERE audit.session_id = grant_record.session_id
                       AND audit.runner_id = grant_record.runner_id
                       AND audit.grant_revision =
                            grant_record.grant_revision
                       AND audit.event_kind = 'revoked'
                ) AS revoked
           FROM runner_credential_grant AS grant_record
          WHERE grant_record.session_id = $1
            AND grant_record.runner_id = $2
            AND grant_record.grant_revision = $3",
    )
    .bind(session.into_uuid())
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
    if row.get::<String, _>("credential_profile_name") != profile {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let tool_rows = sqlx::query(
        "SELECT tool_name, approval_kind
           FROM runner_credential_grant_tool
          WHERE session_id = $1 AND runner_id = $2 AND grant_revision = $3
          ORDER BY tool_name",
    )
    .bind(session.into_uuid())
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    require_count(&row, "tool_count", tool_rows.len())?;
    let mut tools = BTreeSet::new();
    let mut approvals = BTreeMap::new();
    for tool_row in tool_rows {
        let tool = tool_name(tool_row.get("tool_name"))?;
        tools.insert(tool.clone());
        approvals.insert(tool, decode_approval(tool_row.get("approval_kind"))?);
    }
    CredentialProfileGrant::reconstitute(
        CredentialProfileGrantReconstitutionInput {
            session,
            runner: runner_id(runner),
            revision,
            profile: profile_name(profile)?,
            tools,
            approvals,
            state: if row.get::<bool, _>("revoked") {
                CredentialProfileGrantState::Revoked
            } else {
                CredentialProfileGrantState::Active
            },
        },
        session,
        registration,
    )
    .map(Some)
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn insert_lease_generation(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let placement = sqlx::query(RUNNER_LEASE_PLACEMENT)
        .bind(lease.session().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let placement_runner = placement
        .try_get::<Option<Uuid>, _>("pinned_runner_id")?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
    if placement_runner != lease.runner().into_uuid() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let authorization = lease.credential_authorization();
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             credential_profile_name, credential_grant_revision,
             credential_approval_kind, predecessor_generation)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
         )",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(lease.session().into_uuid())
    .bind(lease.runner().into_uuid())
    .bind(lease.tool().as_str())
    .bind(encode_effect(lease.effect()))
    .bind(placement.get::<Decimal, _>("event_ordinal"))
    .bind(
        placement
            .try_get::<Option<Uuid>, _>("registration_enrollment_id")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )
    .bind(
        placement
            .try_get::<Option<Decimal>, _>("registration_revision")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )
    .bind(authorization.map(|authorization| authorization.profile.as_str()))
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

fn decode_lease(row: &PgRow) -> Result<RunnerLease, RunnerProtocolStoreError> {
    let lease = runner_lease_id(row.get("lease_id"));
    let attempt = tool_attempt_id(row.get("attempt_id"));
    let session = session_id(row.get("session_id"));
    let runner = runner_id(row.get("runner_id"));
    let tool = tool_name(row.get("tool_name"))?;
    let generation = decode_generation(row.get("generation"))?;
    let canonical_tool = row
        .try_get::<Option<String>, _>("canonical_attempt_tool")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session,
            turn: TurnId::from_uuid(
                row.try_get::<Option<Uuid>, _>("canonical_attempt_turn")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            issuing_attempt: TurnAttemptId::from_uuid(
                row.try_get::<Option<Uuid>, _>("canonical_issuing_attempt")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            request: ToolRequestId::from_uuid(
                row.try_get::<Option<Uuid>, _>("canonical_attempt_request")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            attempt,
            generation: decode_dispatch_generation(
                row.try_get::<Option<Decimal>, _>("canonical_dispatch_generation")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            )?,
        },
    );
    let canonical_runner = row
        .try_get::<Option<Uuid>, _>("canonical_placement_runner")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_placement_state = row
        .try_get::<Option<String>, _>("canonical_placement_state")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_registration_enrollment = row
        .try_get::<Option<Uuid>, _>("canonical_registration_enrollment")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let canonical_registration_revision = row
        .try_get::<Option<Decimal>, _>("canonical_registration_revision")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    if canonical_placement_state != "pinned"
        || canonical_runner != runner.into_uuid()
        || canonical_registration_enrollment != row.get::<Uuid, _>("registration_enrollment_id")
        || canonical_registration_revision != row.get::<Decimal, _>("registration_revision")
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let authorization = match (
        row.try_get::<Option<String>, _>("credential_profile_name")?,
        row.try_get::<Option<Decimal>, _>("credential_grant_revision")?,
        row.try_get::<Option<String>, _>("credential_approval_kind")?,
    ) {
        (None, None, None) => {
            if row
                .try_get::<Option<String>, _>("canonical_grant_tool")?
                .is_some()
                || row
                    .try_get::<Option<String>, _>("canonical_grant_approval")?
                    .is_some()
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            None
        }
        (Some(profile), Some(grant_revision), Some(approval)) => {
            let canonical_grant_tool = row
                .try_get::<Option<String>, _>("canonical_grant_tool")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let canonical_grant_approval = row
                .try_get::<Option<String>, _>("canonical_grant_approval")?
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
    RunnerLease::reconstitute(RunnerLeaseReconstitutionInput {
        lease,
        dispatch,
        runner,
        tool: tool.clone(),
        effect: decode_effect(row.get("effect_class"))?,
        credential_authorization: authorization.clone(),
        generation,
        state: decode_lease_state(row.get("state_kind"))?,
        recorded_correlation: RunnerLeaseCorrelation {
            lease,
            runner: runner_id(canonical_runner),
            tool: tool_name(canonical_tool)?,
            dispatch,
            generation,
        },
        recorded_session: session,
        recorded_effect: decode_effect(row.get("effect_class"))?,
        recorded_credential_authorization: authorization.clone(),
        recorded_state: decode_lease_state(row.get("state_kind"))?,
    })
    .map_err(RunnerProtocolStoreError::Domain)
}

fn validate_placement_snapshot(
    placement: &SessionRunnerPlacement,
    registration: Option<&StoredValidatedRunnerRegistration>,
    grant: Option<&CredentialProfileGrant>,
) -> Result<(), RunnerProtocolStoreError> {
    SessionRunnerPlacement::reconstitute_from_facts(
        SessionRunnerPlacementReconstitutionInput {
            session: placement.session(),
            revision: placement.revision(),
            request: placement.request().clone(),
            state: placement.state().clone(),
        },
        registration.map(StoredValidatedRunnerRegistration::registration),
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    match (grant, registration) {
        (Some(grant), Some(registration)) => {
            CredentialProfileGrant::reconstitute(
                grant_input(grant),
                placement.session(),
                registration.registration(),
            )
            .map_err(RunnerProtocolStoreError::Domain)?;
        }
        (Some(_), None) => {
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalRegistration,
            ));
        }
        (None, _) => {}
    }
    let binding_matches = match (placement.state(), grant) {
        (SessionRunnerPlacementState::Unpinned, None) => true,
        (
            SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned),
            Some(grant),
        ) => {
            pinned.credential_profile.as_ref() == Some(grant.profile())
                && placement.session() == grant.session()
                && pinned.runner == grant.runner()
        }
        (
            SessionRunnerPlacementState::Pinned(pinned)
            | SessionRunnerPlacementState::RunnerLost(pinned),
            None,
        ) => pinned.credential_profile.is_none(),
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
        row.try_get::<Option<String>, _>("credential_profile_name")?,
        row.try_get::<Option<Decimal>, _>("credential_grant_revision")?,
        row.try_get::<Option<String>, _>("credential_approval_kind")?,
    ) {
        (None, None, None) => None,
        (Some(profile), Some(revision), Some(approval)) => Some(CredentialDispatchAuthorization {
            session: lease.session(),
            runner: lease.runner(),
            grant_revision: decode_generation(revision)?,
            profile: profile_name(profile)?,
            tool: lease.tool().clone(),
            approval: decode_approval(approval)?,
        }),
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    if row.get::<Uuid, _>("attempt_id") != correlation.dispatch.attempt().into_uuid()
        || row.get::<Uuid, _>("session_id") != lease.session().into_uuid()
        || row.get::<Uuid, _>("runner_id") != correlation.runner.into_uuid()
        || row.get::<String, _>("tool_name") != correlation.tool.as_str()
        || decode_effect(row.get("effect_class"))? != lease.effect()
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
    tools: Vec<&'a ToolName>,
    runner_required_tools: BTreeSet<&'a ToolName>,
    workspace_repository: Option<&'a str>,
    workspace_directory: Option<&'a str>,
}

fn encode_placement_state(state: &SessionRunnerPlacementState) -> EncodedPlacementState<'_> {
    let (state_kind, pinned) = match state {
        SessionRunnerPlacementState::Unpinned => {
            return EncodedPlacementState {
                kind: "unpinned",
                pinned_runner: None,
                pinned_directory: None,
                pinned_profile: None,
                tools: Vec::new(),
                runner_required_tools: BTreeSet::new(),
                workspace_repository: None,
                workspace_directory: None,
            };
        }
        SessionRunnerPlacementState::Pinned(pinned) => ("pinned", pinned),
        SessionRunnerPlacementState::RunnerLost(pinned) => ("runner_lost", pinned),
    };
    EncodedPlacementState {
        kind: state_kind,
        pinned_runner: Some(pinned.runner.into_uuid()),
        pinned_directory: Some(pinned.working_directory.as_str()),
        pinned_profile: pinned
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
        tools: pinned.tools.iter().collect(),
        runner_required_tools: pinned.runner_required_tools.iter().collect(),
        workspace_repository: pinned
            .workspace
            .as_ref()
            .map(|workspace| workspace.repository.as_str()),
        workspace_directory: pinned
            .workspace
            .as_ref()
            .map(|workspace| workspace.working_directory.as_str()),
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
    let kind: String = row.get("selector_kind");
    match kind.as_str() {
        "identity" => row
            .try_get::<Option<Uuid>, _>("selector_runner_id")?
            .map(runner_id)
            .map(RunnerSelector::Identity)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        "capability_class" => row
            .try_get::<Option<String>, _>("selector_capability_class")?
            .map(capability_class)
            .transpose()?
            .map(RunnerSelector::CapabilityClass)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_directory(row: &PgRow) -> Result<WorkingDirectorySelection, RunnerProtocolStoreError> {
    let kind: String = row.get("directory_selection_kind");
    match kind.as_str() {
        "runner_default" => Ok(WorkingDirectorySelection::RunnerDefault),
        "exact" => row
            .try_get::<Option<String>, _>("requested_working_directory")?
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
    let kind: String = row.get("workspace_requirement_kind");
    match kind.as_str() {
        "none" => Ok(WorkspaceRequirement::None),
        "repository_worktree" => row
            .try_get::<Option<String>, _>("requested_repository_key")?
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
    let loci: String = row.get("loci_kind");
    let loci = match loci.as_str() {
        "runner_only" => ToolAdmissibleLoci::RunnerOnly { selector },
        "daemon_or_runner" => ToolAdmissibleLoci::DaemonOrRunner { selector },
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    Ok(RunnerToolDeclaration::new(
        tool_name(row.get("tool_name"))?,
        RunnerToolModelDefinition::try_new(
            row.get("model_description"),
            row.get("model_input_schema"),
        )
        .map_err(RunnerProtocolStoreError::Domain)?,
        decode_permission(row.get("permission_kind"))?,
        decode_effect(row.get("effect_class"))?,
        loci,
    ))
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
    }
}

fn decode_lease_state(value: String) -> Result<RunnerLeaseState, RunnerProtocolStoreError> {
    match value.as_str() {
        "offered" => Ok(RunnerLeaseState::Offered),
        "claimed" => Ok(RunnerLeaseState::Claimed),
        "completed" => Ok(RunnerLeaseState::Completed),
        "lost_unclaimed" => Ok(RunnerLeaseState::LostUnclaimed),
        "lost_claimed" => Ok(RunnerLeaseState::LostClaimed),
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
        .map(|row| capability_class(row.get("capability_class")))
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

fn require_count(row: &PgRow, column: &str, actual: usize) -> Result<(), RunnerProtocolStoreError> {
    if row.get::<Decimal, _>(column) == Decimal::from(actual) {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerProtocolCorruption {
    MissingCanonicalEnrollment,
    MissingCanonicalAudit,
    MissingCanonicalRegistration,
    MissingCanonicalPlacement,
    MissingCanonicalGrant,
    MissingCanonicalAttempt,
    IncompleteInventory,
    CrossWiredReference,
    InvalidEncoding,
    GenerationExhausted,
}

impl fmt::Display for RunnerProtocolCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingCanonicalEnrollment => "canonical runner enrollment is missing",
            Self::MissingCanonicalAudit => "canonical runner audit evidence is missing",
            Self::MissingCanonicalRegistration => "canonical runner registration is missing",
            Self::MissingCanonicalPlacement => "canonical runner placement is missing",
            Self::MissingCanonicalGrant => "canonical credential grant is missing",
            Self::MissingCanonicalAttempt => "canonical physical tool attempt is missing",
            Self::IncompleteInventory => "stored runner inventory is incomplete",
            Self::CrossWiredReference => "stored runner references are cross-wired",
            Self::InvalidEncoding => "stored runner encoding is invalid",
            Self::GenerationExhausted => "stored runner generation is exhausted",
        };
        formatter.write_str(message)
    }
}

impl Error for RunnerProtocolCorruption {}

#[derive(Debug)]
pub enum RunnerProtocolStoreError {
    Database(sqlx::Error),
    CommitAmbiguous(sqlx::Error),
    Corruption(RunnerProtocolCorruption),
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
