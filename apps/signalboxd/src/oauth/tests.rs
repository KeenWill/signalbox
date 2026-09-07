use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{
    io::{Read, Write},
    sync::Arc,
};

// Generated, public test-only localhost certificate and key.
const CERTIFICATE: &str = r#"-----BEGIN CERTIFICATE-----
MIIDHDCCAgSgAwIBAgIUar3xOa7Fi2aYaCm9aZejGT/ZV+IwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDkwNzE1MjgzOVoXDTM2MDkw
NDE1MjgzOVowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEA7NdyS/6zUoxsSVRejTB2j8Nn6JgFmaFt9rm8oPQbXbjZ
JvzsDtouLlt5jZUorCrDNIdYXZyL1yrTcx8WPTJfzAzTtUHm4gOxhMrf48ERhc56
Gu6H9FZUeTNO2SsIqeqisyq+wqVeuMJdDtYMW2G/KxP4DxzdngR091yK6Cn++hqn
KR1IahJwMyYTUSwlzrh0m9B0L+Pep6aUc0teVVivmKespqtEJcMUCeHrb8btdEOY
6Raer69OMM7d/nmbloP14WDGHvHFM2R5sE1X0WUTbd0tV/xrW5nkS2i/2oBovqmz
Co5dKqSzz2bne+ToPw+zX/hzXIcLMc/hQa95sPo9twIDAQABo2YwZDAdBgNVHQ4E
FgQUqjOOSGIzD+VobuHiktSK9X0uJQswHwYDVR0jBBgwFoAUqjOOSGIzD+VobuHi
ktSK9X0uJQswFAYDVR0RBA0wC4IJbG9jYWxob3N0MAwGA1UdEwEB/wQCMAAwDQYJ
KoZIhvcNAQELBQADggEBAFZcXTEVu2eCl6yCSGVpHJoNkzkS6gugp/awSDFhksfQ
IvQF1KBSlAm4PD0cMkLZ+ipL6uoXh8bdt1n5x7tRyZUoJjckQsyGjaeoUPt49EOn
6SK9eWxEPNXTiK+JpdsyZEIMo2hhpDNWigceLTIh5IAPPKdmZO7YFgALoIrcyXXh
HMCO6qBnCXlJtNX3srv6UUGczwTTQIi9HE08YwbeAGWH76Gfag3wK++XSK3mgJhx
VncOawL7ZEmqm4LCXkw2Km1/PEC3FwD/lQSrgVsNUWmpj5f8w/EffqCHwfV5rIix
Y/JbV2tWES7pTT2Um+CU89uR1X7QTUTcZYXvr1u+vT4=
-----END CERTIFICATE-----
"#;
const PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDs13JL/rNSjGxJ
VF6NMHaPw2fomAWZoW32ubyg9BtduNkm/OwO2i4uW3mNlSisKsM0h1hdnIvXKtNz
HxY9Ml/MDNO1QebiA7GEyt/jwRGFznoa7of0VlR5M07ZKwip6qKzKr7CpV64wl0O
1gxbYb8rE/gPHN2eBHT3XIroKf76GqcpHUhqEnAzJhNRLCXOuHSb0HQv496nppRz
S15VWK+Yp6ymq0QlwxQJ4etvxu10Q5jpFp6vr04wzt3+eZuWg/XhYMYe8cUzZHmw
TVfRZRNt3S1X/GtbmeRLaL/agGi+qbMKjl0qpLPPZud75Og/D7Nf+HNchwsxz+FB
r3mw+j23AgMBAAECggEADIy7VmI1UlHKiHWGBRPk/xKAS7IyfxovX3wmuJ32t+1M
ObqmhNWezioJXi03LdhI5lxS8bLn3guQ5MVoQkSMZZ2QYkcPBY7jNdbM156qtQe5
2foluMFnpyccLc8P8yD8ZLixHv3wRPis7FT/QUsD/DO0rehTqJndMMugjf4bsAOQ
xPlh91pJGuJelWruaUozjBMliD41W6/E8o9ZInkvS872Mn+EFNU2spBw9qm2Z/xS
xRGfVJY/yQrlfqn95SViknIK17kDoFN+36+qZaroWr4VjNaGBtIQWlwkdpl2SqH+
TrqGRPHsk7aQpIhY2cB8WY3pcTN42KtZmT/JKm+yQQKBgQD7HACkfbsGZMwJwu5O
4j6tr2larSEtk/8VqFr9phCAM5cVQWu1+pBhStpS/f+oE90IEiJa1F5r9hvg6Q4j
eZFMIAtCovT/R+UYfZ8n/1vCD6MHr559Qehb2KnUyg919e3ULIXOqDrwN08CHa4O
CRZoZZwSjCOJq1wGd+NntgiqRwKBgQDxdE6Ax2oapPXq54ajDwdzmeKVKEDUOUQT
bymB65v+3ShtOBrAa0lwK47H76POqawZtMTb8n6cZXRvhvv3K01oKH6m4vVNREUz
dFk1LWZ/GXCgzEl4iwROYDr/5X5MqiTshpdM8GMQ+ehL89bRaA2VIsnZuoy8V5or
JX/QjQoZEQKBgQDei4i+P3fbSMXT+OB/JN/rykQSytFWtY0iwpwxpFWHaTGC8wHk
u/XtZAtt9hH4AfKoTnoICaLNB8bZY3LWWc09rECOhCGhhTQyqlK9fgDyUi1oiGps
FFc73x9UqOde4eAvZG4KIuppLntlIqy5X7BuQW86uNxeDHJ4gRQXPCsdzQKBgQCz
Ojk3gE6zXnWom5mmGfbXCYhWXZ3ZqnRs1JwD82dFBNcIU5gP8tN9bue6Y5i9Q9ca
8cMa3OK8ptaKHrGTpFH+GekBagDaDO4tJpU9Uuj9OV4QDfQPhWl54BaLcseQks97
vuA6XUm8BTU4g9SWdl12sW8RrlbfS0uF8Xzxym+PcQKBgFyk/uR1zWJt9N6Fd1tS
+SfK98dY6piMzw8M1MiJLRSZPGmQ80w+GADzX3oO4MUe4toeZg67fksoZZbktLij
mEqeU3JLxlmoplfkiBI9ctydoGKhaQJyOkH7YnoQbhvH91c+73mDDmE+guUr+ZkC
QL93rV0esr1cPToEJKhHapwR
-----END PRIVATE KEY-----
"#;

