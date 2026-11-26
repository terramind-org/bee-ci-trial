//! Git over SSH: `git-upload-pack` and `git-receive-pack` as exec requests on an SSH session,
//! served by the same engine as smart HTTP.
//!
//! - **Authentication** is public-key only. The server asks the backend which principal the key
//!   belongs to ([`GitBackend::ssh_principal`]); the SSH user name is ignored (clients
//!   conventionally use `git`). A key nobody registered never gets past authentication.
//! - **Commands** are `git-upload-pack '<owner>/<repo>[.git]'` and `git-receive-pack '…'` (the
//!   `git upload-pack` spelling and a leading `/` are accepted). A session with no command (a
//!   shell, as `ssh -T git@host` asks for) is told who it authenticated as and gets exit
//!   status 1; any other command is refused.
//! - **Protocol versions** follow the `GIT_PROTOCOL` environment variable the client sends
//!   (`version=2`, `version=1`; absent: v0). Over SSH the session is a stream, not a series
//!   of stateless requests, so this module frames it: v2 commands are read to their flush
//!   packet; v0/v1 fetch negotiation replays the engine's stateless exchange with the wants
//!   and every have so far (acknowledgments already sent are not repeated); a push reads its
//!   commands, options and exactly the pack that follows.
//! - **Access** is the HTTP gateway's: the backend opens the repo for the principal with the
//!   same gate, so policy, Land for protected refs, push protection and audit are identical.
//!   Refusals before the protocol starts are sent as an `ERR` packet (which git prints as
//!   `remote: …`) and exit status 1.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bee_git::ObjectFormat;
use flate2::{Decompress, FlushDecompress, Status};
use russh::keys::ssh_key::{self, HashAlg};
pub use russh::keys::ssh_key::{PrivateKey, PublicKey};
use russh::server::{Auth, Handle, Handler, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::advertise::{self, RefList};
use crate::backend::{Access, RepoSession};
use crate::cache::CacheUse;
use crate::error::{GitError, GitResult};
use crate::gitmap::GitView;
use crate::http::GitGateway;
use crate::pktline;
use crate::receive;
use crate::upload;

/// Who an SSH key authenticates as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshIdentity {
    /// The principal's id (what the repo gate is opened for).
    pub principal: String,
    /// The name shown in the `ssh -T` greeting.
    pub name: String,
}

/// SSH listener settings.
#[derive(Clone, Debug)]
pub struct SshConfig {
    /// The server's host keys (at least one).
    pub host_keys: Vec<PrivateKey>,
    /// Close connections idle for this long.
    pub idle_timeout: Duration,
}

impl SshConfig {
    /// Settings with one host key and a ten-minute idle timeout.
    pub fn new(host_key: PrivateKey) -> SshConfig {
        SshConfig {
            host_keys: vec![host_key],
            idle_timeout: Duration::from_secs(600),
        }
    }
}

/// A running SSH listener.
pub struct SshServer {
    /// The bound address.
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl SshServer {
    /// Stops accepting connections and ends the open ones.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

impl Drop for SshServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A new Ed25519 host key.
pub fn generate_host_key() -> GitResult<PrivateKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| GitError::internal(format!("cannot generate an SSH host key: {e}")))?;
    Ok(PrivateKey::from(
        ssh_key::private::Ed25519Keypair::from_seed(&seed),
    ))
}

/// The OpenSSH encoding of a private key.
pub fn host_key_to_openssh(key: &PrivateKey) -> GitResult<String> {
    key.to_openssh(ssh_key::LineEnding::LF)
        .map(|z| z.to_string())
        .map_err(|e| GitError::internal(format!("cannot encode the SSH host key: {e}")))
}

/// Reads a private key in OpenSSH format.
pub fn host_key_from_openssh(text: &str) -> GitResult<PrivateKey> {
    PrivateKey::from_openssh(text)
        .map_err(|e| GitError::invalid(format!("not an OpenSSH private key: {e}")))
}

/// A public key line (`ssh-ed25519 AAAA… comment`) parsed.
pub fn parse_public_key(line: &str) -> GitResult<PublicKey> {
    PublicKey::from_openssh(line.trim())
        .map_err(|e| GitError::invalid(format!("not an OpenSSH public key: {e}")))
}

/// The `SHA256:…` fingerprint of a public key (what `ssh-keygen -l` prints).
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

