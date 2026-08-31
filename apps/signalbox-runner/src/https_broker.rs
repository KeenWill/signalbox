//! One-shot HTTPS CONNECT broker for runner-restricted namespaces.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
};

use signalbox_network_policy::is_public_destination_address;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::AllowedNetworkHost;

const CONNECT_HEADER_BYTES: usize = 8 * 1024;
const RESOLVED_DESTINATIONS: usize = 32;
const TLS_RECORD_BYTES: usize = 18 * 1024;
const TLS_HANDSHAKE_BYTES: usize = 64 * 1024;
const HTTPS_PORT: u16 = 443;

/// Sanitized failure from one brokered HTTPS tunnel.
#[derive(Debug)]
pub enum HttpsBrokerError {
    /// The request was not one bounded canonical HTTP/1.1 CONNECT request.
    InvalidConnect,
    /// The requested hostname is outside the configured closed inventory.
    HostUnavailable,
    /// Name resolution failed or returned no destination.
    Resolution(io::Error),
    /// Resolution returned at least one destination outside public address space.
    NonPublicDestination,
    /// No resolved public destination accepted the TCP connection.
    Connect(io::Error),
    /// The first TLS handshake was not a bounded ClientHello for the admitted host.
    InvalidClientHello,
    /// Tunnel I/O failed after admission.
    Io(io::Error),
    /// The caller-supplied whole-tunnel deadline elapsed.
    TimedOut,
}

impl fmt::Display for HttpsBrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConnect => "runner HTTPS broker rejected the CONNECT request",
            Self::HostUnavailable => "runner HTTPS broker rejected the requested host",
            Self::Resolution(_) => "runner HTTPS broker could not resolve the requested host",
            Self::NonPublicDestination => "runner HTTPS broker rejected a non-public destination",
            Self::Connect(_) => "runner HTTPS broker could not connect to the requested host",
            Self::InvalidClientHello => "runner HTTPS broker rejected the TLS ClientHello",
            Self::Io(_) => "runner HTTPS broker tunnel I/O failed",
            Self::TimedOut => "runner HTTPS broker tunnel timed out",
        })
    }
}

impl Error for HttpsBrokerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Resolution(source) | Self::Connect(source) | Self::Io(source) => Some(source),
            Self::InvalidConnect
            | Self::HostUnavailable
            | Self::NonPublicDestination
            | Self::InvalidClientHello
            | Self::TimedOut => None,
        }
    }
}

/// Injectable hostname resolver; production resolves once per tunnel.
pub trait HttpsHostResolver: Clone + Send + Sync {
    /// Resolves one already-admitted canonical hostname.
    fn resolve(&self, hostname: &str) -> impl Future<Output = io::Result<Vec<IpAddr>>> + Send;
}

/// Injectable pinned-address connector.
pub trait HttpsConnector: Clone + Send + Sync {
    /// Connected byte stream returned to the broker.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send;

    /// Connects to one already-resolved exact socket address.
    fn connect(
        &self,
        destination: SocketAddr,
    ) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

/// Tokio resolver used by the executable runner.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioHttpsHostResolver;

impl HttpsHostResolver for TokioHttpsHostResolver {
    async fn resolve(&self, hostname: &str) -> io::Result<Vec<IpAddr>> {
        let mut addresses = BTreeSet::new();
        for address in tokio::net::lookup_host((hostname, HTTPS_PORT)).await? {
            addresses.insert(address.ip());
            if addresses.len() > RESOLVED_DESTINATIONS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "host resolved above the destination bound",
                ));
            }
        }
        Ok(addresses.into_iter().collect())
    }
}

/// Tokio connector that consumes the exact resolved address without DNS reuse.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioHttpsConnector;

impl HttpsConnector for TokioHttpsConnector {
    type Stream = tokio::net::TcpStream;

    async fn connect(&self, destination: SocketAddr) -> io::Result<Self::Stream> {
        tokio::net::TcpStream::connect(destination).await
    }
}

/// One configured runner-owned HTTPS broker.
#[derive(Clone, Debug)]
pub struct HttpsBroker<Resolver = TokioHttpsHostResolver, Connector = TokioHttpsConnector> {
    allowed_hosts: Vec<AllowedNetworkHost>,
    resolver: Resolver,
    connector: Connector,
}

