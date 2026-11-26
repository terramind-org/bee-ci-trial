//! TLS on the main listener, with optional mutual TLS (F49-042).
//!
//! With `tls.cert_file` and `tls.key_file` set the listener speaks TLS 1.2/1.3 only (rustls,
//! ALPN `h2` and `http/1.1`; gRPC, HTTP/JSON, git and MCP all ride it). With
//! `tls.client_ca_file` it asks every client for a certificate signed by those CAs:
//! `tls.require_client_cert` refuses handshakes without one, otherwise a certificate is
//! optional (an untrusted one still fails the handshake).
//!
//! **Certificate principals.** A verified client certificate whose subject common name or a
//! SAN DNS name equals a `[[tls.client_principals]]` `subject` authenticates as that
//! `principal`: at the handshake the server mints a token for it (the principal must exist and
//! be active) and adds it as `Authorization: Bearer …` to every request of that connection
//! that carries no credential of its own. An explicit bearer token always wins. The mapping
//! lives for the connection and is dropped when it closes.
//!
//! Handshakes run on their own tasks (a slow client never stalls the accept loop) and time
//! out after 10 s.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::config::TlsSection;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::state::AppState;

fn tls_error(what: impl std::fmt::Display) -> ApiError {
    ApiError::new(bee_proto::v1::ErrorReason::Internal, format!("tls: {what}"))
}

fn certs(path: &str) -> ApiResult<Vec<CertificateDer<'static>>> {
    let out: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .map_err(|e| tls_error(format!("{path}: {e}")))?
        .collect::<Result<_, _>>()
        .map_err(|e| tls_error(format!("{path}: {e}")))?;
    if out.is_empty() {
        return Err(tls_error(format!("{path} holds no certificate")));
    }
    Ok(out)
}

/// The rustls server configuration for `cfg`, `None` when TLS is off.
pub fn server_config(cfg: &TlsSection) -> ApiResult<Option<Arc<rustls::ServerConfig>>> {
    if cfg.cert_file.is_empty() {
        return Ok(None);
    }
    let chain = certs(&cfg.cert_file)?;
    let key = PrivateKeyDer::from_pem_file(&cfg.key_file)
        .map_err(|e| tls_error(format!("{}: {e}", cfg.key_file)))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?;
    let builder = if cfg.client_ca_file.is_empty() {
        builder.with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for c in certs(&cfg.client_ca_file)? {
            roots.add(c).map_err(tls_error)?;
        }
        let v = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
        let v = if cfg.require_client_cert {
            v
        } else {
            v.allow_unauthenticated()
        };
        builder.with_client_cert_verifier(v.build().map_err(tls_error)?)
    };
    let mut config = builder.with_single_cert(chain, key).map_err(tls_error)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(Arc::new(config)))
}

// ----- certificate names ----------------------------------------------------------------------

/// One DER TLV: (tag, contents, rest).
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)? as usize;
    let (len, hdr) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let body = b.get(hdr..hdr + len)?;
    Some((tag, body, &b[hdr + len..]))
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];

/// The subject common names and SAN DNS names of a DER certificate.
pub fn cert_names(der: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some((0x30, cert, _)) = tlv(der) else {
        return out;
    };
    let Some((0x30, tbs, _)) = tlv(cert) else {
        return out;
    };
    // TBSCertificate: [0] version?, serial, signature, issuer, validity, subject, spki, ...
    let mut rest = tbs;
    let mut fields = Vec::new();
    while let Some((tag, body, r)) = tlv(rest) {
        fields.push((tag, body));
        rest = r;
    }
    let skip = usize::from(fields.first().is_some_and(|(t, _)| *t == 0xa0));
    if let Some((0x30, subject)) = fields.get(skip + 4).copied() {
        let mut rdns = subject;
        while let Some((_, set, r)) = tlv(rdns) {
            let mut atvs = set;
            while let Some((_, atv, r2)) = tlv(atvs) {
                if let Some((0x06, oid, value)) = tlv(atv)
                    && oid == OID_CN
                    && let Some((_, v, _)) = tlv(value)
                {
                    out.push(String::from_utf8_lossy(v).into_owned());
                }
                atvs = r2;
            }
            rdns = r;
        }
    }
    // Extensions: [3] { SEQUENCE OF Extension }.
    if let Some((_, ext)) = fields.iter().find(|(t, _)| *t == 0xa3).copied()
        && let Some((0x30, mut exts, _)) = tlv(ext)
    {
        while let Some((_, e, r)) = tlv(exts) {
            if let Some((0x06, oid, mut rest)) = tlv(e)
                && oid == OID_SAN
            {
                if let Some((0x01, _, r2)) = tlv(rest) {
                    rest = r2; // critical flag
                }
                if let Some((0x04, octets, _)) = tlv(rest)
                    && let Some((0x30, mut names, _)) = tlv(octets)
                {
                    while let Some((tag, name, r3)) = tlv(names) {
                        if tag == 0x82 {
                            out.push(String::from_utf8_lossy(name).into_owned());
                        }
                        names = r3;
                    }
                }
            }
            exts = r;
        }
    }
    out
}

