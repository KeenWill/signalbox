use super::*;

/// Delivery-origin evidence committed with an OAuth generation quarantine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OauthQuarantineCause {
    /// Current registration differs from the minting tuple.
    TupleMismatch,
    /// An exchange may have rotated the refresh token.
    RefreshAmbiguous,
    /// The authorization server permanently rejected refresh.
    RefreshRejected,
    /// Refreshed identity differs from the retained account.
    IdentityChanged,
    /// The adapter could not securely deliver the credential home.
    CredentialHome,
}

impl OauthQuarantineCause {
    fn spelling(self) -> &'static str {
        match self {
            Self::TupleMismatch => "tuple_mismatch",
            Self::RefreshAmbiguous => "refresh_ambiguous",
            Self::RefreshRejected => "refresh_rejected",
            Self::IdentityChanged => "identity_changed",
            Self::CredentialHome => "credential_home",
        }
    }

    fn parse(value: &str) -> Result<Self, OauthCredentialRepositoryError> {
        match value {
            "tuple_mismatch" => Ok(Self::TupleMismatch),
            "refresh_ambiguous" => Ok(Self::RefreshAmbiguous),
            "refresh_rejected" => Ok(Self::RefreshRejected),
            "identity_changed" => Ok(Self::IdentityChanged),
            "credential_home" => Ok(Self::CredentialHome),
            _ => Err(OauthCredentialRepositoryError::Corruption),
        }
    }
}

/// Authorization snapshot protected by a dispatch lease.
#[derive(Clone, Debug)]
pub struct OauthStoredAuthorization {
    /// Canonical minting configuration.
    pub registration: OauthRegistration,
    /// Refresh and identity tokens with redacted diagnostics.
    pub authorization: OauthAuthorization,
    /// Profile generation installed with authorization.
    pub generation: i64,
    /// An exchange must be resolved before presenting this refresh token again.
    pub refresh_in_progress: bool,
    /// Active delivery quarantine, if any.
    pub quarantine: Option<OauthQuarantineCause>,
}

/// Holds the profile row and shared catalog lock through adapter token copying.
pub struct OauthDispatchLease {
    transaction: sqlx::Transaction<'static, sqlx::Postgres>,
    profile: String,
    stored: Option<OauthStoredAuthorization>,
}

impl OauthDispatchLease {
    /// Reads authorization while retaining its row lock.
    pub fn authorization(&self) -> Option<&OauthStoredAuthorization> {
        self.stored.as_ref()
    }

    /// Commits the refresh marker before request bytes may be sent.
    pub async fn mark_refresh(mut self) -> Result<(), OauthCredentialRepositoryError> {
        sqlx::query("UPDATE oauth_credential_authorization SET refresh_in_progress = true WHERE profile = $1")
            .bind(&self.profile).execute(&mut *self.transaction).await?;
        self.commit().await
    }

    /// Clears a marker only after a definitive non-rotation.
    pub async fn clear_refresh(mut self) -> Result<(), OauthCredentialRepositoryError> {
        sqlx::query("UPDATE oauth_credential_authorization SET refresh_in_progress = false WHERE profile = $1")
            .bind(&self.profile).execute(&mut *self.transaction).await?;
        self.commit().await
    }

    /// Replaces refreshed material and clears its marker in one transaction.
    pub async fn replace_refresh(
        mut self,
        authorization: &OauthAuthorization,
    ) -> Result<(), OauthCredentialRepositoryError> {
        sqlx::query("UPDATE oauth_credential_authorization SET refresh_token = $2, identity_token = $3, refresh_in_progress = false WHERE profile = $1")
            .bind(&self.profile).bind(&authorization.refresh_token).bind(&authorization.identity_token)
            .execute(&mut *self.transaction).await?;
        self.commit().await
    }

