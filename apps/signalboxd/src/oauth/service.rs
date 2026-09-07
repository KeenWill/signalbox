use super::{
    OauthClient,
    refresh::{RefreshFailure, Refreshed},
};
use signalbox_model_runtime::{
    CancellationSignal, CredentialAccessFailure as Failure, CredentialReference, CredentialValue,
};
use signalbox_model_runtime_codex_cli::{
    OauthCredentialInstaller, OauthCredentialMaterial, OauthCredentialProvider,
    OauthDeliveryFuture, OauthDeliveryOutcome,
};
use signalbox_persistence::oauth_credential::{
    OauthCredentialRepository, OauthDispatchLease, OauthQuarantineCause as Cause,
    OauthRegistration, OauthStoredAuthorization,
};
use std::collections::HashMap;
use tokio::{sync::Mutex, time::Instant};

struct CachedAccess {
    generation: i64,
    token: CredentialValue,
    expires_at: Option<Instant>,
}

#[derive(Default)]
struct RefreshState {
    access: Option<CachedAccess>,
    failure: Option<Failure>,
}

struct Profile {
    registration: OauthRegistration,
    access: Mutex<RefreshState>,
}

/// Process-shared authority for OAuth refresh and invocation delivery.
pub struct OauthCredentialService {
    repository: OauthCredentialRepository,
    client: OauthClient,
    profiles: HashMap<String, Profile>,
}

impl std::fmt::Debug for OauthCredentialService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OauthCredentialService")
    }
}

impl OauthCredentialService {
    /// Creates one shared refresh cache from the validated registration catalog.
    pub fn new(
        pool: sqlx::PgPool,
        registrations: Vec<(String, OauthRegistration)>,
    ) -> Result<Self, Failure> {
        Ok(Self {
            repository: OauthCredentialRepository::new(pool),
            client: OauthClient::new().map_err(|_| Failure::Unavailable)?,
            profiles: registrations
                .into_iter()
                .map(|(name, registration)| {
                    (
                        name,
                        Profile {
                            registration,
                            access: Mutex::new(RefreshState::default()),
                        },
                    )
                })
                .collect(),
        })
    }

    async fn lease(&self, reference: &str) -> Result<OauthDispatchLease, Failure> {
        self.repository
            .lock_dispatch(reference)
            .await
            .map_err(|_| Failure::Unavailable)?
            .ok_or(Failure::Unavailable)
    }

    async fn prepare(
        &self,
        reference: &str,
        installer: &mut dyn OauthCredentialInstaller,
        mut cancellation: CancellationSignal,
    ) -> Result<OauthDeliveryOutcome, Failure> {
        let profile = self.profiles.get(reference).ok_or(Failure::Unmapped)?;
        let (mut state, joined) = match profile.access.try_lock() {
            Ok(state) => (state, false),
            Err(_) => {
                let Some(state) = cancellation
                    .run_until_cancelled(profile.access.lock())
                    .await
                else {
                    return Ok(OauthDeliveryOutcome::Cancelled);
                };
                (state, true)
            }
        };
        if joined && let Some(failure) = state.failure {
            return Err(failure);
        }
        let result = self
            .prepare_locked(
                reference,
                profile,
                &mut state.access,
                joined,
                installer,
                cancellation,
            )
            .await;
        state.failure = result.as_ref().err().copied();
        result.map(|()| OauthDeliveryOutcome::Delivered)
    }

