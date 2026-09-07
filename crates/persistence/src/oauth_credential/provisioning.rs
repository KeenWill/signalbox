use super::*;
use serde::{Deserialize, Serialize};

/// Canonical configuration tuple used by the authorization server.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OauthRegistration {
    /// Exact configured client identifier.
    pub client_id: String,
    /// Canonical HTTPS token endpoint.
    pub token_url: String,
    /// Canonical HTTPS device endpoint.
    pub device_authorization_url: String,
    /// Exact scopes in declared order.
    pub scopes: Vec<String>,
}

/// Operator instructions retained before their first emission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OauthProgress {
    /// Human authorization code, never the secret device code.
    pub user_code: String,
    /// Validated HTTPS verification URI.
    pub verification_uri: String,
}

/// Authorization material accepted by the configured token endpoint.
#[derive(Clone)]
pub struct OauthAuthorization {
    /// Token presented only by the daemon to the token endpoint.
    pub refresh_token: String,
    /// Original JWT provided by the token endpoint.
    pub identity_token: String,
    /// Harvested subject and account metadata used for account independence.
    pub account_identity: serde_json::Value,
}

impl std::fmt::Debug for OauthAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OauthAuthorization([redacted])")
    }
}

/// A durable claim that owns exactly one device exchange.
#[derive(Clone, Debug)]
pub struct OauthExchange {
    command: OauthCredentialCommand,
    registration: OauthRegistration,
    generation: i64,
}

/// Admission either owns the exchange or returns an existing disposition.
#[derive(Clone, Debug)]
pub enum OauthStartOutcome {
    /// The caller owns the newly committed exchange claim.
    Started(OauthExchange),
    /// Equal replay, conflict, or a terminal no-exchange result.
    Existing(OauthCredentialHandlingOutcome),
}

fn json(value: &impl Serialize) -> Result<String, OauthCredentialRepositoryError> {
    serde_json::to_string(value).map_err(|_| OauthCredentialRepositoryError::Corruption)
}

async fn lock_catalog(connection: &mut PgConnection) -> Result<(), OauthCredentialRepositoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('oauth-registration-catalog', 0))")
        .execute(connection)
        .await?;
    Ok(())
}

