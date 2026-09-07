use super::*;

impl OauthCredentialRepository {
    /// Deletes retained authorization by profile identity and advances its generation.
    /// The callback discards process-local access tokens while the profile lock is held.
    /// Equal replay neither advances the generation nor invokes the callback.
    pub async fn delete(
        &self,
        command: &OauthCredentialCommand,
        unretained_failure: OauthCredentialFailure,
        discard_access: impl FnOnce() + Send,
    ) -> Result<OauthCredentialHandlingOutcome, OauthCredentialRepositoryError> {
        if command.operation != OauthCredentialOperation::Delete {
            return Err(OauthCredentialRepositoryError::Corruption);
        }
        let mut transaction = self.pool.begin().await?;
        if let Some(outcome) = claim(&mut transaction, command).await? {
            provisioning::commit(transaction).await?;
            return Ok(outcome);
        }
        let generation: Option<i64> = sqlx::query_scalar(
            "SELECT generation FROM oauth_credential_profile WHERE profile = $1 FOR UPDATE",
        )
        .bind(&command.profile)
        .fetch_optional(&mut *transaction)
        .await?;
        let outcome = if generation.is_some() {
            sqlx::query("UPDATE oauth_credential_profile SET generation = generation + 1 WHERE profile = $1")
                .bind(&command.profile).execute(&mut *transaction).await?;
            let removed =
                sqlx::query("DELETE FROM oauth_credential_authorization WHERE profile = $1")
                    .bind(&command.profile)
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected();
            discard_access();
            if removed == 0 {
                OauthCredentialOutcome::AlreadyDeleted
            } else {
                OauthCredentialOutcome::Deleted
            }
        } else {
            OauthCredentialOutcome::Failed(unretained_failure)
        };
        provisioning::finish(&mut transaction, command, &outcome).await?;
        provisioning::commit(transaction).await?;
        Ok(OauthCredentialHandlingOutcome::Recorded(outcome))
    }
}