    /// Stores failure evidence and makes the current generation unavailable atomically.
    pub async fn quarantine(
        mut self,
        cause: OauthQuarantineCause,
    ) -> Result<(), OauthCredentialRepositoryError> {
        let stored = self
            .stored
            .as_ref()
            .ok_or(OauthCredentialRepositoryError::Corruption)?;
        let cause = stored.quarantine.unwrap_or(cause);
        if cause == OauthQuarantineCause::CredentialHome {
            sqlx::query("INSERT INTO credential_exclusion (kind, profile, origin, oauth_generation) VALUES ('profile_quarantine', $1, 'codex_home', $2)")
                .bind(&self.profile).bind(stored.generation).execute(&mut *self.transaction).await?;
        }
        sqlx::query("INSERT INTO oauth_credential_failure (profile, generation, cause) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(&self.profile).bind(stored.generation).bind(cause.spelling()).execute(&mut *self.transaction).await?;
        sqlx::query("UPDATE oauth_credential_authorization SET quarantined = true, quarantine_cause = $2 WHERE profile = $1")
            .bind(&self.profile).bind(cause.spelling()).execute(&mut *self.transaction).await?;
        self.commit().await
    }

    /// Releases the lock after the adapter has copied the tokens into its owned home.
    pub async fn commit(self) -> Result<(), OauthCredentialRepositoryError> {
        provisioning::commit(self.transaction).await
    }
}

impl OauthCredentialRepository {
    /// Locks retained profile state without contacting an authorization server.
    pub async fn lock_dispatch(
        &self,
        profile: &str,
    ) -> Result<Option<OauthDispatchLease>, OauthCredentialRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(crate::lock_inventory::HASHED_TRANSACTION_ADVISORY_LOCK)
            .bind(format!("credential_pool_action_head:{profile}"))
            .execute(&mut *transaction)
            .await?;
        provisioning::read_catalog(&mut transaction).await?;
        let generation: Option<i64> = sqlx::query_scalar(
            "SELECT generation FROM oauth_credential_profile WHERE profile = $1 FOR UPDATE",
        )
        .bind(profile)
        .fetch_optional(&mut *transaction)
        .await?;
        if generation.is_none() {
            return Ok(None);
        }
        let row = sqlx::query("SELECT tuple::text, refresh_token, identity_token, account_identity::text, generation, refresh_in_progress, CASE WHEN quarantine_cause = 'credential_home' AND credential_home_quarantine_cleared(profile, generation) THEN NULL ELSE quarantine_cause END AS quarantine_cause FROM oauth_credential_authorization WHERE profile = $1")
            .bind(profile).fetch_optional(&mut *transaction).await?;
        let stored = row
            .map(|row| {
                let registration: String = row.try_get("tuple")?;
                let account: String = row.try_get("account_identity")?;
                let cause: Option<String> = row.try_get("quarantine_cause")?;
                Ok::<_, OauthCredentialRepositoryError>(OauthStoredAuthorization {
                    registration: serde_json::from_str(&registration)
                        .map_err(|_| OauthCredentialRepositoryError::Corruption)?,
                    authorization: OauthAuthorization {
                        refresh_token: row.try_get("refresh_token")?,
                        identity_token: row.try_get("identity_token")?,
                        account_identity: serde_json::from_str(&account)
                            .map_err(|_| OauthCredentialRepositoryError::Corruption)?,
                    },
                    generation: row.try_get("generation")?,
                    refresh_in_progress: row.try_get("refresh_in_progress")?,
                    quarantine: cause
                        .as_deref()
                        .map(OauthQuarantineCause::parse)
                        .transpose()?,
                })
            })
            .transpose()?;
        Ok(Some(OauthDispatchLease {
            transaction,
            profile: profile.to_owned(),
            stored,
        }))
    }
}

pub(crate) async fn quarantined_profiles(
    connection: &mut PgConnection,
    policy: &crate::model_execution::CredentialPoolRuntimePolicy,
) -> Result<Vec<String>, sqlx::Error> {
    let members = policy
        .members()
        .iter()
        .map(|member| member.credential_reference())
        .collect::<Vec<_>>();
    provisioning::read_catalog(connection).await?;
    let profiles: Vec<String> = sqlx::query_scalar(
        "SELECT p.profile FROM oauth_credential_profile p
         JOIN oauth_credential_registration r USING (profile)
         WHERE p.profile = ANY($1) ORDER BY p.profile COLLATE \"C\"
         FOR UPDATE OF p",
    )
    .bind(members)
    .fetch_all(&mut *connection)
    .await?;
    if profiles.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar("SELECT profile FROM oauth_credential_authorization WHERE quarantined AND profile = ANY($1) AND (quarantine_cause <> 'credential_home' OR NOT credential_home_quarantine_cleared(profile, generation))")
        .bind(profiles)
        .fetch_all(connection)
        .await
}