async fn read_catalog(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('oauth-registration-catalog', 0))",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn lock_profiles(
    connection: &mut PgConnection,
    profiles: &[String],
) -> Result<(), sqlx::Error> {
    for profile in profiles {
        sqlx::query(
            "INSERT INTO oauth_credential_profile (profile) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(profile)
        .execute(&mut *connection)
        .await?;
        sqlx::query("SELECT profile FROM oauth_credential_profile WHERE profile = $1 FOR UPDATE")
            .bind(profile)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

pub(crate) async fn lock_pool_members(
    connection: &mut PgConnection,
    policy: &crate::model_execution::CredentialPoolRuntimePolicy,
) -> Result<(), sqlx::Error> {
    let mut profiles = policy
        .members()
        .iter()
        .map(|member| member.credential_reference().to_owned())
        .collect::<Vec<_>>();
    profiles.sort();
    profiles.dedup();
    lock_profiles(connection, &profiles).await
}

async fn co_members(
    connection: &mut PgConnection,
    profile: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let mut profiles: Vec<String> = sqlx::query_scalar("SELECT $1::text UNION SELECT peer.credential_reference FROM model_call_credential_pool_member own JOIN model_call_credential_pool_member peer USING (model_call_id) WHERE own.credential_reference = $1")
        .bind(profile).fetch_all(connection).await?;
    profiles.sort();
    Ok(profiles)
}

async fn finish(
    connection: &mut PgConnection,
    command: &OauthCredentialCommand,
    outcome: &OauthCredentialOutcome,
) -> Result<(), OauthCredentialRepositoryError> {
    let (_, result_table) = command.operation.tables();
    let (outcome, reason) = outcome.columns();
    // The closed operation supplies the identifier; all values are bound.
    sqlx::query(sqlx::AssertSqlSafe(
        format!("INSERT INTO {result_table} (command_id, outcome, reason) VALUES ($1, $2, $3)")
            .as_str(),
    ))
    .bind(command.command_id.into_uuid())
    .bind(outcome)
    .bind(reason)
    .execute(connection)
    .await?;
    Ok(())
}

async fn commit(
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), OauthCredentialRepositoryError> {
    tx.commit().await.map_err(|error| {
        if crate::commit_failure_is_ambiguous(&error) {
            OauthCredentialRepositoryError::CommitAmbiguous
        } else {
            OauthCredentialRepositoryError::Database
        }
    })
}

impl OauthCredentialRepository {
    /// Replaces current OAuth registrations, serialized with authorization commits.
    pub async fn replace_registrations(
        &self,
        registrations: &[(String, OauthRegistration)],
    ) -> Result<(), OauthCredentialRepositoryError> {
        let mut tx = self.pool.begin().await?;
        lock_catalog(&mut tx).await?;
        sqlx::query("DELETE FROM oauth_credential_registration")
            .execute(&mut *tx)
            .await?;
        for (profile, registration) in registrations {
            sqlx::query(
                "INSERT INTO oauth_credential_profile (profile) VALUES ($1) ON CONFLICT DO NOTHING",
            )
            .bind(profile)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO oauth_credential_registration (profile, tuple) VALUES ($1, $2::jsonb)",
            )
            .bind(profile)
            .bind(json(registration)?)
            .execute(&mut *tx)
            .await?;
        }
        commit(tx).await
    }

    /// Commits a pending claim before allowing a device request to be sent.
    pub async fn begin_exchange(
        &self,
        command: &OauthCredentialCommand,
        registration: Result<&OauthRegistration, OauthCredentialFailure>,
    ) -> Result<OauthStartOutcome, OauthCredentialRepositoryError> {
        let mut tx = self.pool.begin().await?;
        if let Some(existing) = claim(&mut tx, command).await? {
            return Ok(OauthStartOutcome::Existing(existing));
        }
        let registration = match registration {
            Ok(registration) => registration,
            Err(reason) => {
                let outcome = OauthCredentialOutcome::Failed(reason);
                finish(&mut tx, command, &outcome).await?;
                commit(tx).await?;
                return Ok(OauthStartOutcome::Existing(
                    OauthCredentialHandlingOutcome::Recorded(outcome),
                ));
            }
        };
        read_catalog(&mut tx).await?;
        sqlx::query(
            "INSERT INTO oauth_credential_profile (profile) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(&command.profile)
        .execute(&mut *tx)
        .await?;
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM oauth_credential_profile WHERE profile = $1 FOR UPDATE",
        )
        .bind(&command.profile)
        .fetch_one(&mut *tx)
        .await?;
        let authorized: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM oauth_credential_authorization WHERE profile = $1)",
        )
        .bind(&command.profile)
        .fetch_one(&mut *tx)
        .await?;
        let outcome = match (command.operation, authorized) {
            (OauthCredentialOperation::Provision, true) => {
                Some(OauthCredentialOutcome::AlreadyProvisioned)
            }
            (OauthCredentialOperation::Reprovision, false) => {
                Some(OauthCredentialOutcome::NotProvisioned)
            }
            (OauthCredentialOperation::Delete, _) => {
                return Err(OauthCredentialRepositoryError::Corruption);
            }
            _ => None,
        };
        if let Some(outcome) = outcome {
            finish(&mut tx, command, &outcome).await?;
            commit(tx).await?;
            return Ok(OauthStartOutcome::Existing(
                OauthCredentialHandlingOutcome::Recorded(outcome),
            ));
        }
        sqlx::query("INSERT INTO oauth_credential_exchange (command_id, profile, tuple, starting_generation) VALUES ($1, $2, $3::jsonb, $4)")
            .bind(command.command_id.into_uuid()).bind(&command.profile).bind(json(registration)?).bind(generation)
            .execute(&mut *tx).await?;
        commit(tx).await?;
        Ok(OauthStartOutcome::Started(OauthExchange {
            command: command.clone(),
            registration: registration.clone(),
            generation,
        }))
    }

    /// Retains validated operator instructions before emitting them.
    pub async fn retain_progress(
        &self,
        exchange: &OauthExchange,
        progress: &OauthProgress,
    ) -> Result<(), OauthCredentialRepositoryError> {
        sqlx::query("INSERT INTO oauth_credential_authorization_progress (command_id, user_code, verification_uri) VALUES ($1, $2, $3)")
            .bind(exchange.command.command_id.into_uuid()).bind(&progress.user_code).bind(&progress.verification_uri)
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Reads retained operator instructions for an equal pending replay.
    pub async fn progress(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<OauthProgress>, OauthCredentialRepositoryError> {
        sqlx::query("SELECT user_code, verification_uri FROM oauth_credential_authorization_progress WHERE command_id = $1")
            .bind(command_id.into_uuid()).fetch_optional(&self.pool).await?
            .map(|row| Ok(OauthProgress { user_code: row.try_get("user_code")?, verification_uri: row.try_get("verification_uri")? })).transpose()
    }

    /// Atomically retains authorization and the receipt after revalidating generation and registration.
    pub async fn complete_exchange(
        &self,
        exchange: &OauthExchange,
        authorization: Result<&OauthAuthorization, OauthCredentialFailure>,
    ) -> Result<OauthCredentialOutcome, OauthCredentialRepositoryError> {
        loop {
            let mut tx = self.pool.begin().await?;
            read_catalog(&mut tx).await?;
            sqlx::query("SELECT command_id FROM durable_command WHERE command_id = $1 FOR UPDATE")
                .bind(exchange.command.command_id.into_uuid())
                .execute(&mut *tx)
                .await?;
            if let OauthCredentialHandlingOutcome::Recorded(outcome) = existing(
                &mut tx,
                &exchange.command,
                exchange.command.operation.kind(),
            )
            .await?
            {
                return Ok(outcome);
            }
            let profiles = co_members(&mut tx, &exchange.command.profile).await?;
            lock_profiles(&mut tx, &profiles).await?;
            let current_profiles = co_members(&mut tx, &exchange.command.profile).await?;
            if current_profiles
                .iter()
                .any(|profile| profiles.binary_search(profile).is_err())
            {
                tx.rollback().await?;
                continue;
            }
            let generation: i64 = sqlx::query_scalar(
                "SELECT generation FROM oauth_credential_profile WHERE profile = $1 FOR UPDATE",
            )
            .bind(&exchange.command.profile)
            .fetch_one(&mut *tx)
            .await?;
            let current: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM oauth_credential_registration WHERE profile = $1 AND tuple = $2::jsonb)")
            .bind(&exchange.command.profile).bind(json(&exchange.registration)?).fetch_one(&mut *tx).await?;
            let outcome = if generation != exchange.generation {
                OauthCredentialOutcome::Superseded
            } else if !current {
                OauthCredentialOutcome::Failed(OauthCredentialFailure::RegistrationChanged)
            } else {
                match authorization {
                    Err(reason) => OauthCredentialOutcome::Failed(reason),
                    Ok(authorization) if authorization.identity_token.is_empty() => {
                        OauthCredentialOutcome::Failed(
                            OauthCredentialFailure::TokenResponseWithoutIdentity,
                        )
                    }
                    Ok(authorization) => {
                        let collision: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM model_call_credential_pool_member own
                         JOIN model_call_credential_pool_member peer USING (model_call_id)
                         JOIN oauth_credential_authorization auth ON auth.profile = peer.credential_reference
                         WHERE own.credential_reference = $1 AND peer.credential_reference <> $1 AND auth.account_identity = $2::jsonb)"
                    ).bind(&exchange.command.profile).bind(json(&authorization.account_identity)?).fetch_one(&mut *tx).await?;
                        if collision {
                            OauthCredentialOutcome::Failed(
                                OauthCredentialFailure::AccountIndependenceFailed,
                            )
                        } else {
                            sqlx::query("UPDATE oauth_credential_profile SET generation = generation + 1 WHERE profile = $1")
                            .bind(&exchange.command.profile).execute(&mut *tx).await?;
                            sqlx::query("INSERT INTO oauth_credential_authorization (profile, tuple, refresh_token, identity_token, account_identity, generation)
                            VALUES ($1, $2::jsonb, $3, $4, $5::jsonb, $6)
                            ON CONFLICT (profile) DO UPDATE SET tuple = EXCLUDED.tuple, refresh_token = EXCLUDED.refresh_token,
                            identity_token = EXCLUDED.identity_token, account_identity = EXCLUDED.account_identity,
                            generation = EXCLUDED.generation, refresh_in_progress = false, quarantined = false")
                            .bind(&exchange.command.profile).bind(json(&exchange.registration)?).bind(&authorization.refresh_token)
                            .bind(&authorization.identity_token).bind(json(&authorization.account_identity)?).bind(generation + 1)
                            .execute(&mut *tx).await?;
                            match exchange.command.operation {
                                OauthCredentialOperation::Provision => {
                                    OauthCredentialOutcome::Provisioned
                                }
                                OauthCredentialOperation::Reprovision => {
                                    OauthCredentialOutcome::Reprovisioned
                                }
                                OauthCredentialOperation::Delete => {
                                    return Err(OauthCredentialRepositoryError::Corruption);
                                }
                            }
                        }
                    }
                }
            };
            finish(&mut tx, &exchange.command, &outcome).await?;
            commit(tx).await?;
            return Ok(outcome);
        }
    }

    /// Terminalizes pending exchange claims before the daemon accepts work.
    pub async fn abandon_pending(&self) -> Result<(), OauthCredentialRepositoryError> {
        let mut tx = self.pool.begin().await?;
        for operation in [
            OauthCredentialOperation::Provision,
            OauthCredentialOperation::Reprovision,
        ] {
            let (request, result) = operation.tables();
            // Both identifiers come from the closed operation.
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO {result} (command_id, outcome) SELECT request.command_id, 'abandoned' FROM {request} request LEFT JOIN {result} result USING (command_id) WHERE result.command_id IS NULL").as_str()))
                .execute(&mut *tx).await?;
        }
        commit(tx).await
    }
}
