//! Reloadable authenticated webhook wakes for the repository task.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
};
use ring::hmac;
use signalbox_domain::RepositorySlug;
use signalbox_model_runtime::{CredentialAccess, CredentialReference};
use signalbox_module_repo_watch_v2::{
    RepoWatchStore, WebhookAdmission, WebhookDelivery, WebhookDisposition,
};
use signalbox_ownership_seam::OffsetDateTime;
use tokio::{
    net::TcpListener,
    sync::{Notify, RwLock, oneshot},
    task::JoinSet,
};
use uuid::Uuid;

use crate::{
    FileCredentialAccess, RepositoryWatchConfiguration, configuration::RepositoryWatchWebhookMode,
};

struct Hook {
    store: RepoWatchStore,
    retention: Duration,
    repository: RepositorySlug,
    credentials: FileCredentialAccess,
    reference: CredentialReference,
    mode: RepositoryWatchWebhookMode,
    wake: Arc<Notify>,
}

#[derive(Default)]
struct Routing {
    path: String,
    hooks: BTreeMap<u64, Hook>,
}

type SharedRouting = Arc<RwLock<Arc<Routing>>>;

/// A replacement socket is reserved before any running configuration changes.
pub(crate) struct PreparedListener {
    address: Option<SocketAddr>,
    socket: Option<TcpListener>,
    routing: Arc<Routing>,
}

struct Binding {
    address: SocketAddr,
    socket: SocketState,
}

enum SocketState {
    Reserved(TcpListener),
    Running(oneshot::Sender<()>),
}

#[derive(Default)]
pub(crate) struct WebhookListener {
    routing: SharedRouting,
    binding: Option<Binding>,
    servers: JoinSet<Result<(), std::io::Error>>,
}

impl WebhookListener {
    pub(crate) async fn prepare(
        &self,
        configuration: Option<&RepositoryWatchConfiguration>,
        wakes: &BTreeMap<RepositorySlug, Arc<Notify>>,
        store: &RepoWatchStore,
    ) -> Result<PreparedListener, std::io::Error> {
        let configuration = configuration.filter(|configuration| configuration.enabled());
        let webhook = configuration.and_then(RepositoryWatchConfiguration::webhook);
        let address = webhook.map(|webhook| webhook.bind_address());
        let socket = match address {
            Some(address)
                if self
                    .binding
                    .as_ref()
                    .is_none_or(|old| old.address != address) =>
            {
                Some(TcpListener::bind(address).await?)
            }
            _ => None,
        };
        let mut routing = Routing::default();
        if let Some(webhook) = webhook {
            routing.path = webhook.path().to_owned();
        }
        for (repository, retention) in configuration.into_iter().flat_map(|config| {
            config
                .repositories()
                .iter()
                .map(move |repository| (repository, config.webhook_retention()))
        }) {
            if let Some(webhook) = repository.webhook()
                && let Some(reference) = repository.webhook_secret_reference()
                && let Some(wake) = wakes.get(repository.repository())
            {
                routing.hooks.insert(
                    webhook.hook_id().get(),
                    Hook {
                        store: store.clone(),
                        retention,
                        repository: repository.repository().clone(),
                        credentials: FileCredentialAccess::new(
                            webhook.secret_file().to_path_buf(),
                            reference.clone(),
                        ),
                        reference,
                        mode: webhook.mode(),
                        wake: wake.clone(),
                    },
                );
            }
        }
        Ok(PreparedListener {
            address,
            socket,
            routing: Arc::new(routing),
        })
    }

    pub(crate) async fn apply(&mut self, prepared: PreparedListener) {
        *self.routing.write().await = prepared.routing;
        if self.binding.as_ref().map(|binding| binding.address) != prepared.address {
            if let Some(old) = self.binding.take()
                && let SocketState::Running(shutdown) = old.socket
            {
                let _ = shutdown.send(());
            }
            if let (Some(address), Some(socket)) = (prepared.address, prepared.socket) {
                self.binding = Some(Binding {
                    address,
                    socket: SocketState::Reserved(socket),
                });
            }
        }
    }

    pub(crate) fn start(&mut self) {
        let Some(binding) = self.binding.take() else {
            return;
        };
        let socket = match binding.socket {
            SocketState::Reserved(socket) => {
                let router = Router::new()
                    .fallback(delivery)
                    .with_state(self.routing.clone());
                let (shutdown, stopped) = oneshot::channel();
                self.servers.spawn(async move {
                    axum::serve(socket, router)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                });
                SocketState::Running(shutdown)
            }
            running @ SocketState::Running(_) => running,
        };
        self.binding = Some(Binding {
            address: binding.address,
            socket,
        });
    }

    pub(crate) fn failed(&mut self) -> bool {
        while let Some(completed) = self.servers.try_join_next() {
            if !matches!(completed, Ok(Ok(()))) {
                return true;
            }
        }
        false
    }

    pub(crate) async fn shutdown(&mut self) {
        if let Some(binding) = self.binding.take()
            && let SocketState::Running(shutdown) = binding.socket
        {
            let _ = shutdown.send(());
        }
        while self.servers.join_next().await.is_some() {}
    }
}