    async fn prepare_locked(
        &self,
        reference: &str,
        profile: &Profile,
        cached: &mut Option<CachedAccess>,
        joined: bool,
        installer: &mut dyn OauthCredentialInstaller,
        mut cancellation: CancellationSignal,
    ) -> Result<(), Failure> {
        let lease = self.lease(reference).await?;
        let stored = lease.authorization().cloned().ok_or(Failure::Unavailable)?;
        if let Some(cause) = stored.quarantine {
            return Err(failure(cause));
        }
        if stored.registration != profile.registration {
            *cached = None;
            return quarantine(lease, Cause::TupleMismatch).await;
        }
        if stored.refresh_in_progress {
            *cached = None;
            return quarantine(lease, Cause::RefreshAmbiguous).await;
        }
        if let Some(access) = cached.as_ref().filter(|access| {
            access.generation == stored.generation
                && access
                    .expires_at
                    .map_or(joined, |until| until > Instant::now())
        }) {
            return install(lease, &stored, access.token.clone(), installer).await;
        }
        *cached = None;
        if lease.mark_refresh().await.is_err() {
            // No request was formed; even an ambiguous marker commit cannot have rotated a token.
            if let Ok(lease) = self.lease(reference).await
                && lease
                    .authorization()
                    .is_some_and(|current| current.generation == stored.generation)
            {
                let _ = lease.clear_refresh().await;
            }
            return Err(Failure::Unavailable);
        }
        let refreshed = self
            .client
            .refresh(
                &profile.registration,
                &stored.authorization,
                &mut cancellation,
            )
            .await;
        let lease = self.lease(reference).await?;
        let current = lease.authorization().cloned().ok_or(Failure::Unavailable)?;
        if current.generation != stored.generation {
            return Err(Failure::Unavailable);
        }
        if let Some(cause) = current.quarantine {
            return Err(failure(cause));
        }
        let Refreshed {
            authorization,
            access_token,
            expires_at,
        } = match refreshed {
            Ok(value) => value,
            Err(RefreshFailure::NonRotating) => {
                lease
                    .clear_refresh()
                    .await
                    .map_err(|_| Failure::Unavailable)?;
                return Err(Failure::Unavailable);
            }
            Err(RefreshFailure::Ambiguous) => {
                return quarantine(lease, Cause::RefreshAmbiguous).await;
            }
            Err(RefreshFailure::Rejected) => {
                return quarantine(lease, Cause::RefreshRejected).await;
            }
            Err(RefreshFailure::IdentityChanged) => {
                return quarantine(lease, Cause::IdentityChanged).await;
            }
        };
        let committed = lease.replace_refresh(&authorization).await.is_ok();
        let lease = self.lease(reference).await?;
        let current = lease.authorization().cloned().ok_or(Failure::Unavailable)?;
        if current.generation != stored.generation {
            return Err(Failure::Unavailable);
        }
        if let Some(cause) = current.quarantine {
            return Err(failure(cause));
        }
        if current.refresh_in_progress {
            return quarantine(lease, Cause::RefreshAmbiguous).await;
        }
        if !committed
            && (current.authorization.refresh_token != authorization.refresh_token
                || current.authorization.identity_token != authorization.identity_token)
        {
            return quarantine(lease, Cause::RefreshAmbiguous).await;
        }
        install(lease, &current, access_token.clone(), installer).await?;
        *cached = Some(CachedAccess {
            generation: current.generation,
            token: access_token,
            expires_at,
        });
        Ok(())
    }
}

async fn install(
    lease: OauthDispatchLease,
    stored: &OauthStoredAuthorization,
    access_token: CredentialValue,
    installer: &mut dyn OauthCredentialInstaller,
) -> Result<(), Failure> {
    let material = OauthCredentialMaterial {
        access_token,
        identity_token: CredentialValue::new(
            stored.authorization.identity_token.as_bytes().to_vec(),
        ),
        account_id: stored.authorization.account_identity["account_id"]
            .as_str()
            .map(str::to_owned),
    };
    if installer.install(material).is_err() {
        return quarantine(lease, Cause::CredentialHome).await;
    }
    lease.commit().await.map_err(|_| Failure::Unavailable)
}

async fn quarantine(lease: OauthDispatchLease, cause: Cause) -> Result<(), Failure> {
    lease
        .quarantine(cause)
        .await
        .map_err(|_| Failure::Unavailable)?;
    Err(failure(cause))
}

fn failure(cause: Cause) -> Failure {
    match cause {
        Cause::TupleMismatch => Failure::OauthTupleMismatch,
        Cause::RefreshAmbiguous => Failure::OauthRefreshAmbiguous,
        Cause::RefreshRejected => Failure::OauthRefreshRejected,
        Cause::IdentityChanged => Failure::OauthIdentityChanged,
        Cause::CredentialHome => Failure::OauthCredentialHome,
    }
}

