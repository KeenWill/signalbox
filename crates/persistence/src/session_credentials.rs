//! Append-only per-session model credential history.

use std::{collections::HashMap, sync::Arc};

use signalbox_application::ModelCallCredentialReference;
use signalbox_domain::{FastMode, ResolvedProviderTarget, SessionId};
use sqlx::{PgConnection, PgPool, Row, types::Uuid};

/// One model-family credential entry in a complete session snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionModelCredential {
    model_family: Arc<str>,
    credential_reference: Arc<str>,
}

impl SessionModelCredential {
    /// Names one model family and its non-secret credential reference.
    pub fn new(
        model_family: impl Into<Arc<str>>,
        credential_reference: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            model_family: model_family.into(),
            credential_reference: credential_reference.into(),
        }
    }

    /// Configuration-owned model family key.
    pub fn model_family(&self) -> &str {
        &self.model_family
    }

    /// Non-secret reference pinned for this family.
    pub fn credential_reference(&self) -> &str {
        &self.credential_reference
    }
}

/// A complete credential snapshot pinned as a session history event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCredentialPin {
    credentials: Arc<[SessionModelCredential]>,
}

impl SessionCredentialPin {
    /// Validates a nonempty snapshot with one entry per model family.
    pub fn try_new(
        mut credentials: Vec<SessionModelCredential>,
    ) -> Result<Self, SessionCredentialPinError> {
        credentials.sort_by(|left, right| left.model_family.cmp(&right.model_family));
        if credentials.is_empty() {
            return Err(SessionCredentialPinError::Empty);
        }
        if credentials.iter().any(|credential| {
            credential.model_family.is_empty() || credential.credential_reference.is_empty()
        }) {
            return Err(SessionCredentialPinError::EmptyValue);
        }
        if credentials
            .windows(2)
            .any(|pair| pair[0].model_family == pair[1].model_family)
        {
            return Err(SessionCredentialPinError::DuplicateModelFamily);
        }
        Ok(Self {
            credentials: credentials.into(),
        })
    }

    /// Iterates the complete snapshot in stable family order.
    pub fn credentials(&self) -> impl Iterator<Item = &SessionModelCredential> {
        self.credentials.iter()
    }
}

/// Why a configuration-owned credential snapshot is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCredentialPinError {
    /// A complete snapshot must contain at least one family.
    Empty,
    /// Family and credential spellings must be nonempty.
    EmptyValue,
    /// A family appeared more than once.
    DuplicateModelFamily,
}

/// Static model-target-to-family mapping used only to select a pinned entry.
#[derive(Clone, Debug)]
pub struct ModelCredentialFamilyCatalog {
    families: Arc<HashMap<ResolvedProviderTarget, ModelCredentialFamilyRoute>>,
    fast_targets: Arc<HashMap<ResolvedProviderTarget, ResolvedProviderTarget>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelCredentialFamilyRoute {
    family: Arc<str>,
    migration_fallback_family: Option<Arc<str>>,
}

impl ModelCredentialFamilyCatalog {
    /// Builds an exact target mapping and rejects conflicting definitions.
    pub fn try_new(
        entries: impl IntoIterator<Item = (ResolvedProviderTarget, Arc<str>, Option<Arc<str>>)>,
    ) -> Result<Self, ModelCredentialFamilyCatalogError> {
        let mut families = HashMap::new();
        for (target, family, migration_fallback_family) in entries {
            let route = ModelCredentialFamilyRoute {
                family,
                migration_fallback_family,
            };
            if let Some(previous) = families.insert(target, route.clone())
                && previous != route
            {
                return Err(ModelCredentialFamilyCatalogError::ConflictingTarget);
            }
        }
        Ok(Self {
            families: Arc::new(families),
            fast_targets: Arc::new(HashMap::new()),
        })
    }