async fn delivery(
    State(routing): State<SharedRouting>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED;
    }
    let Some(hook_id) = headers
        .get("x-github-hook-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("sha256="))
        .and_then(|value| hex::decode(value).ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let Some(delivery_id) = headers
        .get("x-github-delivery")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
    else {
        return StatusCode::BAD_REQUEST;
    };
    let Some(event) = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .filter(|event| !event.is_empty())
    else {
        return StatusCode::BAD_REQUEST;
    };
    loop {
        let snapshot = routing.read().await.clone();
        if uri.path() != snapshot.path {
            return StatusCode::NOT_FOUND;
        }
        let Some(hook) = snapshot.hooks.get(&hook_id) else {
            return StatusCode::UNAUTHORIZED;
        };
        let credential = hook.credentials.resolve(&hook.reference).await;
        let current = routing.read().await;
        // A reload during credential I/O retries admission against the new hook map.
        if !Arc::ptr_eq(&snapshot, &current) {
            continue;
        }
        let Ok(credential) = credential else {
            return StatusCode::SERVICE_UNAVAILABLE;
        };
        if credential.expose_bytes().is_empty() {
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        if hmac::verify(
            &hmac::Key::new(hmac::HMAC_SHA256, credential.expose_bytes()),
            &body,
            &signature,
        )
        .is_err()
        {
            return StatusCode::UNAUTHORIZED;
        }
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return StatusCode::BAD_REQUEST;
        };
        let repository = payload
            .get("repository")
            .and_then(|value| value.get("full_name"))
            .and_then(serde_json::Value::as_str)
            .and_then(|name| RepositorySlug::try_new(name.to_owned()).ok());
        if repository.as_ref() != Some(&hook.repository) {
            return StatusCode::BAD_REQUEST;
        }
        let received_at = OffsetDateTime::now_utc();
        let Ok(retention) = hook.retention.try_into() else {
            return StatusCode::SERVICE_UNAVAILABLE;
        };
        let Some(expires_at) = received_at.checked_add(retention) else {
            return StatusCode::SERVICE_UNAVAILABLE;
        };
        return admit_and_wake(
            hook,
            WebhookDelivery {
                repository: &hook.repository,
                hook_id,
                delivery_id,
                event,
                action: payload.get("action").and_then(serde_json::Value::as_str),
                body: &body,
                received_at,
                expires_at,
            },
        )
        .await;
    }
}

async fn admit_and_wake(hook: &Hook, delivery: WebhookDelivery<'_>) -> StatusCode {
    let hook_id = delivery.hook_id;
    let delivery_id = delivery.delivery_id;
    match hook.store.admit_webhook(delivery).await {
        Ok(WebhookAdmission::Inserted | WebhookAdmission::Replayed) => {}
        Ok(WebhookAdmission::ConflictingReuse) => return StatusCode::CONFLICT,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    }
    let disposition = match hook.mode {
        RepositoryWatchWebhookMode::Primary => {
            hook.wake.notify_one();
            WebhookDisposition::Applied
        }
        RepositoryWatchWebhookMode::Shadow => WebhookDisposition::Ignored,
    };
    if hook
        .store
        .settle_webhook(hook_id, delivery_id, disposition, OffsetDateTime::now_utc())
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::ACCEPTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[tokio::test]
    async fn empty_resolved_secrets_reject_signed_deliveries_without_waking_the_repository() {
        const FIXTURE_HOOK_ID: u64 = 17;
        const FIXTURE_PATH: &str = "/webhook";
        const FIXTURE_BODY: &[u8] = br#"{"repository":{"full_name":"example/project"}}"#;
        let directory = tempfile::tempdir().expect("credential directory");
        let path = directory.path().join("hook-secret");
        let reference = CredentialReference::new("repository-watch:example/project:webhook");
        let wake = Arc::new(Notify::new());
        let routing = Arc::new(RwLock::new(Arc::new(Routing {
            path: FIXTURE_PATH.to_owned(),
            hooks: BTreeMap::from([(
                FIXTURE_HOOK_ID,
                Hook {
                    retention: Duration::from_secs(7 * 24 * 60 * 60),
                    store: RepoWatchStore::new(
                        sqlx::postgres::PgPoolOptions::new()
                            .connect_lazy_with(sqlx::postgres::PgConnectOptions::new()),
                    ),
                    repository: RepositorySlug::try_new(String::from("example/project"))
                        .expect("repository slug"),
                    credentials: FileCredentialAccess::new(path.clone(), reference.clone()),
                    reference,
                    mode: RepositoryWatchWebhookMode::Primary,
                    wake: wake.clone(),
                },
            )]),
        })));
        let empty_key_signature = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, b""), FIXTURE_BODY);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-github-delivery",
            Uuid::now_v7().to_string().parse().expect("delivery header"),
        );
        headers.insert("x-github-event", "push".parse().expect("event header"));
        headers.insert(
            "x-github-hook-id",
            FIXTURE_HOOK_ID.to_string().parse().expect("hook header"),
        );
        headers.insert(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(empty_key_signature.as_ref()))
                .parse()
                .expect("signature header"),
        );
        for file_bytes in [b"".as_slice(), b"\r\n".as_slice(), b"\n\r\n".as_slice()] {
            std::fs::write(&path, file_bytes).expect("write empty resolved secret");
            assert_eq!(
                delivery(
                    State(routing.clone()),
                    Method::POST,
                    FIXTURE_PATH.parse().expect("webhook URI"),
                    headers.clone(),
                    Bytes::from_static(FIXTURE_BODY),
                )
                .await,
                StatusCode::SERVICE_UNAVAILABLE,
                "secret file bytes: {file_bytes:?}"
            );
            assert!(
                wake.notified().now_or_never().is_none(),
                "rejected delivery must not wake the repository: {file_bytes:?}"
            );
        }
    }
}
