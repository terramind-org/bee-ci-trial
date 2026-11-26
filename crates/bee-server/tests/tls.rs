//! F49-042: TLS on the main listener and optional mutual TLS whose client certificates map
//! to principals, with test certificates under `fixtures/tls` (an EC P-256 test CA, a server
//! certificate for `localhost`/127.0.0.1, client certificates for `ci-bot` and
//! `alice-laptop`, and a `ci-bot` certificate from a CA the server does not trust).

#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use common::*;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CI_PRINCIPAL: &str = "01M5TLSC1B0T0000000000CI00";
const LAPTOP_PRINCIPAL: &str = "01M5TLSA11CE0000000000AL00";

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn client_config(cert: Option<&str>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(fixture("ca.pem")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let b = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let cfg = match cert {
        None => b.with_no_client_auth(),
        Some(name) => {
            let chain: Vec<CertificateDer<'static>> =
                CertificateDer::pem_file_iter(fixture(&format!("{name}.pem")))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
            let key = PrivateKeyDer::from_pem_file(fixture(&format!("{name}.key.pem"))).unwrap();
            b.with_client_auth_cert(chain, key).unwrap()
        }
    };
    Arc::new(cfg)
}

/// One HTTP/1.1 request over TLS: status and body, or the I/O error (a refused handshake).
async fn request(
    addr: std::net::SocketAddr,
    cert: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> std::io::Result<(u16, String)> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let connector = tokio_rustls::TlsConnector::from(client_config(cert));
    let mut tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    tls.write_all(req.as_bytes()).await?;
    let mut out = Vec::new();
    tls.read_to_end(&mut out).await?;
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, rest) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other(format!("no response: {text:?}")))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("bad status line"))?;
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut body = String::new();
        let mut r = rest;
        while let Some((size, after)) = r.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            body.push_str(&after[..n]);
            r = &after[n + 2..];
        }
        body
    } else {
        rest.to_string()
    };
    Ok((status, body))
}

async fn whoami(
    addr: std::net::SocketAddr,
    cert: Option<&str>,
    bearer: Option<&str>,
) -> std::io::Result<(u16, Value)> {
    let auth = bearer.map(|b| format!("Bearer {b}"));
    let mut h = vec![("content-type", "application/json")];
    if let Some(a) = &auth {
        h.push(("authorization", a.as_str()));
    }
    let (s, b) = request(addr, cert, "POST", "/bee.v1.AuthService/WhoAmI", &h, "{}").await?;
    Ok((s, serde_json::from_str(&b).unwrap_or(Value::Null)))
}

fn tls_config(c: &mut bee_server::config::Config, require: bool) {
    c.tls.cert_file = fixture("server.pem");
    c.tls.key_file = fixture("server.key.pem");
    c.tls.client_ca_file = fixture("ca.pem");
    c.tls.require_client_cert = require;
    c.tls.client_principals = vec![
        bee_server::config::TlsPrincipal {
            subject: "ci-bot.swarm.test".into(),
            principal: CI_PRINCIPAL.into(),
        },
        bee_server::config::TlsPrincipal {
            subject: "alice-laptop".into(),
            principal: LAPTOP_PRINCIPAL.into(),
        },
    ];
}

async fn add_principal(t: &TestServer, id: &str, kind: &str, display: &str) {
    let rec = bee_server::auth::PrincipalRecord {
        id: id.into(),
        kind: kind.into(),
        display: display.into(),
        org: String::new(),
        owner: String::new(),
        runtime: String::new(),
        purpose: String::new(),
        external_ids: Vec::new(),
        groups: Vec::new(),
        deactivated: false,
    };
    t.srv
        .state
        .kv
        .put(
            bee_server::auth::NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&rec).unwrap(),
        )
        .await
        .unwrap();
}

/// The listener speaks TLS only and advertises an https URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_listener_serves_https_only() {
    let t = start_with(|c| {
        c.tls.cert_file = fixture("server.pem");
        c.tls.key_file = fixture("server.key.pem");
    })
    .await;
    assert!(t.srv.url.starts_with("https://"), "{}", t.srv.url);
    let (s, body) = request(t.srv.addr, None, "GET", "/healthz", &[], "")
        .await
        .expect("https");
    assert_eq!((s, body.trim()), (200, "ok"));
    let (s, who) = whoami(t.srv.addr, None, Some(&t.token)).await.unwrap();
    assert_eq!(s, 200);
    assert_eq!(who["principal"]["id"], t.dev_user().as_str());
    // Plain HTTP on the TLS port gets no HTTP answer.
    let mut tcp = tokio::net::TcpStream::connect(t.srv.addr).await.unwrap();
    tcp.write_all(b"GET /healthz HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tcp.read_to_end(&mut out).await;
    assert!(!String::from_utf8_lossy(&out).contains("200 OK"));
    // Half a TLS configuration is refused at startup.
    let mut cfg = test_config(t.dir.path());
    cfg.tls.cert_file = fixture("server.pem");
    assert!(cfg.validate().is_err());
}

/// Mutual TLS: without a trusted client certificate there is no connection; a mapped
/// certificate (by SAN DNS name or by subject CN) authenticates as its principal; an explicit
/// bearer token wins over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutual_tls_maps_client_certificates_to_principals() {
    let t = start_with(|c| tls_config(c, true)).await;
    add_principal(&t, CI_PRINCIPAL, "service", "ci-bot").await;
    add_principal(&t, LAPTOP_PRINCIPAL, "human", "alice").await;
    assert!(
        request(t.srv.addr, None, "GET", "/healthz", &[], "")
            .await
            .is_err(),
        "a client certificate is required"
    );
    assert!(
        request(t.srv.addr, Some("client-rogue"), "GET", "/healthz", &[], "")
            .await
            .is_err(),
        "a certificate from an untrusted CA is refused"
    );
    let (s, who) = whoami(t.srv.addr, Some("client-ci"), None).await.unwrap();
    assert_eq!(s, 200, "{who}");
    assert_eq!(who["principal"]["id"], CI_PRINCIPAL);
    assert_eq!(who["principal"]["kind"], "PRINCIPAL_KIND_SERVICE");
    let (_, who) = whoami(t.srv.addr, Some("client-cn"), None).await.unwrap();
    assert_eq!(who["principal"]["id"], LAPTOP_PRINCIPAL, "mapped by CN");
    let (_, who) = whoami(t.srv.addr, Some("client-ci"), Some(&t.token))
        .await
        .unwrap();
    assert_eq!(who["principal"]["id"], t.dev_user().as_str(), "bearer wins");
}

/// Optional client certificates: anonymous TLS clients connect but are not authenticated;
/// a deactivated principal's certificate authenticates nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn optional_client_certificates_authenticate_only_when_mapped() {
    let t = start_with(|c| tls_config(c, false)).await;
    add_principal(&t, LAPTOP_PRINCIPAL, "human", "alice").await;
    let (s, _) = request(t.srv.addr, None, "GET", "/healthz", &[], "")
        .await
        .unwrap();
    assert_eq!(s, 200);
    let (s, who) = whoami(t.srv.addr, None, None).await.unwrap();
    assert_ne!(s, 200, "{who}");
    // ci-bot's principal does not exist here: the certificate maps to nobody.
    let (s, _) = whoami(t.srv.addr, Some("client-ci"), None).await.unwrap();
    assert_ne!(s, 200);
    let (s, who) = whoami(t.srv.addr, Some("client-cn"), None).await.unwrap();
    assert_eq!(s, 200);
    assert_eq!(who["principal"]["id"], LAPTOP_PRINCIPAL);
}