type HttpsFixture = (
    OauthClient,
    OauthRegistration,
    std::thread::JoinHandle<std::io::Result<Vec<String>>>,
);

fn https_server(
    responses: Vec<(u16, serde_json::Value)>,
) -> Result<HttpsFixture, Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from_pem_slice(CERTIFICATE.as_bytes())?],
        PrivateKeyDer::from_pem_slice(PRIVATE_KEY.as_bytes())?,
    )?;
    let config = Arc::new(config);
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (socket, _) = listener.accept()?;
            socket.set_read_timeout(Some(Duration::from_secs(10)))?;
            let connection =
                rustls::ServerConnection::new(config.clone()).map_err(std::io::Error::other)?;
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).map_err(std::io::Error::other)?;
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .ok_or_else(|| std::io::Error::other("missing request length"))?
                .parse()
                .map_err(std::io::Error::other)?;
            let mut body_bytes = vec![0; length];
            stream.read_exact(&mut body_bytes)?;
            requests.push(String::from_utf8(body_bytes).map_err(std::io::Error::other)?);
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nLocation: https://localhost:{port}/redirect\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()?;
        }
        Ok(requests)
    });
    let certificate = reqwest::Certificate::from_pem(CERTIFICATE.as_bytes())?;
    let client = OauthClient::from_builder(
        reqwest::Client::builder()
            .add_root_certificate(certificate)
            .no_proxy(),
    )
    .map_err(|_| "client construction failed")?;
    let registration = OauthRegistration {
        client_id: "local-client".into(),
        token_url: format!("https://localhost:{port}/token"),
        device_authorization_url: format!("https://localhost:{port}/device"),
        scopes: vec!["openid".into(), "offline_access".into()],
    };
    Ok((client, registration, server))
}

fn device_response() -> serde_json::Value {
    serde_json::json!({"device_code":"secret-device-code", "user_code":"OPERATOR-CODE", "verification_uri":"https://authorization.example/verify", "expires_in":30, "interval":0})
}

#[tokio::test]
async fn oauth_device_polling_preserves_scope_order_and_harvests_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"subject","https://api.openai.com/auth":{"chatgpt_account_id":"account","chatgpt_user_id":"user"}}"#);
    let identity_token = format!("header.{payload}.signature");
    let (client, registration, server) = https_server(vec![
        (200, device_response()),
        (400, serde_json::json!({"error":"authorization_pending"})),
        (
            200,
            serde_json::json!({"refresh_token":"synthetic-refresh", "id_token":identity_token}),
        ),
    ])?;
    let device = client
        .authorize(&registration)
        .await
        .map_err(|reason| format!("{reason:?}"))?;
    assert_eq!(device.progress.user_code, "OPERATOR-CODE");
    let authorization = client
        .poll(&registration, device)
        .await
        .map_err(|reason| format!("{reason:?}"))?;
    assert_eq!(authorization.refresh_token, "synthetic-refresh");
    assert_eq!(authorization.identity_token, identity_token);
    assert_eq!(
        authorization.account_identity,
        serde_json::json!({"subject":"subject", "account_id":"account", "user_id":"user"})
    );
    let requests = server.join().map_err(|_| "TLS server panicked")??;
    assert_eq!(
        requests,
        vec![
            "client_id=local-client&scope=openid+offline_access",
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=secret-device-code&client_id=local-client",
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=secret-device-code&client_id=local-client",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn oauth_provisioning_without_identity_token_fails_typed()
-> Result<(), Box<dyn std::error::Error>> {
    let (client, registration, server) = https_server(vec![
        (200, device_response()),
        (
            200,
            serde_json::json!({"refresh_token":"synthetic-refresh"}),
        ),
    ])?;
    let device = client
        .authorize(&registration)
        .await
        .map_err(|reason| format!("{reason:?}"))?;
    assert_eq!(
        client.poll(&registration, device).await.err(),
        Some(Failure::TokenResponseWithoutIdentity)
    );
    assert_eq!(server.join().map_err(|_| "TLS server panicked")??.len(), 2);
    Ok(())
}

#[tokio::test]
async fn oauth_device_redirect_is_rejected_without_following()
-> Result<(), Box<dyn std::error::Error>> {
    let (client, registration, server) = https_server(vec![(302, device_response())])?;
    assert_eq!(
        client.authorize(&registration).await.err(),
        Some(Failure::DeviceEndpointRejected)
    );
    assert_eq!(server.join().map_err(|_| "TLS server panicked")??.len(), 1);
    Ok(())
}