impl GitGateway {
    /// Serves git over SSH on `listener`. Every connection authenticates by public key through
    /// the backend and runs `git-upload-pack` / `git-receive-pack` over this gateway.
    pub async fn serve_ssh(
        self: &Arc<Self>,
        listener: TcpListener,
        cfg: SshConfig,
    ) -> io::Result<SshServer> {
        let addr = listener.local_addr()?;
        let config = Arc::new(russh::server::Config {
            keys: cfg.host_keys,
            inactivity_timeout: Some(cfg.idle_timeout),
            // Unknown keys are refused at the offer, so a short constant delay is enough.
            auth_rejection_time: Duration::from_millis(100),
            auth_rejection_time_initial: Some(Duration::ZERO),
            max_auth_attempts: 20,
            ..russh::server::Config::default()
        });
        let mut server = Listener { gw: self.clone() };
        let task = tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        Ok(SshServer { addr, task })
    }
}

struct Listener {
    gw: Arc<GitGateway>,
}

impl russh::server::Server for Listener {
    type Handler = Conn;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Conn {
        Conn {
            gw: self.gw.clone(),
            identity: None,
            chans: HashMap::new(),
        }
    }
}

/// What the connection knows about one session channel.
#[derive(Default)]
struct Chan {
    git_protocol: Option<String>,
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

struct Conn {
    gw: Arc<GitGateway>,
    identity: Option<SshIdentity>,
    chans: HashMap<ChannelId, Chan>,
}

impl Conn {
    async fn lookup(&mut self, key: &PublicKey) -> Result<Auth, russh::Error> {
        let fp = fingerprint(key);
        let line = key.to_openssh().unwrap_or_default();
        match self.gw.backend.ssh_principal_for_key(&fp, &line).await {
            Ok(Some(id)) => {
                self.identity = Some(id);
                Ok(Auth::Accept)
            }
            _ => Ok(Auth::reject()),
        }
    }
}

impl Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.lookup(key).await
    }

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.lookup(key).await
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.chans.insert(channel.id(), Chan::default());
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // No terminal: the client carries on and asks for the shell without one.
        session.channel_failure(channel)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL"
            && value.len() <= 256
            && let Some(c) = self.chans.get_mut(&channel)
        {
            c.git_protocol = Some(value.to_string());
            return session.channel_success(channel);
        }
        session.channel_failure(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let name = self.identity.as_ref().map_or("", |i| i.name.as_str());
        let msg = format!(
            "Hi {name}! You've successfully authenticated, but Beehive does not provide shell access.\n"
        );
        tokio::spawn(finish(session.handle(), channel, Some(msg), 1));
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        let (Some(ident), Some(chan)) = (self.identity.clone(), self.chans.get_mut(&channel))
        else {
            return session.channel_failure(channel);
        };
        session.channel_success(channel)?;
        let handle = session.handle();
        let Some(exec) = parse_exec(&cmd) else {
            let msg =
                format!("Beehive serves only git-upload-pack and git-receive-pack, not {cmd:?}\n");
            tokio::spawn(finish(handle, channel, Some(msg), 1));
            return Ok(());
        };
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        chan.stdin = Some(stdin_tx);
        let proto = chan.git_protocol.clone().unwrap_or_default();
        let gw = self.gw.clone();
        tokio::spawn(async move {
            let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
            let writer = {
                let handle = handle.clone();
                tokio::spawn(async move {
                    while let Some(bytes) = out_rx.recv().await {
                        for chunk in bytes.chunks(16 * 1024) {
                            if handle.data(channel, chunk.to_vec()).await.is_err() {
                                return;
                            }
                        }
                    }
                })
            };
            let mut input = Input::new(stdin_rx, gw.cfg.max_request_bytes);
            let started = std::time::Instant::now();
            let r = serve_exec(&gw, &ident, &exec, &proto, &mut input, &out_tx).await;
            let service = match exec.service {
                Service::UploadPack => "upload-pack",
                Service::ReceivePack => "receive-pack",
                Service::LfsAuthenticate { .. } => "lfs-authenticate",
            };
            gw.metrics
                .request("ssh", service, r.is_ok(), started.elapsed().as_secs_f64());
            let code = match r {
                Ok(()) => 0,
                Err(e) => {
                    let mut p = Vec::new();
                    pktline::text(&mut p, &format!("ERR {}\n", e.line()));
                    let _ = out_tx.send(p).await;
                    1
                }
            };
            drop(out_tx);
            let _ = writer.await;
            finish(handle, channel, None, code).await;
        });
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&channel).and_then(|c| c.stdin.as_ref()) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(c) = self.chans.get_mut(&channel) {
            c.stdin = None;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.chans.remove(&channel);
        Ok(())
    }
}