impl HttpsBroker {
    /// Constructs the production broker from the checked closed host inventory.
    pub fn production(allowed_hosts: &[AllowedNetworkHost]) -> Self {
        Self::with_components(allowed_hosts, TokioHttpsHostResolver, TokioHttpsConnector)
    }
}

impl<Resolver, Connector> HttpsBroker<Resolver, Connector>
where
    Resolver: HttpsHostResolver,
    Connector: HttpsConnector,
{
    /// Constructs a broker around injected resolution and connection boundaries.
    pub fn with_components(
        allowed_hosts: &[AllowedNetworkHost],
        resolver: Resolver,
        connector: Connector,
    ) -> Self {
        Self {
            allowed_hosts: allowed_hosts.to_vec(),
            resolver,
            connector,
        }
    }

    /// Serves one CONNECT request, pins one public destination, authenticates
    /// the TLS SNI, and relays until either peer closes or the deadline elapses.
    pub async fn tunnel<Client>(
        &self,
        client: Client,
        deadline: tokio::time::Instant,
    ) -> Result<(), HttpsBrokerError>
    where
        Client: AsyncRead + AsyncWrite + Unpin + Send,
    {
        tokio::time::timeout_at(deadline, self.tunnel_before_deadline(client, deadline))
            .await
            .map_err(|_| HttpsBrokerError::TimedOut)?
    }

    async fn tunnel_before_deadline<Client>(
        &self,
        mut client: Client,
        deadline: tokio::time::Instant,
    ) -> Result<(), HttpsBrokerError>
    where
        Client: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let request = read_connect_request(&mut client).await?;
        let hostname = parse_connect_request(&request)?;
        if !self
            .allowed_hosts
            .iter()
            .copied()
            .any(|allowed| allowed.admits_hostname(hostname))
        {
            return Err(HttpsBrokerError::HostUnavailable);
        }
        let addresses = self
            .resolver
            .resolve(hostname)
            .await
            .map_err(HttpsBrokerError::Resolution)?;
        if addresses.is_empty() || addresses.len() > RESOLVED_DESTINATIONS {
            return Err(HttpsBrokerError::Resolution(io::Error::new(
                io::ErrorKind::InvalidData,
                "host resolved outside the destination count bound",
            )));
        }
        if addresses
            .iter()
            .any(|address| !is_public_destination_address(*address))
        {
            return Err(HttpsBrokerError::NonPublicDestination);
        }
        let mut upstream = connect_resolved(&self.connector, &addresses, deadline).await?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(HttpsBrokerError::Io)?;
        client.flush().await.map_err(HttpsBrokerError::Io)?;
        let client_hello = read_client_hello(&mut client).await?;
        let sni = client_hello_sni(&client_hello.handshake)?;
        if sni != hostname {
            return Err(HttpsBrokerError::InvalidClientHello);
        }
        upstream
            .write_all(&client_hello.records)
            .await
            .map_err(HttpsBrokerError::Io)?;
        upstream.flush().await.map_err(HttpsBrokerError::Io)?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream)
            .await
            .map_err(HttpsBrokerError::Io)?;
        Ok(())
    }
}

async fn connect_resolved<Connector>(
    connector: &Connector,
    addresses: &[IpAddr],
    deadline: tokio::time::Instant,
) -> Result<Connector::Stream, HttpsBrokerError>
where
    Connector: HttpsConnector,
{
    let mut last_error = None;
    for (index, address) in addresses.iter().enumerate() {
        let attempts_left = addresses.len() - index;
        let attempt_count = u32::try_from(attempts_left).unwrap_or(u32::MAX);
        let attempt_budget =
            deadline.saturating_duration_since(tokio::time::Instant::now()) / attempt_count;
        match tokio::time::timeout(
            attempt_budget,
            connector.connect(SocketAddr::new(*address, HTTPS_PORT)),
        )
        .await
        {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "resolved destination connection attempt timed out",
                ));
            }
        }
    }
    Err(HttpsBrokerError::Connect(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no resolved destination")
    })))
}

