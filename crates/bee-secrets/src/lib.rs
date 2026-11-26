//! Secret detection for push protection (PRODUCT 4.18).
//!
//! A layer-1 library: the server and the git gateway scan the blobs a changeset adds, then call
//! [`evaluate_push_protection`]. A blocked [`Decision`] becomes `SECRET_DETECTED`
//! ([`bee_core::Reason::SecretDetected`]) through [`Decision::to_error`]; an exercised bypass
//! is returned in [`Decision::bypass`] for the audit log.
//!
//! # Pipeline
//!
//! 1. Skip the file when it is over the size cap (default 1 MiB, [`ScanOptions`]) or has a NUL in
//!    its first 8000 bytes (binary).
//! 2. One Aho-Corasick pass (ASCII case-insensitive) finds which rules' keywords occur. Rules
//!    with no keyword hit never run their regex, so most files cost a single linear pass.
//! 3. Each surviving rule's regex runs in byte mode with ASCII semantics. The secret is the
//!    first participating capture group. A rule's `entropy` floor (Shannon bits per byte of the
//!    secret) discards low-entropy matches such as `AKIAAAAAAAAAAAAAAAAA`.
//! 4. Allow-lists drop findings: path globs and rule ids ([`AllowEntry`]), and the inline marker
//!    `bee:allow-secret` anywhere on the line where the secret starts (or ends).
//! 5. Output is sorted by start offset; a finding wholly inside a wider one, or covering the same bytes as a
//!    higher-confidence one, is dropped
//!    (a connection string hides its embedded account key). Findings hold offsets and a
//!    redacted preview (at most a quarter of the secret, `AKIA****`), never the secret.
//!
//! # Rules
//!
//! Built-in rules ([`builtin_rules`]) cover AWS, GitHub, GitLab, Slack, Stripe, Google, Azure,
//! OpenAI, Anthropic, npm, PyPI, PEM and PGP private keys, JWTs, Twilio, SendGrid, Mailgun,
//! Discord and Telegram, plus a low-confidence generic `password=`/`secret=`/`token=` class.
//! Each carries an `example` that must match and a `near_miss` that must not; tests enforce
//! both. Custom rules use the same shape through TOML ([`Scanner::from_toml`], [`RuleSpec`]).
//!
//! # Confidence and blocking
//!
//! `High` and `Medium` findings block; `Low` findings are advisory. A bypass
//! ([`Bypass`] with a [`BypassReason`]: `false_positive`, `used_in_tests`, `will_fix_later`)
//! must already be authorised by policy; this crate only records it in the [`Decision`].
//!
//! # Path globs
//!
//! `*` and `?` never cross `/`; `**` does (`**/` also matches no directory). A pattern with no
//! `/` matches the basename at any depth; a leading `/` anchors at the root; a trailing `/`
//! means everything below. Matching is byte-exact and case-sensitive.
//!
//! Everything here is a pure function of its inputs: no clock, no randomness, no hash-map
//! iteration.

mod builtin;
mod glob;
mod protect;
mod rule;
mod scanner;

pub use builtin::builtin_rules;
pub use protect::{Bypass, BypassReason, Decision, evaluate_push_protection};
pub use rule::{Confidence, Rule, RuleSpec, shannon_entropy};
pub use scanner::{
    AllowEntry, BINARY_SNIFF_BYTES, DEFAULT_MAX_FILE_SIZE, Finding, INLINE_ALLOW, ScanOptions,
    Scanner,
};