// ----- listener -------------------------------------------------------------------------------

/// Bearer tokens of connections authenticated by a mapped client certificate.
#[derive(Default)]
pub struct PeerTokens(Mutex<HashMap<SocketAddr, String>>);

impl PeerTokens {
    /// The token of the connection from `addr`, if its certificate maps to a principal.
    pub fn get(&self, addr: &SocketAddr) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(addr)
            .cloned()
    }
}

/// A TLS connection; dropping it forgets its certificate principal.
pub struct TlsConn {
    inner: TlsStream<TcpStream>,
    addr: SocketAddr,
    tokens: Arc<PeerTokens>,
}

impl Drop for TlsConn {
    fn drop(&mut self) {
        self.tokens
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.addr);
    }
}

impl AsyncRead for TlsConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// An accept loop that hands out finished TLS handshakes.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsConn, SocketAddr)>,
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsConn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The acceptor task only ends with the process; wait forever like a quiet socket.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The principal a verified client certificate maps to.
fn principal_of(cfg: &TlsSection, names: &[String]) -> Option<String> {
    cfg.client_principals
        .iter()
        .find(|m| names.iter().any(|n| n.eq_ignore_ascii_case(&m.subject)))
        .map(|m| m.principal.clone())
}

async fn mint_for(state: &AppState, principal: &str) -> Option<String> {
    let rec = state
        .principal(principal)
        .await
        .ok()
        .flatten()
        .filter(|r| !r.deactivated)?;
    let now = state.now_ms() / 1000;
    state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &bee_auth::MintRequest {
                principal: rec.to_auth(),
                on_behalf_of: Vec::new(),
                scope: bee_auth::Scope::new()
                    .expires_at(now + state.cfg.auth.token_lifetime_secs)
                    .session("mtls".to_string()),
            },
            now,
        )
        .ok()
        .map(|t| t.text)
}

/// Serves TLS handshakes on `listener` with `config`.
pub fn listen(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    state: Arc<AppState>,
    tokens: Arc<PeerTokens>,
) -> io::Result<TlsListener> {
    let local = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(config);
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        loop {
            let (tcp, addr) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let (acceptor, tx, state, tokens) =
                (acceptor.clone(), tx.clone(), state.clone(), tokens.clone());
            tokio::spawn(async move {
                let tls = match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp))
                    .await
                {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        logging::log(
                            Level::Debug,
                            "tls",
                            "handshake failed",
                            &[
                                ("peer", addr.to_string().into()),
                                ("error", e.to_string().into()),
                            ],
                        );
                        return;
                    }
                    Err(_) => return,
                };
                let names = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|c| c.first())
                    .map(|leaf| cert_names(leaf.as_ref()))
                    .unwrap_or_default();
                if let Some(p) = principal_of(&state.cfg.tls, &names)
                    && let Some(token) = mint_for(&state, &p).await
                {
                    tokens
                        .0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(addr, token);
                }
                let conn = TlsConn {
                    inner: tls,
                    addr,
                    tokens,
                };
                let _ = tx.send((conn, addr)).await;
            });
        }
    });
    Ok(TlsListener { rx, local })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_walker_survives_garbage() {
        assert!(cert_names(&[]).is_empty());
        assert!(cert_names(&[0x30, 0x82, 0xff]).is_empty());
        assert!(cert_names(&[0x30, 0x03, 0x30, 0x01, 0x00]).is_empty());
    }

    #[test]
    fn principals_match_case_insensitively() {
        let cfg = TlsSection {
            client_principals: vec![crate::config::TlsPrincipal {
                subject: "ci-bot.bee.test".into(),
                principal: "01CI".into(),
            }],
            ..TlsSection::default()
        };
        assert_eq!(
            principal_of(&cfg, &["x".into(), "CI-BOT.bee.test".into()]).as_deref(),
            Some("01CI")
        );
        assert!(principal_of(&cfg, &["other".into()]).is_none());
    }
}