async fn read_connect_request<Client>(client: &mut Client) -> Result<Vec<u8>, HttpsBrokerError>
where
    Client: AsyncRead + Unpin,
{
    let mut request = Vec::new();
    loop {
        if request.len() == CONNECT_HEADER_BYTES {
            return Err(HttpsBrokerError::InvalidConnect);
        }
        let byte = match client.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(HttpsBrokerError::InvalidConnect);
            }
            Err(error) => return Err(HttpsBrokerError::Io(error)),
        };
        request.push(byte);
        if request.ends_with(b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn parse_connect_request(request: &[u8]) -> Result<&str, HttpsBrokerError> {
    let request = std::str::from_utf8(request).map_err(|_| HttpsBrokerError::InvalidConnect)?;
    let mut lines = request
        .strip_suffix("\r\n\r\n")
        .ok_or(HttpsBrokerError::InvalidConnect)?
        .split("\r\n");
    let request_line = lines.next().ok_or(HttpsBrokerError::InvalidConnect)?;
    let mut fields = request_line.split(' ');
    if fields.next() != Some("CONNECT")
        || fields.next().is_none()
        || fields.next() != Some("HTTP/1.1")
        || fields.next().is_some()
    {
        return Err(HttpsBrokerError::InvalidConnect);
    }
    let authority = request_line
        .strip_prefix("CONNECT ")
        .and_then(|line| line.strip_suffix(" HTTP/1.1"))
        .ok_or(HttpsBrokerError::InvalidConnect)?;
    let hostname = authority
        .strip_suffix(":443")
        .filter(|hostname| canonical_dns_name(hostname))
        .ok_or(HttpsBrokerError::InvalidConnect)?;
    let mut host_header = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpsBrokerError::InvalidConnect)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !value
                .bytes()
                .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(HttpsBrokerError::InvalidConnect);
        }
        if name.eq_ignore_ascii_case("host") && host_header.replace(value.trim()).is_some() {
            return Err(HttpsBrokerError::InvalidConnect);
        }
    }
    if host_header != Some(authority) {
        return Err(HttpsBrokerError::InvalidConnect);
    }
    Ok(hostname)
}

fn canonical_dns_name(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && hostname.parse::<IpAddr>().is_err()
}

struct CapturedClientHello {
    records: Vec<u8>,
    handshake: Vec<u8>,
}

async fn read_client_hello<Client>(
    client: &mut Client,
) -> Result<CapturedClientHello, HttpsBrokerError>
where
    Client: AsyncRead + Unpin,
{
    let mut records = Vec::new();
    let mut handshake = Vec::new();
    loop {
        let mut header = [0_u8; 5];
        client
            .read_exact(&mut header)
            .await
            .map_err(classify_client_hello_read_error)?;
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if header[0] != 22 || header[1] != 3 || length == 0 || length > TLS_RECORD_BYTES {
            return Err(HttpsBrokerError::InvalidClientHello);
        }
        let mut payload = vec![0_u8; length];
        client
            .read_exact(&mut payload)
            .await
            .map_err(classify_client_hello_read_error)?;
        records.extend_from_slice(&header);
        records.extend_from_slice(&payload);
        handshake.extend_from_slice(&payload);
        if handshake.len() > TLS_HANDSHAKE_BYTES {
            return Err(HttpsBrokerError::InvalidClientHello);
        }
        if handshake.len() >= 4 {
            let expected = 4
                + ((usize::from(handshake[1]) << 16)
                    | (usize::from(handshake[2]) << 8)
                    | usize::from(handshake[3]));
            if expected > TLS_HANDSHAKE_BYTES {
                return Err(HttpsBrokerError::InvalidClientHello);
            }
            if handshake.len() >= expected {
                handshake.truncate(expected);
                return Ok(CapturedClientHello { records, handshake });
            }
        }
    }
}

fn classify_client_hello_read_error(error: io::Error) -> HttpsBrokerError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        HttpsBrokerError::InvalidClientHello
    } else {
        HttpsBrokerError::Io(error)
    }
}