    /// Adds each capability-authorized selected-to-serving fast-target route.
    pub fn with_fast_targets(
        mut self,
        entries: impl IntoIterator<Item = (ResolvedProviderTarget, ResolvedProviderTarget)>,
    ) -> Result<Self, ModelCredentialFamilyCatalogError> {
        let mut fast_targets = HashMap::new();
        for (selected, serving) in entries {
            if !self.families.contains_key(&selected) || !self.families.contains_key(&serving) {
                return Err(ModelCredentialFamilyCatalogError::ConflictingTarget);
            }
            if let Some(previous) = fast_targets.insert(selected, serving)
                && previous != serving
            {
                return Err(ModelCredentialFamilyCatalogError::ConflictingTarget);
            }
        }
        self.fast_targets = Arc::new(fast_targets);
        Ok(self)
    }

    /// Resolves the configuration family for one exact provider target.
    pub fn family(&self, target: ResolvedProviderTarget) -> Option<&str> {
        self.families
            .get(&target)
            .map(|route| route.family.as_ref())
    }

    /// Resolves the credential family for the effective serving target.
    pub fn family_for_call(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> Option<&str> {
        self.families
            .get(&self.serving_target(selected, fast_mode))
            .map(|route| route.family.as_ref())
    }

    pub(crate) fn migration_fallback_family_for_call(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> Option<&str> {
        self.families
            .get(&self.serving_target(selected, fast_mode))
            .and_then(|route| route.migration_fallback_family.as_deref())
    }

    /// Resolves the target that actually serves a call under this fast mode.
    ///
    /// Credential-pool selection needs the same target the credential family
    /// resolves from: a fast alternate target can carry its own family and its
    /// own pool, and the selectable base target names neither.
    pub fn serving_target_for_call(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> ResolvedProviderTarget {
        self.serving_target(selected, fast_mode)
    }

    fn serving_target(
        &self,
        selected: ResolvedProviderTarget,
        fast_mode: FastMode,
    ) -> ResolvedProviderTarget {
        match fast_mode {
            FastMode::Disabled => selected,
            FastMode::Enabled => self
                .fast_targets
                .get(&selected)
                .copied()
                .unwrap_or(selected),
        }
    }
}

/// Why a static target-family catalog cannot be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCredentialFamilyCatalogError {
    /// One exact target was assigned more than one family.
    ConflictingTarget,
}

pub(crate) async fn insert_initial_session_credential_event(
    connection: &mut PgConnection,
    session_id: Uuid,
    command_id: Uuid,
    provenance_kind: &'static str,
    pin: &SessionCredentialPin,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_model_credential_record
            (session_id, event_ordinal, event_kind, provenance_kind,
             provenance_command_id, recorded_at)
         VALUES ($1, 1, 'created', $2, $3, transaction_timestamp())",
    )
    .bind(session_id)
    .bind(provenance_kind)
    .bind(command_id)
    .execute(&mut *connection)
    .await?;
    for credential in pin.credentials() {
        sqlx::query(
            "INSERT INTO session_model_credential_entry
                (session_id, event_ordinal, model_family, credential_reference)
             VALUES ($1, 1, $2, $3)",
        )
        .bind(session_id)
        .bind(credential.model_family())
        .bind(credential.credential_reference())
        .execute(&mut *connection)
        .await?;
    }
    sqlx::query(
        "INSERT INTO session_current_model_credentials
            (session_id, current_event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(session_id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn load_current_session_credential(
    connection: &mut PgConnection,
    session_id: Uuid,
    family: &str,
) -> Result<ModelCallCredentialReference, sqlx::Error> {
    let reference: String = sqlx::query(
        "SELECT entry.credential_reference
           FROM session_current_model_credentials AS current
           JOIN session_model_credential_entry AS entry
             ON entry.session_id = current.session_id
            AND entry.event_ordinal = current.current_event_ordinal
          WHERE current.session_id = $1
            AND entry.model_family = $2",
    )
    .bind(session_id)
    .bind(family)
    .fetch_one(&mut *connection)
    .await?
    .try_get("credential_reference")?;
    Ok(ModelCallCredentialReference::new(reference))
}

pub(crate) async fn load_migrated_session_credential(
    connection: &mut PgConnection,
    session_id: Uuid,
    family: &str,
) -> Result<ModelCallCredentialReference, sqlx::Error> {
    let reference: String = sqlx::query(
        "SELECT entry.credential_reference
           FROM session_current_model_credentials AS current
           JOIN session_model_credential_record AS record
             ON record.session_id = current.session_id
            AND record.event_ordinal = current.current_event_ordinal
           JOIN session_model_credential_entry AS entry
             ON entry.session_id = current.session_id
            AND entry.event_ordinal = current.current_event_ordinal
          WHERE current.session_id = $1
            AND record.provenance_kind = 'migration_backfill'
            AND entry.model_family = $2",
    )
    .bind(session_id)
    .bind(family)
    .fetch_one(&mut *connection)
    .await?
    .try_get("credential_reference")?;
    Ok(ModelCallCredentialReference::new(reference))
}

/// Loads the current credential reference for one session and model family.
pub async fn current_session_credential(
    pool: &PgPool,
    session: SessionId,
    family: &str,
) -> Result<ModelCallCredentialReference, sqlx::Error> {
    current_session_credential_with_migration_fallback(pool, session, family, None).await
}

/// Loads an exact current credential, then a provenance-gated migration alias.
pub async fn current_session_credential_with_migration_fallback(
    pool: &PgPool,
    session: SessionId,
    family: &str,
    migration_fallback_family: Option<&str>,
) -> Result<ModelCallCredentialReference, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    match load_current_session_credential(&mut connection, session.into_uuid(), family).await {
        Ok(reference) => Ok(reference),
        Err(sqlx::Error::RowNotFound) => match migration_fallback_family {
            Some(fallback_family) => {
                load_migrated_session_credential(
                    &mut connection,
                    session.into_uuid(),
                    fallback_family,
                )
                .await
            }
            None => Err(sqlx::Error::RowNotFound),
        },
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use signalbox_domain::{FastMode, ProviderModelIdentity, ResolvedProviderTarget};
    use sqlx::types::Uuid;

    use super::{
        ModelCredentialFamilyCatalog, SessionCredentialPin, SessionCredentialPinError,
        SessionModelCredential,
    };

    #[test]
    fn credential_pin_rejects_duplicate_model_families() {
        let result = SessionCredentialPin::try_new(vec![
            SessionModelCredential::new("codex", "first"),
            SessionModelCredential::new("codex", "second"),
        ]);
        assert_eq!(result, Err(SessionCredentialPinError::DuplicateModelFamily));
    }

    #[test]
    fn target_family_catalog_retains_explicit_migration_fallback_metadata() {
        let configured_family = Arc::<str>::from("custom-anthropic");
        let migration_family = Arc::<str>::from("anthropic");
        let target =
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(1)));
        let catalog = ModelCredentialFamilyCatalog::try_new([(
            target,
            Arc::clone(&configured_family),
            Some(Arc::clone(&migration_family)),
        )])
        .expect("one exact family route is valid");

        assert_eq!(catalog.family(target), Some(configured_family.as_ref()));
        assert_eq!(
            catalog.migration_fallback_family_for_call(target, FastMode::Disabled),
            Some(migration_family.as_ref())
        );
    }

    #[test]
    fn fast_call_uses_the_serving_target_credential_family() {
        let selected_family = Arc::<str>::from("standard-family");
        let serving_family = Arc::<str>::from("fast-family");
        let selected =
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(1)));
        let serving =
            ResolvedProviderTarget::naming(ProviderModelIdentity::from_uuid(Uuid::from_u128(2)));
        let catalog = ModelCredentialFamilyCatalog::try_new([
            (selected, Arc::clone(&selected_family), None),
            (serving, Arc::clone(&serving_family), None),
        ])
        .expect("both credential families are valid")
        .with_fast_targets([(selected, serving)])
        .expect("the serving route names declared targets");

        assert_eq!(
            catalog.family_for_call(selected, FastMode::Disabled),
            Some(selected_family.as_ref())
        );
        assert_eq!(
            catalog.family_for_call(selected, FastMode::Enabled),
            Some(serving_family.as_ref())
        );
    }
}