impl OauthCredentialProvider for OauthCredentialService {
    fn deliver<'a>(
        &'a self,
        reference: &'a CredentialReference,
        installer: &'a mut dyn OauthCredentialInstaller,
        cancellation: CancellationSignal,
    ) -> OauthDeliveryFuture<'a> {
        Box::pin(self.prepare(reference.as_str(), installer, cancellation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::DurableCommandId;
    use signalbox_persistence::oauth_credential::*;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ImageExt, runners::AsyncRunner},
    };

    #[derive(Default)]
    struct Installer(Option<OauthCredentialMaterial>);
    impl OauthCredentialInstaller for Installer {
        fn install(&mut self, material: OauthCredentialMaterial) -> Result<(), Failure> {
            self.0 = Some(material);
            Ok(())
        }
    }

    struct RejectInstall;
    impl OauthCredentialInstaller for RejectInstall {
        fn install(&mut self, _: OauthCredentialMaterial) -> Result<(), Failure> {
            Err(Failure::Unavailable)
        }
    }

    #[tokio::test]
    async fn cancelling_a_profile_wait_preserves_the_refresh_state() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgresql://signalbox:fixture@127.0.0.1/signalbox")
            .expect("lazy pool");
        let registration = OauthRegistration {
            client_id: "fixture-client".into(),
            token_url: "https://authorization.example/token".into(),
            device_authorization_url: "https://authorization.example/device".into(),
            scopes: vec!["openid".into()],
        };
        let service = OauthCredentialService::new(pool, vec![("profile".into(), registration)])
            .expect("service");
        let mut state = service.profiles["profile"].access.lock().await;
        state.failure = Some(Failure::OauthRefreshRejected);
        let mut installer = Installer::default();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let waiting = service.prepare(
            "profile",
            &mut installer,
            CancellationSignal::when(async {
                let _ = cancelled.await;
            }),
        );
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|context| {
                std::task::Poll::Ready(
                    std::future::Future::poll(waiting.as_mut(), context).is_pending(),
                )
            })
            .await
        );
        cancel.send(()).expect("waiting preparation");
        assert_eq!(waiting.await, Ok(OauthDeliveryOutcome::Cancelled));
        assert_eq!(state.failure, Some(Failure::OauthRefreshRejected));
        assert!(state.access.is_none());
        drop(state);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL and local HTTPS"]
    async fn oauth_service_singleflight_reuses_access_and_restart_marker_quarantines()
    -> Result<(), Box<dyn std::error::Error>> {
        let container = Postgres::default()
            // Same PostgreSQL image as tests/process_protocol_runtime/fixtures.rs.
            .with_tag("18.4-alpine3.23")
            .with_cmd(signalbox_persistence::disposable_postgres_server_args())
            .with_mount(signalbox_persistence::disposable_postgres_state_tmpfs_from_example()?)
            .with_labels(signalbox_persistence::disposable_test_container_labels())
            .start()
            .await?;
        let url = format!(
            "postgres://postgres:postgres@{}:{}/postgres",
            container.get_host().await?,
            container.get_host_port_ipv4(5432).await?
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(signalbox_persistence::local_test_connection_options(&url)?)
            .await?;
        signalbox_persistence::migrate(&pool).await?;
        let (client, registration, server) = super::super::tests::https_server(vec![(
            200,
            serde_json::json!({"access_token":"shared-access", "refresh_token":"rotated-refresh", "expires_in":3600}),
        )])?;
        let repository = OauthCredentialRepository::new(pool.clone());
        repository
            .replace_registrations(&[("profile".into(), registration.clone())])
            .await?;
        let command = OauthCredentialCommand {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            operation: OauthCredentialOperation::Provision,
            profile: "profile".into(),
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
                    refresh_token: "initial-refresh".into(),
                    identity_token: "retained-identity".into(),
                    account_identity: serde_json::json!({"subject":"subject"}),
                }),
            )
            .await?;
        let mut service = OauthCredentialService::new(
            pool.clone(),
            vec![("profile".into(), registration.clone())],
        )
        .map_err(|_| "service construction")?;
        service.client = client;
        let mut first = Installer::default();
        let mut second = Installer::default();
        let (a, b) = tokio::join!(
            service.prepare("profile", &mut first, CancellationSignal::never()),
            service.prepare("profile", &mut second, CancellationSignal::never())
        );
        a.map_err(|_| "first delivery")?;
        b.map_err(|_| "joined delivery")?;
        assert_eq!(
            first.0.expect("material").access_token.expose_bytes(),
            b"shared-access"
        );
        assert_eq!(
            second.0.expect("material").access_token.expose_bytes(),
            b"shared-access"
        );
        assert_eq!(server.join().map_err(|_| "TLS server panicked")??.len(), 1);
        assert_eq!(
            service
                .prepare("profile", &mut RejectInstall, CancellationSignal::never())
                .await,
            Err(Failure::OauthCredentialHome)
        );
        let cause: String = sqlx::query_scalar(
            "SELECT cause FROM oauth_credential_failure WHERE profile = 'profile'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(cause, "credential_home");
        let command = OauthCredentialCommand {
            command_id: DurableCommandId::from_uuid(uuid::Uuid::now_v7()),
            operation: OauthCredentialOperation::Reprovision,
            profile: "profile".into(),
        };
        let OauthStartOutcome::Started(exchange) = repository
            .begin_exchange(&command, Ok(&registration))
            .await?
        else {
            panic!("reprovision exchange");
        };
        repository
            .complete_exchange(
                &exchange,
                Ok(&OauthAuthorization {
                    refresh_token: "reprovisioned-refresh".into(),
                    identity_token: "retained-identity".into(),
                    account_identity: serde_json::json!({"subject":"subject"}),
                }),
            )
            .await?;
        repository
            .lock_dispatch("profile")
            .await?
            .expect("profile")
            .mark_refresh()
            .await?;
        let restarted =
            OauthCredentialService::new(pool.clone(), vec![("profile".into(), registration)])
                .map_err(|_| "service construction")?;
        assert_eq!(
            restarted
                .prepare(
                    "profile",
                    &mut Installer::default(),
                    CancellationSignal::never()
                )
                .await,
            Err(Failure::OauthRefreshAmbiguous)
        );
        let lease = repository.lock_dispatch("profile").await?.expect("profile");
        assert_eq!(
            lease.authorization().expect("authorization").quarantine,
            Some(OauthQuarantineCause::RefreshAmbiguous)
        );
        Ok(())
    }
}