fn client_hello_sni(handshake: &[u8]) -> Result<&str, HttpsBrokerError> {
    if handshake.first() != Some(&1) || handshake.len() < 4 + 2 + 32 + 1 {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    let mut cursor = 4 + 2 + 32;
    let session_length = take_u8(handshake, &mut cursor)?;
    take(handshake, &mut cursor, usize::from(session_length))?;
    let cipher_length = take_u16(handshake, &mut cursor)?;
    if cipher_length == 0 || cipher_length % 2 != 0 {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    take(handshake, &mut cursor, usize::from(cipher_length))?;
    let compression_length = take_u8(handshake, &mut cursor)?;
    if compression_length == 0 {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    take(handshake, &mut cursor, usize::from(compression_length))?;
    let extensions_length = usize::from(take_u16(handshake, &mut cursor)?);
    let extensions = take(handshake, &mut cursor, extensions_length)?;
    if cursor != handshake.len() {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    parse_sni_extension(extensions)
}

fn parse_sni_extension(mut extensions: &[u8]) -> Result<&str, HttpsBrokerError> {
    let mut hostname = None;
    while !extensions.is_empty() {
        let mut cursor = 0;
        let kind = take_u16(extensions, &mut cursor)?;
        let length = usize::from(take_u16(extensions, &mut cursor)?);
        let value = take(extensions, &mut cursor, length)?;
        extensions = &extensions[cursor..];
        if kind == 0 {
            if hostname.is_some() {
                return Err(HttpsBrokerError::InvalidClientHello);
            }
            hostname = Some(parse_server_name(value)?);
        }
    }
    hostname.ok_or(HttpsBrokerError::InvalidClientHello)
}

fn parse_server_name(value: &[u8]) -> Result<&str, HttpsBrokerError> {
    let mut cursor = 0;
    let list_length = usize::from(take_u16(value, &mut cursor)?);
    let names = take(value, &mut cursor, list_length)?;
    if cursor != value.len() {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    let mut name_cursor = 0;
    if take_u8(names, &mut name_cursor)? != 0 {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    let name_length = usize::from(take_u16(names, &mut name_cursor)?);
    let hostname = take(names, &mut name_cursor, name_length)?;
    if name_cursor != names.len() {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    let hostname =
        std::str::from_utf8(hostname).map_err(|_| HttpsBrokerError::InvalidClientHello)?;
    if !canonical_dns_name(hostname) {
        return Err(HttpsBrokerError::InvalidClientHello);
    }
    Ok(hostname)
}

fn take<'a>(
    input: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], HttpsBrokerError> {
    let end = cursor
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .ok_or(HttpsBrokerError::InvalidClientHello)?;
    let value = &input[*cursor..end];
    *cursor = end;
    Ok(value)
}

fn take_u8(input: &[u8], cursor: &mut usize) -> Result<u8, HttpsBrokerError> {
    Ok(take(input, cursor, 1)?[0])
}

fn take_u16(input: &[u8], cursor: &mut usize) -> Result<u16, HttpsBrokerError> {
    let bytes = take(input, cursor, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        net::{Ipv4Addr, Ipv6Addr},
        sync::{Arc, Mutex},
    };

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

    use super::*;

    const PUBLIC_DESTINATION: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));

    #[derive(Clone)]
    struct FixedResolver {
        addresses: Vec<IpAddr>,
    }

    impl HttpsHostResolver for FixedResolver {
        async fn resolve(&self, _hostname: &str) -> io::Result<Vec<IpAddr>> {
            Ok(self.addresses.clone())
        }
    }

    #[derive(Clone)]
    struct FixedConnector {
        stream: Arc<Mutex<Option<DuplexStream>>>,
        destinations: Arc<Mutex<Vec<SocketAddr>>>,
    }

    impl HttpsConnector for FixedConnector {
        type Stream = DuplexStream;

        async fn connect(&self, destination: SocketAddr) -> io::Result<Self::Stream> {
            self.destinations
                .lock()
                .expect("the destination fixture is available")
                .push(destination);
            Ok(self
                .stream
                .lock()
                .expect("the upstream fixture is available")
                .take()
                .expect("one upstream fixture is configured"))
        }
    }

    fn broker_fixture(
        addresses: Vec<IpAddr>,
    ) -> (
        HttpsBroker<FixedResolver, FixedConnector>,
        DuplexStream,
        Arc<Mutex<Vec<SocketAddr>>>,
    ) {
        let (broker_stream, upstream_stream) = tokio::io::duplex(16 * 1024);
        let destinations = Arc::new(Mutex::new(Vec::new()));
        let broker = HttpsBroker::with_components(
            &[AllowedNetworkHost::GithubCom],
            FixedResolver { addresses },
            FixedConnector {
                stream: Arc::new(Mutex::new(Some(broker_stream))),
                destinations: destinations.clone(),
            },
        );
        (broker, upstream_stream, destinations)
    }

    fn connect_request(hostname: &str) -> Vec<u8> {
        format!("CONNECT {hostname}:443 HTTP/1.1\r\nHost: {hostname}:443\r\n\r\n").into_bytes()
    }

    fn tunnel_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(5)
    }

    fn client_hello(hostname: &str) -> Vec<u8> {
        let mut server_name = Vec::new();
        server_name.extend_from_slice(&(hostname.len() as u16).to_be_bytes());
        server_name.extend_from_slice(hostname.as_bytes());
        server_name.insert(0, 0);
        let mut server_names = Vec::new();
        server_names.extend_from_slice(&(server_name.len() as u16).to_be_bytes());
        server_names.extend_from_slice(&server_name);
        let mut extension = vec![0, 0];
        extension.extend_from_slice(&(server_names.len() as u16).to_be_bytes());
        extension.extend_from_slice(&server_names);
        let mut body = vec![3, 3];
        body.extend_from_slice(&[7; 32]);
        body.push(0);
        body.extend_from_slice(&[0, 2, 0x13, 0x01]);
        body.extend_from_slice(&[1, 0]);
        body.extend_from_slice(&(extension.len() as u16).to_be_bytes());
        body.extend_from_slice(&extension);
        let mut handshake = vec![1];
        let length = body.len();
        handshake.extend_from_slice(&[
            ((length >> 16) & 0xff) as u8,
            ((length >> 8) & 0xff) as u8,
            (length & 0xff) as u8,
        ]);
        handshake.extend_from_slice(&body);
        let mut record = vec![22, 3, 1];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    #[test]
    fn configured_host_admission_is_label_boundary_exact() {
        assert!(AllowedNetworkHost::GithubCom.admits_hostname("github.com"));
        assert!(
            AllowedNetworkHost::GithubCom.admits_hostname("objects.githubusercontent.github.com")
        );
        assert!(!AllowedNetworkHost::GithubCom.admits_hostname("notgithub.com"));
        assert!(!AllowedNetworkHost::ApiAnthropicCom.admits_hostname("x.api.anthropic.com"));
    }

    #[test]
    fn direct_ip_connect_authority_fails_closed() {
        assert!(!canonical_dns_name("127.0.0.1"));
        assert!(matches!(
            parse_connect_request(&connect_request("127.0.0.1")),
            Err(HttpsBrokerError::InvalidConnect)
        ));
    }

    #[test]
    fn non_public_resolution_classes_fail_closed() {
        assert!(!is_public_destination_address(IpAddr::V4(Ipv4Addr::new(
            10, 0, 0, 1
        ))));
        assert!(!is_public_destination_address(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 1
        ))));
        assert!(!is_public_destination_address(IpAddr::V6(
            Ipv6Addr::LOCALHOST
        )));
        assert!(!is_public_destination_address(IpAddr::V6(
            "2001:db8::1"
                .parse()
                .expect("the documentation address fixture parses")
        )));
        assert!(is_public_destination_address(PUBLIC_DESTINATION));
    }

    #[derive(Clone)]
    struct BlackholeThenConnect {
        blackhole: IpAddr,
        stream: Arc<Mutex<Option<DuplexStream>>>,
        destinations: Arc<Mutex<Vec<SocketAddr>>>,
    }

    impl HttpsConnector for BlackholeThenConnect {
        type Stream = DuplexStream;

        async fn connect(&self, destination: SocketAddr) -> io::Result<Self::Stream> {
            self.destinations
                .lock()
                .expect("the destination fixture is available")
                .push(destination);
            if destination.ip() == self.blackhole {
                pending().await
            }
            Ok(self
                .stream
                .lock()
                .expect("the upstream fixture is available")
                .take()
                .expect("one upstream fixture is configured"))
        }
    }

    #[tokio::test]
    async fn blackholed_destination_leaves_time_for_the_next_address() {
        let expected_blackhole: SocketAddr = "93.184.216.35:443"
            .parse()
            .expect("the blackholed destination fixture parses");
        let expected_reachable: SocketAddr = "93.184.216.34:443"
            .parse()
            .expect("the reachable destination fixture parses");
        let blackhole = expected_blackhole.ip();
        let reachable = expected_reachable.ip();
        let (broker_stream, _upstream_stream) = tokio::io::duplex(4096);
        let destinations = Arc::new(Mutex::new(Vec::new()));
        let connector = BlackholeThenConnect {
            blackhole,
            stream: Arc::new(Mutex::new(Some(broker_stream))),
            destinations: destinations.clone(),
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);

        let connected = connect_resolved(&connector, &[blackhole, reachable], deadline).await;
        let observed = destinations
            .lock()
            .expect("the destination fixture is available")
            .clone();

        assert!(connected.is_ok());
        assert_eq!(observed, vec![expected_blackhole, expected_reachable]);
    }

    #[tokio::test]
    async fn truncated_client_hello_record_header_is_invalid() {
        let (mut client, mut broker_client) = tokio::io::duplex(4096);
        client
            .write_all(&[22, 3])
            .await
            .expect("the partial TLS record header writes");
        client
            .shutdown()
            .await
            .expect("the partial TLS record header closes");

        let rejected = read_client_hello(&mut broker_client).await;

        assert!(matches!(
            rejected,
            Err(HttpsBrokerError::InvalidClientHello)
        ));
    }

    #[tokio::test]
    async fn truncated_client_hello_record_payload_is_invalid() {
        let (mut client, mut broker_client) = tokio::io::duplex(4096);
        client
            .write_all(&[22, 3, 1, 0, 4, 1, 0])
            .await
            .expect("the partial TLS record payload writes");
        client
            .shutdown()
            .await
            .expect("the partial TLS record payload closes");

        let rejected = read_client_hello(&mut broker_client).await;

        assert!(matches!(
            rejected,
            Err(HttpsBrokerError::InvalidClientHello)
        ));
    }

    #[tokio::test]
    async fn truncated_connect_request_is_invalid_admission() {
        let (broker, _upstream, destinations) = broker_fixture(vec![PUBLIC_DESTINATION]);
        let (mut client, broker_client) = tokio::io::duplex(4096);
        let client_task = async move {
            client
                .write_all(b"CONNECT github.com:443 HTTP/1.1\r\nHost:")
                .await
                .expect("the partial CONNECT request writes");
            client.shutdown().await.expect("the partial request closes");
        };
        let broker_task = broker.tunnel(broker_client, tunnel_deadline());
        let ((), rejected) = tokio::join!(client_task, broker_task);

        assert!(matches!(rejected, Err(HttpsBrokerError::InvalidConnect)));
        assert!(
            destinations
                .lock()
                .expect("the destination fixture is available")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn non_public_resolution_fails_before_connection() {
        let (broker, _upstream, destinations) =
            broker_fixture(vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
        let (mut client, broker_client) = tokio::io::duplex(4096);
        let request = connect_request("github.com");
        let client_task = async move {
            client
                .write_all(&request)
                .await
                .expect("the CONNECT request writes");
        };
        let broker_task = broker.tunnel(broker_client, tunnel_deadline());
        let ((), rejected) = tokio::join!(client_task, broker_task);

        assert!(matches!(
            rejected,
            Err(HttpsBrokerError::NonPublicDestination)
        ));
        assert!(
            destinations
                .lock()
                .expect("the destination fixture is available")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn oversized_resolution_fails_before_connection() {
        let (broker, _upstream, destinations) =
            broker_fixture(vec![PUBLIC_DESTINATION; RESOLVED_DESTINATIONS + 1]);
        let (mut client, broker_client) = tokio::io::duplex(4096);
        let request = connect_request("github.com");
        let client_task = async move {
            client
                .write_all(&request)
                .await
                .expect("the CONNECT request writes");
        };
        let broker_task = broker.tunnel(broker_client, tunnel_deadline());
        let ((), rejected) = tokio::join!(client_task, broker_task);

        assert!(matches!(rejected, Err(HttpsBrokerError::Resolution(_))));
        assert!(
            destinations
                .lock()
                .expect("the destination fixture is available")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn exact_sni_tunnels_through_the_pinned_public_destination() {
        let (broker, mut upstream, destinations) = broker_fixture(vec![PUBLIC_DESTINATION]);
        let (mut client, broker_client) = tokio::io::duplex(16 * 1024);
        let request = connect_request("github.com");
        let hello = client_hello("github.com");
        let expected_hello = hello.clone();
        let hello_length = expected_hello.len();
        let expected_connect_response = b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec();
        let connect_response_length = expected_connect_response.len();
        let client_payload = b"client payload".to_vec();
        let expected_client_payload = client_payload.clone();
        let client_payload_length = expected_client_payload.len();
        let upstream_reply = b"upstream reply".to_vec();
        let expected_upstream_reply = upstream_reply.clone();
        let upstream_reply_length = expected_upstream_reply.len();
        let client_task = async move {
            client
                .write_all(&request)
                .await
                .expect("the CONNECT request writes");
            let mut response = vec![0_u8; connect_response_length];
            client
                .read_exact(&mut response)
                .await
                .expect("the CONNECT response reads");
            client
                .write_all(&hello)
                .await
                .expect("the ClientHello writes");
            client
                .write_all(&client_payload)
                .await
                .expect("the tunneled payload writes");
            let mut reply = vec![0_u8; upstream_reply_length];
            client
                .read_exact(&mut reply)
                .await
                .expect("the tunneled reply reads");
            client.shutdown().await.expect("the client tunnel closes");
            (response, reply)
        };
        let upstream_task = async move {
            let mut observed_hello = vec![0_u8; hello_length];
            upstream
                .read_exact(&mut observed_hello)
                .await
                .expect("the admitted ClientHello reaches upstream");
            let mut payload = vec![0_u8; client_payload_length];
            upstream
                .read_exact(&mut payload)
                .await
                .expect("the tunneled payload reaches upstream");
            upstream
                .write_all(&upstream_reply)
                .await
                .expect("the upstream reply writes");
            upstream.shutdown().await.expect("the upstream closes");
            (observed_hello, payload)
        };
        let broker_task = broker.tunnel(broker_client, tunnel_deadline());
        let ((response, reply), (observed_hello, payload), tunneled) =
            tokio::join!(client_task, upstream_task, broker_task);
        let observed_destinations = destinations
            .lock()
            .expect("the destination fixture is available")
            .clone();
        let expected_destination = "93.184.216.34:443"
            .parse()
            .expect("the pinned HTTPS destination fixture parses");

        assert_eq!(response, expected_connect_response);
        assert_eq!(reply, expected_upstream_reply);
        assert_eq!(observed_hello, expected_hello);
        assert_eq!(payload, expected_client_payload);
        assert!(tunneled.is_ok());
        assert_eq!(observed_destinations, vec![expected_destination]);
    }

    #[tokio::test]
    async fn mismatched_sni_fails_before_any_tls_bytes_reach_upstream() {
        let (broker, mut upstream, _destinations) = broker_fixture(vec![PUBLIC_DESTINATION]);
        let (mut client, broker_client) = tokio::io::duplex(16 * 1024);
        let request = connect_request("github.com");
        let hello = client_hello("crates.io");
        let client_task = async move {
            client
                .write_all(&request)
                .await
                .expect("the CONNECT request writes");
            let mut response = vec![0_u8; 39];
            client
                .read_exact(&mut response)
                .await
                .expect("the CONNECT response reads");
            client
                .write_all(&hello)
                .await
                .expect("the mismatched ClientHello writes");
            response
        };
        let broker_task = broker.tunnel(broker_client, tunnel_deadline());
        let (response, rejected) = tokio::join!(client_task, broker_task);
        let mut observed = [0_u8; 1];
        let read = upstream
            .read(&mut observed)
            .await
            .expect("the closed upstream reads EOF");

        assert_eq!(response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        assert!(matches!(
            rejected,
            Err(HttpsBrokerError::InvalidClientHello)
        ));
        assert_eq!(read, 0);
    }

    #[tokio::test]
    async fn whole_tunnel_deadline_bounds_an_idle_client() {
        let (broker, _upstream, _destinations) = broker_fixture(vec![PUBLIC_DESTINATION]);
        let (_client, broker_client) = tokio::io::duplex(4096);

        let rejected = broker
            .tunnel(broker_client, tokio::time::Instant::now())
            .await;

        assert!(matches!(rejected, Err(HttpsBrokerError::TimedOut)));
    }
}