#[cfg(all(test, feature = "postgres-integration"))]
mod tests {
    use super::*;
    use crate::model_execution::{
        CredentialPoolRuntimeAction, CredentialPoolRuntimeExhaustion, CredentialPoolRuntimeMember,
        CredentialPoolRuntimePolicy,
    };
    use std::num::NonZeroU32;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ImageExt, runners::AsyncRunner},
    };

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn quarantine_reads_lock_only_registered_oauth_pool_members()
    -> Result<(), Box<dyn std::error::Error>> {
        let container = Postgres::default()
            // Same PostgreSQL image as tests/postgres_integration/main.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(crate::disposable_postgres_server_args())
            .with_mount(crate::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(crate::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?,
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(crate::local_test_connection_options(&url)?)
            .await?;
        crate::migrate(&pool).await?;
        let repository = OauthCredentialRepository::new(pool.clone());
        let registration = OauthRegistration {
            client_id: "fixture-client".into(),
            token_url: "https://authorization.example/token".into(),
            refresh_token_url: "https://authorization.example/oauth/token".into(),
            device_authorization_url: "https://authorization.example/device".into(),
            scopes: vec!["openid".into()],
        };
        repository
            .replace_registrations(&[("oauth".into(), registration.clone())])
            .await?;
        let command = OauthCredentialCommand {
            command_id: signalbox_domain::DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            operation: OauthCredentialOperation::Provision,
            profile: "oauth".into(),
        };
        let OauthStartOutcome::Started(exchange) = repository
            .begin_exchange(&command, Ok(&registration))
            .await?
        else {
            panic!("initial exchange");
        };
        repository
            .complete_exchange(
                &exchange,
                Ok(&OauthAuthorization {
                    refresh_token: "fixture-refresh".into(),
                    identity_token: "fixture-identity".into(),
                    account_identity: serde_json::json!({"subject":"fixture-account"}),
                }),
            )
            .await?;
        repository
            .lock_dispatch("oauth")
            .await?
            .expect("registered profile")
            .quarantine(OauthQuarantineCause::RefreshRejected)
            .await?;
        // A retained pool lock row does not establish current OAuth membership.
        sqlx::query("INSERT INTO oauth_credential_profile (profile) VALUES ('ambient')")
            .execute(&pool)
            .await?;
        let mut blocked_ambient = pool.begin().await?;
        sqlx::query(
            "SELECT profile FROM oauth_credential_profile WHERE profile = 'ambient' FOR UPDATE",
        )
        .execute(&mut *blocked_ambient)
        .await?;
        for (members, expected) in [
            (vec!["ambient", "api-key"], vec![]),
            (vec!["ambient", "api-key", "oauth"], vec!["oauth"]),
        ] {
            let policy = CredentialPoolRuntimePolicy::new(
                "fixture-pool",
                members
                    .into_iter()
                    .map(|profile| CredentialPoolRuntimeMember::new(profile, NonZeroU32::MIN))
                    .collect::<Vec<_>>(),
                CredentialPoolRuntimeExhaustion::Fail,
                CredentialPoolRuntimeAction::Stay,
                CredentialPoolRuntimeAction::Stay,
                CredentialPoolRuntimeAction::Stay,
                CredentialPoolRuntimeAction::Stay,
            );
            let mut selection = pool.begin().await?;
            let quarantined = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                quarantined_profiles(&mut selection, &policy),
            )
            .await??;
            assert_eq!(quarantined, expected);
            selection.commit().await?;
        }
        let retained: Vec<String> = sqlx::query_scalar(
            "SELECT profile FROM oauth_credential_profile ORDER BY profile COLLATE \"C\"",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(retained, ["ambient", "oauth"]);
        blocked_ambient.rollback().await?;
        Ok(())
    }
}