/// Sends an optional message on stderr, then the exit status, EOF and close.
async fn finish(handle: Handle, channel: ChannelId, stderr: Option<String>, code: u32) {
    if let Some(m) = stderr {
        let _ = handle.extended_data(channel, 1, m.into_bytes()).await;
    }
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// Which service an exec request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// `git-upload-pack`: clone and fetch.
    UploadPack,
    /// `git-receive-pack`: push.
    ReceivePack,
    /// `git-lfs-authenticate <repo> <download|upload>`: how git-lfs gets its HTTP credential
    /// for an SSH remote.
    LfsAuthenticate {
        /// Whether the LFS operation is an upload.
        upload: bool,
    },
}

/// A parsed exec request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    /// The service.
    pub service: Service,
    /// The owner (org) segment of the path.
    pub owner: String,
    /// The repo segment, without `.git`.
    pub repo: String,
}

/// Parses `git-upload-pack '<owner>/<repo>.git'` and the other accepted spellings.
pub fn parse_exec(cmd: &str) -> Option<ExecRequest> {
    let cmd = cmd.trim();
    let (head, arg) = cmd.split_once(' ')?;
    let (service, arg) = match head {
        "git-upload-pack" => (Service::UploadPack, arg),
        "git-receive-pack" => (Service::ReceivePack, arg),
        "git-lfs-authenticate" => {
            let (path, op) = arg.trim().rsplit_once(' ')?;
            let upload = match op.trim() {
                "upload" => true,
                "download" => false,
                _ => return None,
            };
            (Service::LfsAuthenticate { upload }, path)
        }
        "git" => {
            let (sub, rest) = arg.trim_start().split_once(' ')?;
            match sub {
                "upload-pack" => (Service::UploadPack, rest),
                "receive-pack" => (Service::ReceivePack, rest),
                _ => return None,
            }
        }
        _ => return None,
    };
    let arg = arg.trim();
    let path = if let Some(p) = arg.strip_prefix('\'').and_then(|a| a.strip_suffix('\'')) {
        p
    } else if let Some(p) = arg.strip_prefix('"').and_then(|a| a.strip_suffix('"')) {
        p
    } else {
        arg
    };
    if path
        .chars()
        .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c.is_control())
    {
        return None;
    }
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(ExecRequest {
        service,
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn protocol_flags(env: &str) -> (bool, bool) {
    let has = |v: &str| env.split(':').any(|p| p.trim() == v);
    (has("version=2"), has("version=1"))
}

// ----- input framing --------------------------------------------------------------------

/// One packet as it came off the wire.
struct RawPkt {
    kind: PktKind,
    /// The packet with its four-digit length.
    raw: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PktKind {
    Flush,
    Delim,
    End,
    Data,
}

impl RawPkt {
    /// The data payload without one trailing newline (empty for control packets).
    fn line(&self) -> &[u8] {
        match self.kind {
            PktKind::Data => {
                let p = &self.raw[4..];
                p.strip_suffix(b"\n").unwrap_or(p)
            }
            _ => &[],
        }
    }
}

/// The client's side of the SSH channel as a byte stream with packet and pack framing.
struct Input {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    total: usize,
    limit: usize,
}

impl Input {
    fn new(rx: mpsc::UnboundedReceiver<Vec<u8>>, limit: usize) -> Input {
        Input {
            rx,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            total: 0,
            limit,
        }
    }

    /// Waits for one more chunk; false at the end of the stream.
    async fn more(&mut self) -> GitResult<bool> {
        if self.eof {
            return Ok(false);
        }
        match self.rx.recv().await {
            Some(chunk) => {
                self.total += chunk.len();
                if self.total > self.limit {
                    return Err(GitError::new(
                        bee_core::Reason::TooLarge,
                        "the session sent more than the request limit",
                    ));
                }
                if self.pos > (1 << 20) && self.pos == self.buf.len() {
                    self.buf.clear();
                    self.pos = 0;
                }
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }

    async fn need(&mut self, n: usize) -> GitResult<bool> {
        while self.buf.len() - self.pos < n {
            if !self.more().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The next packet; `None` at a clean end of stream.
    async fn pkt(&mut self) -> GitResult<Option<RawPkt>> {
        if !self.need(4).await? {
            return if self.buf.len() == self.pos {
                Ok(None)
            } else {
                Err(GitError::invalid("the session ended inside a packet"))
            };
        }
        let head = &self.buf[self.pos..self.pos + 4];
        let len = std::str::from_utf8(head)
            .ok()
            .and_then(|t| usize::from_str_radix(t, 16).ok())
            .ok_or_else(|| GitError::invalid("bad pkt-line length"))?;
        let kind = match len {
            0 => PktKind::Flush,
            1 => PktKind::Delim,
            2 => PktKind::End,
            3 => return Err(GitError::invalid("pkt-line length 3 is reserved")),
            _ => PktKind::Data,
        };
        let size = if kind == PktKind::Data { len } else { 4 };
        if !self.need(size).await? {
            return Err(GitError::invalid("the session ended inside a packet"));
        }
        let raw = self.buf[self.pos..self.pos + size].to_vec();
        self.pos += size;
        Ok(Some(RawPkt { kind, raw }))
    }

    /// Packets up to and including the next flush, as raw bytes; `None` at a clean end.
    async fn section(&mut self) -> GitResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        loop {
            match self.pkt().await? {
                None if out.is_empty() => return Ok(None),
                None => return Err(GitError::invalid("the session ended before a flush packet")),
                Some(p) => {
                    out.extend_from_slice(&p.raw);
                    if p.kind == PktKind::Flush {
                        return Ok(Some(out));
                    }
                }
            }
        }
    }

    /// The pack that follows a push's commands: exactly its bytes, found by walking its
    /// entries as they arrive.
    async fn pack(&mut self, format: ObjectFormat) -> GitResult<Vec<u8>> {
        let mut scan = PackScanner::new(format);
        loop {
            let start = self.pos;
            if let Some(len) = scan.advance(&self.buf[start..])? {
                let out = self.buf[start..start + len].to_vec();
                self.pos += len;
                return Ok(out);
            }
            if !self.more().await? {
                return Err(GitError::invalid("the session ended inside the pack"));
            }
        }
    }
}

/// Finds the end of a pack in a growing buffer without keeping the inflated objects: header,
/// then each entry's header and zlib stream (resumable), then the trailing checksum.
/// Verification of contents is left to `Pack::parse`.
struct PackScanner {
    hlen: usize,
    state: Scan,
}

enum Scan {
    Header,
    Entry {
        remaining: u32,
        pos: usize,
    },
    Inflating {
        remaining: u32,
        start: usize,
        size: u64,
        d: Box<Decompress>,
    },
    Trailer {
        pos: usize,
    },
}

impl PackScanner {
    fn new(format: ObjectFormat) -> PackScanner {
        PackScanner {
            hlen: format.oid_len(),
            state: Scan::Header,
        }
    }

    /// Advances over `data` (the pack so far, from its first byte). `Some(len)` when the pack
    /// is complete and is `len` bytes long, `None` when more bytes are needed.
    fn advance(&mut self, data: &[u8]) -> GitResult<Option<usize>> {
        let bad = |why: &str| GitError::invalid(format!("malformed pack: {why}"));
        loop {
            match &mut self.state {
                Scan::Header => {
                    if data.len() < 12 {
                        return Ok(None);
                    }
                    if &data[..4] != b"PACK" {
                        return Err(bad("bad signature"));
                    }
                    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
                    self.state = if count == 0 {
                        Scan::Trailer { pos: 12 }
                    } else {
                        Scan::Entry {
                            remaining: count,
                            pos: 12,
                        }
                    };
                }
                Scan::Entry { remaining, pos } => {
                    let (remaining, mut p) = (*remaining, *pos);
                    let Some(&first) = data.get(p) else {
                        return Ok(None);
                    };
                    p += 1;
                    let code = (first >> 4) & 7;
                    let mut size = u64::from(first & 0x0f);
                    let mut shift = 4u32;
                    let mut c = first;
                    while c & 0x80 != 0 {
                        let Some(&b) = data.get(p) else {
                            return Ok(None);
                        };
                        p += 1;
                        c = b;
                        if shift > 60 {
                            return Err(bad("entry size overflows"));
                        }
                        size |= u64::from(c & 0x7f) << shift;
                        shift += 7;
                    }
                    match code {
                        1..=4 => {}
                        6 => loop {
                            let Some(&b) = data.get(p) else {
                                return Ok(None);
                            };
                            p += 1;
                            if b & 0x80 == 0 {
                                break;
                            }
                        },
                        7 => {
                            if data.len() < p + self.hlen {
                                return Ok(None);
                            }
                            p += self.hlen;
                        }
                        _ => return Err(bad("reserved entry type")),
                    }
                    self.state = Scan::Inflating {
                        remaining,
                        start: p,
                        size,
                        d: Box::new(Decompress::new(true)),
                    };
                }
                Scan::Inflating {
                    remaining,
                    start,
                    size,
                    d,
                } => {
                    let mut scratch = [0u8; 16 * 1024];
                    loop {
                        let consumed = d.total_in() as usize;
                        let input = &data[(*start + consumed).min(data.len())..];
                        if input.is_empty() {
                            return Ok(None);
                        }
                        let (in0, out0) = (d.total_in(), d.total_out());
                        let status = d
                            .decompress(input, &mut scratch, FlushDecompress::None)
                            .map_err(|e| bad(&format!("zlib: {e}")))?;
                        if d.total_out() > *size {
                            return Err(bad("entry inflates past its declared size"));
                        }
                        match status {
                            Status::StreamEnd => break,
                            _ if d.total_in() == in0 && d.total_out() == out0 => {
                                return Err(bad("zlib stream makes no progress"));
                            }
                            _ => {}
                        }
                    }
                    let end = *start + d.total_in() as usize;
                    let remaining = *remaining - 1;
                    self.state = if remaining == 0 {
                        Scan::Trailer { pos: end }
                    } else {
                        Scan::Entry {
                            remaining,
                            pos: end,
                        }
                    };
                }
                Scan::Trailer { pos } => {
                    let end = *pos + self.hlen;
                    return Ok((data.len() >= end).then_some(end));
                }
            }
        }
    }
}

// ----- the exec session -----------------------------------------------------------------

async fn send(out: &mpsc::Sender<Vec<u8>>, bytes: Vec<u8>) -> GitResult<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    out.send(bytes)
        .await
        .map_err(|_| GitError::internal("the SSH channel closed"))
}

/// Runs one exec request to its end.
async fn serve_exec(
    gw: &Arc<GitGateway>,
    ident: &SshIdentity,
    exec: &ExecRequest,
    git_protocol: &str,
    input: &mut Input,
    out: &mpsc::Sender<Vec<u8>>,
) -> GitResult<()> {
    let access = match exec.service {
        Service::UploadPack | Service::LfsAuthenticate { upload: false } => Access::Read,
        Service::ReceivePack | Service::LfsAuthenticate { upload: true } => Access::Write,
    };
    let session = gw
        .backend
        .open_as(&ident.principal, &exec.owner, &exec.repo, access)
        .await?;
    let (v2, v1) = protocol_flags(git_protocol);
    match exec.service {
        Service::UploadPack => upload_session(gw, session.as_ref(), v2, v1, input, out).await,
        Service::ReceivePack => receive_session(gw, session.as_ref(), v1, input, out).await,
        Service::LfsAuthenticate { .. } => {
            // Opening the repo above checked access; hand git-lfs a credential for its HTTP
            // endpoint that is good for this repo for an hour.
            let c = gw
                .backend
                .ssh_lfs_credential(&ident.principal, &exec.owner, &exec.repo)
                .await?;
            let body = serde_json::json!({
                "href": c.href,
                "header": { "Authorization": format!("Bearer {}", c.token) },
                "expires_in": c.expires_in,
            });
            send(out, body.to_string().into_bytes()).await
        }
    }
}

async fn upload_session(
    gw: &GitGateway,
    session: &dyn RepoSession,
    v2: bool,
    v1: bool,
    input: &mut Input,
    out: &mpsc::Sender<Vec<u8>>,
) -> GitResult<()> {
    let format = advertise::repo_format(session).await?;
    let used = CacheUse::new(&gw.cache);
    gw.metrics.fetch_protocol(
        "ssh",
        if v2 {
            2
        } else if v1 {
            1
        } else {
            0
        },
    );
    if v2 {
        send(out, upload::advertise_v2(format)).await?;
        // Each command is read to its flush; a bare flush (or the end of the stream) ends it.
        while let Some(req) = input.section().await? {
            if req == b"0000" {
                break;
            }
            let mut view = GitView::new(session.history(), format);
            let refs = advertise::list(session, &mut view).await?;
            let resp = upload::serve_v2(session, &mut view, &refs, &used, &req).await?;
            gw.metrics.upload_bytes("ssh", resp.len() as u64);
            send(out, resp).await?;
        }
        gw.metrics.pack_cache(used.outcome());
        return Ok(());
    }
    let mut view = GitView::new(session.history(), format);
    let refs = advertise::list(session, &mut view).await?;
    send(out, upload::advertise_refs(&refs, v1, true)).await?;
    // The wants, shallow and filter lines up to the first flush.
    let Some(wants) = input.section().await? else {
        return Ok(());
    };
    if wants == b"0000" {
        return Ok(());
    }
    let deepen = has_deepen(&wants);
    let first = upload::serve_v0(session, &mut view, &refs, &used, &wants).await?;
    gw.metrics.upload_bytes("ssh", first.len() as u64);
    send(out, first).await?;
    let mut haves: Vec<u8> = Vec::new();
    let mut acked: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut shallow_sent = deepen;
    loop {
        let Some(p) = input.pkt().await? else {
            return Ok(());
        };
        let done = p.kind == PktKind::Data && p.line() == b"done";
        match p.kind {
            PktKind::Data if !done => haves.extend_from_slice(&p.raw),
            PktKind::Flush | PktKind::Data => {
                let mut req = wants.clone();
                req.extend_from_slice(&haves);
                if done {
                    req.extend_from_slice(&p.raw);
                } else {
                    pktline::flush(&mut req);
                }
                let resp = upload::serve_v0(session, &mut view, &refs, &used, &req).await?;
                let resp = trim_replayed(resp, shallow_sent, &mut acked);
                shallow_sent = false;
                gw.metrics.upload_bytes("ssh", resp.len() as u64);
                send(out, resp).await?;
                if done {
                    gw.metrics.pack_cache(used.outcome());
                    return Ok(());
                }
            }
            PktKind::Delim | PktKind::End => {
                return Err(GitError::invalid("unexpected packet during negotiation"));
            }
        }
    }
}

/// Whether the wants section carries a deepen request (a shallow-info block answers it).
fn has_deepen(wants: &[u8]) -> bool {
    let mut r = pktline::Reader::new(wants);
    while let Ok(Some(p)) = r.next_pkt() {
        if let Some(l) = p.line()
            && l.starts_with(b"deepen")
        {
            return true;
        }
    }
    false
}

/// Drops from a replayed stateless response what the client was already sent: the shallow
/// block (when `shallow` is set) and `ACK <id> common` lines for ids already acknowledged.
fn trim_replayed(resp: Vec<u8>, shallow: bool, acked: &mut BTreeSet<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::with_capacity(resp.len());
    let mut pos = 0usize;
    let mut skipping_shallow = shallow;
    while resp.len() - pos >= 4 {
        let Some(len) = std::str::from_utf8(&resp[pos..pos + 4])
            .ok()
            .and_then(|t| usize::from_str_radix(t, 16).ok())
        else {
            break;
        };
        if len < 4 {
            // A flush ends the shallow block.
            if len == 0 && skipping_shallow {
                skipping_shallow = false;
                pos += 4;
                continue;
            }
            break;
        }
        let Some(pkt) = resp.get(pos..pos + len) else {
            break;
        };
        let line = &pkt[4..];
        if skipping_shallow {
            pos += len;
            continue;
        }
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some(rest) = text.strip_prefix(b"ACK ")
            && let Some(id) = rest.strip_suffix(b" common")
        {
            pos += len;
            if acked.insert(id.to_vec()) {
                out.extend_from_slice(pkt);
            }
            continue;
        }
        if text.starts_with(b"ACK ") || text.starts_with(b"NAK") {
            out.extend_from_slice(pkt);
            pos += len;
            continue;
        }
        break;
    }
    out.extend_from_slice(&resp[pos..]);
    out
}

async fn receive_session(
    gw: &GitGateway,
    session: &dyn RepoSession,
    v1: bool,
    input: &mut Input,
    out: &mpsc::Sender<Vec<u8>>,
) -> GitResult<()> {
    let format = advertise::repo_format(session).await?;
    let mut view = GitView::new(session.history(), format);
    let refs: RefList = advertise::list(session, &mut view).await?;
    send(out, upload::advertise_refs(&refs, v1, false)).await?;
    let Some(mut raw) = input.section().await? else {
        return Ok(());
    };
    if raw == b"0000" {
        return Ok(());
    }
    // The first section is the commands; with `push-options` a second section follows, and
    // unless every command deletes, the pack.
    let probe = receive::parse(format, &raw)?;
    if probe.has("push-options") {
        let Some(opts) = input.section().await? else {
            return Err(GitError::invalid(
                "the session ended before the push options",
            ));
        };
        raw.extend_from_slice(&opts);
    }
    if probe.commands.iter().any(|c| !c.new.is_null()) {
        let pack = input.pack(format).await?;
        raw.extend_from_slice(&pack);
    }
    gw.metrics.receive_bytes("ssh", raw.len() as u64);
    let req = receive::parse(format, &raw)?;
    let (unpack, outcomes) =
        receive::execute(session, &mut view, &refs, &req, gw.backend.now_ms()).await?;
    view.flush().await?;
    let ok = outcomes
        .iter()
        .filter(|o| matches!(o, receive::Outcome::Ok { .. }))
        .count() as u64;
    gw.metrics.ref_updates(ok, outcomes.len() as u64 - ok);
    send(out, receive::report(&req, &unpack, &outcomes)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_git::{ObjectType, PackObject, RawObject, write_pack};

    fn exec(s: &str) -> Option<(Service, String, String)> {
        parse_exec(s).map(|e| (e.service, e.owner, e.repo))
    }

    #[test]
    fn exec_commands() {
        let want = Some((Service::UploadPack, "dev".to_string(), "r".to_string()));
        assert_eq!(exec("git-upload-pack '/dev/r.git'"), want);
        assert_eq!(exec("git-upload-pack 'dev/r.git'"), want);
        assert_eq!(exec("git-upload-pack /dev/r"), want);
        assert_eq!(exec("git upload-pack \"dev/r\""), want);
        assert_eq!(
            exec("git-receive-pack '/dev/r.git/'"),
            Some((Service::ReceivePack, "dev".to_string(), "r".to_string()))
        );
        assert_eq!(
            exec("git-lfs-authenticate 'dev/r.git' upload"),
            Some((
                Service::LfsAuthenticate { upload: true },
                "dev".to_string(),
                "r".to_string()
            ))
        );
        for bad in [
            "git-lfs-authenticate 'dev/r.git' sideways",
            "git-lfs-authenticate 'dev/r.git'",
            "ls",
            "git-upload-pack",
            "git-upload-pack 'dev'",
            "git-upload-pack 'a/b/c'",
            "git-upload-pack '/'",
            "git-upload-pack 'dev/r' ; rm -rf /",
            "git-upload-pack 'dev/r.git'x'",
        ] {
            assert!(exec(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn protocol_environment() {
        assert_eq!(protocol_flags("version=2"), (true, false));
        assert_eq!(
            protocol_flags("object-format=sha1:version=1"),
            (false, true)
        );
        assert_eq!(protocol_flags(""), (false, false));
    }

    #[test]
    fn replayed_responses_are_trimmed() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let mut resp = Vec::new();
        pktline::text(&mut resp, &format!("shallow {a}\n"));
        pktline::flush(&mut resp);
        pktline::text(&mut resp, &format!("ACK {a} common\n"));
        pktline::text(&mut resp, &format!("ACK {b} common\n"));
        pktline::text(&mut resp, &format!("ACK {b} ready\n"));
        pktline::text(&mut resp, "NAK\n");
        resp.extend_from_slice(b"\x01PACKDATA");
        let mut acked = BTreeSet::new();
        acked.insert(a.as_bytes().to_vec());
        let got = trim_replayed(resp.clone(), true, &mut acked);
        let mut want = Vec::new();
        pktline::text(&mut want, &format!("ACK {b} common\n"));
        pktline::text(&mut want, &format!("ACK {b} ready\n"));
        pktline::text(&mut want, "NAK\n");
        want.extend_from_slice(b"\x01PACKDATA");
        assert_eq!(got, want);
        // A later round replays the same acknowledgments: only the ready line, the NAK and
        // the pack are new.
        let mut later = Vec::new();
        pktline::text(&mut later, &format!("ACK {a} common\n"));
        pktline::text(&mut later, &format!("ACK {b} common\n"));
        pktline::text(&mut later, &format!("ACK {b} ready\n"));
        pktline::text(&mut later, "NAK\n");
        let got = trim_replayed(later, false, &mut acked);
        let mut want = Vec::new();
        pktline::text(&mut want, &format!("ACK {b} ready\n"));
        pktline::text(&mut want, "NAK\n");
        assert_eq!(got, want);
    }

    fn sample_pack() -> Vec<u8> {
        let base: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
        let mut changed = base.clone();
        changed[100] ^= 0xff;
        let a = RawObject::new(ObjectType::Blob, base);
        let b = RawObject::new(ObjectType::Blob, changed);
        let a_oid = a.oid(ObjectFormat::Sha1).unwrap();
        let objects = vec![
            PackObject::full(a),
            PackObject {
                object: b,
                delta_base: Some(bee_git::DeltaBase::InPack(a_oid)),
            },
            PackObject::full(RawObject::new(ObjectType::Blob, b"tiny".to_vec())),
        ];
        write_pack(ObjectFormat::Sha1, &objects).unwrap().pack
    }

    #[test]
    fn pack_end_is_found_however_the_bytes_arrive() {
        let pack = sample_pack();
        let mut stream = pack.clone();
        stream.extend_from_slice(b"trailing report bytes");
        for step in [1usize, 7, 64, 5000, stream.len()] {
            let mut scan = PackScanner::new(ObjectFormat::Sha1);
            let mut got = None;
            let mut n = 0;
            while n < stream.len() {
                n = (n + step).min(stream.len());
                if let Some(len) = scan.advance(&stream[..n]).unwrap() {
                    got = Some(len);
                    break;
                }
            }
            assert_eq!(got, Some(pack.len()), "step {step}");
        }
        let mut scan = PackScanner::new(ObjectFormat::Sha1);
        assert!(scan.advance(&pack[..pack.len() - 1]).unwrap().is_none());
        let mut scan = PackScanner::new(ObjectFormat::Sha1);
        assert!(scan.advance(b"NOTAPACKATALLNOTAPACK").is_err());
    }

    #[test]
    fn empty_pack_and_sha256() {
        let empty = write_pack(ObjectFormat::Sha1, &[]).unwrap().pack;
        let mut scan = PackScanner::new(ObjectFormat::Sha1);
        assert_eq!(scan.advance(&empty).unwrap(), Some(empty.len()));
        let one = write_pack(
            ObjectFormat::Sha256,
            &[PackObject::full(RawObject::new(
                ObjectType::Blob,
                b"x".to_vec(),
            ))],
        )
        .unwrap()
        .pack;
        let mut scan = PackScanner::new(ObjectFormat::Sha256);
        assert_eq!(scan.advance(&one).unwrap(), Some(one.len()));
    }

    #[tokio::test]
    async fn input_frames_packets_and_sections() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut input = Input::new(rx, 1 << 20);
        let mut bytes = Vec::new();
        pktline::text(&mut bytes, "want x\n");
        pktline::flush(&mut bytes);
        pktline::text(&mut bytes, "done\n");
        // Delivered in awkward pieces.
        for chunk in bytes.chunks(3) {
            tx.send(chunk.to_vec()).unwrap();
        }
        drop(tx);
        let s = input.section().await.unwrap().unwrap();
        assert_eq!(s, b"000bwant x\n0000");
        let p = input.pkt().await.unwrap().unwrap();
        assert_eq!(p.line(), b"done");
        assert!(input.pkt().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn input_limit_and_truncation() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut input = Input::new(rx, 10);
        tx.send(vec![b'0'; 20]).unwrap();
        assert!(input.pkt().await.is_err());
        let (tx, rx) = mpsc::unbounded_channel();
        let mut input = Input::new(rx, 100);
        tx.send(b"0010abc".to_vec()).unwrap();
        drop(tx);
        assert!(input.pkt().await.is_err());
    }

    #[test]
    fn keys_round_trip() {
        let k = generate_host_key().unwrap();
        let text = host_key_to_openssh(&k).unwrap();
        let back = host_key_from_openssh(&text).unwrap();
        assert_eq!(fingerprint(k.public_key()), fingerprint(back.public_key()));
        let line = k.public_key().to_openssh().unwrap();
        assert_eq!(
            fingerprint(&parse_public_key(&line).unwrap()),
            fingerprint(k.public_key())
        );
        assert!(fingerprint(k.public_key()).starts_with("SHA256:"));
        assert!(parse_public_key("not a key").is_err());
    }
}
