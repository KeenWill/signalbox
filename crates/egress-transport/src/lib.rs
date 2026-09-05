//! Shared bounded HTTP egress transport for daemon-local web clients.

use std::{
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr},
    sync::LazyLock,
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{Client, Url, redirect::Policy};

const MAX_RESOLVED_ADDRESSES: usize = 32;

/// Sanitized result of one physical fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebFetchTransportFailure {
    /// Destination resolution or client setup failed before dispatch.
    RequestFailed,
    /// Dispatch began but no complete bounded response was established.
    DispatchUnknown,
}

/// The fixed production web-fetch client could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReqwestWebFetchConstructionError;

impl fmt::Display for ReqwestWebFetchConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("credential-free web fetch client construction failed")
    }
}

impl Error for ReqwestWebFetchConstructionError {}

/// Whether a body stream still holds content after an exact-cap read. Empty
/// trailing frames are legal and are not evidence that bytes were discarded.
#[doc(hidden)]
pub async fn has_more_response_bytes<S, B, E>(
    stream: &mut S,
) -> Result<bool, WebFetchTransportFailure>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| WebFetchTransportFailure::DispatchUnknown)?;
        if !chunk.as_ref().is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct ResolvedPublicDestination {
    pub host: String,
    pub addresses: Vec<SocketAddr>,
}

/// Builds one credential-free client pinned to a URL's complete admitted
/// public DNS result. `None` leaves DNS resolution and exchange time unbounded.
#[doc(hidden)]
pub async fn public_destination_client(
    url: &Url,
    exchange_timeout: Option<Duration>,
) -> Result<Client, PublicDestinationClientError> {
    let started = tokio::time::Instant::now();
    let destination = resolve_public_destination(url, exchange_timeout).await?;
    let remaining = exchange_timeout
        .map(|timeout| {
            timeout
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(PublicDestinationClientError::Infrastructure)
        })
        .transpose()?;
    build_web_fetch_client(remaining, Some(&destination))
        .map_err(|_| PublicDestinationClientError::Infrastructure)
}

/// A URL could not be resolved and pinned as a public-only destination before
/// dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum PublicDestinationClientError {
    /// The destination shape or resolved address set was not public-only.
    DestinationRejected,
    /// DNS resolution or client construction failed before dispatch.
    Infrastructure,
}

async fn resolve_public_destination(
    url: &Url,
    exchange_timeout: Option<Duration>,
) -> Result<ResolvedPublicDestination, PublicDestinationClientError> {
    let host = url
        .host_str()
        .ok_or(PublicDestinationClientError::DestinationRejected)?;
    let port = url
        .port_or_known_default()
        .ok_or(PublicDestinationClientError::DestinationRejected)?;
    let addresses = if let Some(address) = parse_url_host_ip(host) {
        vec![SocketAddr::new(address, port)]
    } else {
        let lookup = tokio::net::lookup_host((host, port));
        let resolved = match exchange_timeout {
            Some(timeout) => tokio::time::timeout(timeout, lookup)
                .await
                .map_err(|_| PublicDestinationClientError::Infrastructure)?,
            None => lookup.await,
        }
        .map_err(|_| PublicDestinationClientError::Infrastructure)?;
        resolved
            .take(MAX_RESOLVED_ADDRESSES + 1)
            .collect::<Vec<_>>()
    };
    if addresses.is_empty()
        || addresses.len() > MAX_RESOLVED_ADDRESSES
        || addresses
            .iter()
            .any(|address| !is_public_destination_address(address.ip()))
    {
        return Err(PublicDestinationClientError::DestinationRejected);
    }
    Ok(ResolvedPublicDestination {
        host: host.to_owned(),
        addresses,
    })
}

#[doc(hidden)]
/// Builds the pinned transport client; `None` omits its exchange timeout.
pub fn build_web_fetch_client(
    exchange_timeout: Option<Duration>,
    destination: Option<&ResolvedPublicDestination>,
) -> Result<Client, ReqwestWebFetchConstructionError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = Client::builder()
        .tls_backend_rustls()
        .tls_version_min(reqwest::tls::Version::TLS_1_2)
        .tls_danger_accept_invalid_certs(false)
        .tls_danger_accept_invalid_hostnames(false)
        .no_proxy()
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0);
    if let Some(timeout) = exchange_timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(destination) = destination {
        builder = builder.resolve_to_addrs(&destination.host, &destination.addresses);
    }
    builder
        .build()
        .map_err(|_| ReqwestWebFetchConstructionError)
}

#[doc(hidden)]
pub fn is_public_destination_address(address: IpAddr) -> bool {
    const DENIED_NETWORKS: [&str; 17] = [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/3",
        "2001::/23",
        "2001:db8::/32",
        "2002::/16",
    ];
    static DENIED: LazyLock<Option<Vec<ipnet::IpNet>>> = LazyLock::new(|| {
        DENIED_NETWORKS
            .into_iter()
            .map(str::parse::<ipnet::IpNet>)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    });
    static GLOBAL_V6: LazyLock<Option<ipnet::IpNet>> = LazyLock::new(|| "2000::/3".parse().ok());
    static DOCUMENTATION_V6: LazyLock<Option<ipnet::IpNet>> =
        LazyLock::new(|| "3fff::/20".parse().ok());
    let permitted_family = match address {
        IpAddr::V4(_) => true,
        IpAddr::V6(_) => GLOBAL_V6
            .as_ref()
            .is_some_and(|network| network.contains(&address)),
    };
    permitted_family
        && DENIED
            .as_ref()
            .is_some_and(|networks| !networks.iter().any(|network| network.contains(&address)))
        && DOCUMENTATION_V6
            .as_ref()
            .is_some_and(|network| !network.contains(&address))
}

#[doc(hidden)]
pub fn parse_url_host_ip(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Public address classification admits ordinary global-unicast
    /// destinations.
    #[test]
    fn web_fetch_public_destination_classification_accepts_global_addresses() {
        let public_v4 = "93.184.216.34".parse().expect("fixture IPv4 parses");
        let public_v6 = "2606:2800:220:1:248:1893:25c8:1946"
            .parse()
            .expect("fixture IPv6 parses");

        assert!(is_public_destination_address(public_v4));
        assert!(is_public_destination_address(public_v6));
    }

    /// Public address classification rejects link-local and documentation
    /// ranges that must never become fetch destinations.
    #[test]
    fn web_fetch_public_destination_classification_rejects_non_public_addresses() {
        let link_local_v4 = "169.254.169.254".parse().expect("fixture IPv4 parses");
        let documentation_v4 = "192.0.2.1".parse().expect("fixture IPv4 parses");
        let documentation_v6 = "2001:db8::1".parse().expect("fixture IPv6 parses");

        assert!(!is_public_destination_address(link_local_v4));
        assert!(!is_public_destination_address(documentation_v4));
        assert!(!is_public_destination_address(documentation_v6));
    }

    /// Empty frames after an exact-cap response do not imply retained bytes
    /// were discarded.
    #[tokio::test]
    async fn exact_body_cap_ignores_empty_trailing_chunks() {
        let mut stream = futures_util::stream::iter([
            Ok::<Vec<u8>, std::convert::Infallible>(Vec::new()),
            Ok(Vec::new()),
        ]);

        assert_eq!(has_more_response_bytes(&mut stream).await, Ok(false));
    }
}
