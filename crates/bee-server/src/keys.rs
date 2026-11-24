//! Token signing keys, shared by every server replica through the server KV store.

use std::path::PathBuf;

use bee_auth::{KeyRing, SigningKey};
use bee_proto::v1::ErrorReason;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::errors::{ApiError, ApiResult};
use crate::kv::{Kv, KvOp};
use crate::state::{AppState, KeyInfo};

const NS: &str = "signing_keys";
const KEY: &[u8] = b"ring";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredKey {
    id: u32,
    secret_hex: String,
    created_ms: u64,
    #[serde(default)]
    retired_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredRing {
    version: u64,
    keys: Vec<StoredKey>,
}

fn legacy_file(cfg: &Config) -> Option<PathBuf> {
    cfg.auth
        .signing_key_hex
        .is_empty()
        .then(|| cfg.server.data_dir.join("signing-keys.json"))
}
fn key_from(k: &StoredKey) -> ApiResult<SigningKey> {
    let bytes = bee_core::hex::decode(&k.secret_hex)
        .filter(|b| b.len() == 32)
        .ok_or_else(|| ApiError::new(ErrorReason::Internal, "signing key must be 64 hex digits"))?;
    let mut secret = [0; 32];
    secret.copy_from_slice(&bytes);
    SigningKey::from_secret_bytes(k.id, &secret).map_err(ApiError::from)
}
fn info(k: &StoredKey) -> ApiResult<KeyInfo> {
    let key = key_from(k)?;
    Ok(KeyInfo {
        id: k.id,
        public_key: key.public_key().to_bytes(),
        created_ms: k.created_ms,
        retired_ms: k.retired_ms,
    })
}
fn new_key(id: u32, now: u64) -> ApiResult<StoredKey> {
    Ok(StoredKey {
        id,
        secret_hex: bee_core::hex::encode(&crate::util::random_bytes(32)?),
        created_ms: now,
        retired_ms: 0,
    })
}
fn root(cfg: &Config) -> Option<StoredKey> {
    (!cfg.auth.signing_key_hex.is_empty()).then(|| StoredKey {
        id: cfg.auth.signing_key_id,
        secret_hex: cfg.auth.signing_key_hex.clone(),
        created_ms: 0,
        retired_ms: 0,
    })
}
fn legacy(cfg: &Config) -> ApiResult<Vec<StoredKey>> {
    let Some(path) = legacy_file(cfg) else {
        return Ok(Vec::new());
    };
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| {
            ApiError::new(
                ErrorReason::Internal,
                format!("bad {}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(ApiError::new(
            ErrorReason::Internal,
            format!("cannot read {}: {e}", path.display()),
        )),
    }
}
fn decode(raw: &[u8]) -> ApiResult<StoredRing> {
    serde_json::from_slice(raw).map_err(|e| {
        ApiError::new(
            ErrorReason::Internal,
            format!("bad shared signing key ring: {e}"),
        )
    })
}
fn encode(r: &StoredRing) -> ApiResult<Vec<u8>> {
    serde_json::to_vec(r).map_err(|e| ApiError::new(ErrorReason::Internal, e.to_string()))
}
fn materialize(r: &StoredRing) -> ApiResult<(KeyRing, Vec<KeyInfo>)> {
    let mut active: Vec<_> = r.keys.iter().filter(|k| k.retired_ms == 0).collect();
    active.sort_by_key(|k| (k.created_ms, k.id));
    let first = active
        .first()
        .ok_or_else(|| ApiError::new(ErrorReason::Internal, "no active signing key"))?;
    let mut ring = KeyRing::new(key_from(first)?);
    for k in active.iter().skip(1) {
        ring.rotate(key_from(k)?).map_err(ApiError::from)?;
    }
    Ok((ring, r.keys.iter().map(info).collect::<ApiResult<_>>()?))
}

/// Loads the shared ring, importing the legacy file once when no shared ring exists.
pub async fn load(cfg: &Config, kv: &dyn Kv, now: u64) -> ApiResult<(KeyRing, Vec<KeyInfo>)> {
    loop {
        let raw = kv.get(NS, KEY).await.map_err(ApiError::from)?;
        if let Some(raw) = raw {
            return materialize(&decode(&raw)?);
        }
        let mut keys = root(cfg).into_iter().collect::<Vec<_>>();
        if keys.is_empty() {
            keys = legacy(cfg)?;
        }
        if keys.is_empty() {
            keys.push(new_key(cfg.auth.signing_key_id, now)?);
        }
        let candidate = StoredRing { version: 1, keys };
        if kv
            .apply(vec![
                KvOp::expect(NS, KEY, None),
                KvOp::put(NS, KEY, encode(&candidate)?),
            ])
            .await
            .map_err(ApiError::from)?
        {
            return materialize(&candidate);
        }
    }
}
async fn current(state: &AppState) -> ApiResult<(Vec<u8>, StoredRing)> {
    let raw = state
        .kv
        .get(NS, KEY)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(ErrorReason::Internal, "shared signing key ring disappeared")
        })?;
    Ok((raw.clone(), decode(&raw)?))
}
fn install(state: &AppState, r: &StoredRing) -> ApiResult<()> {
    let (ring, keys) = materialize(r)?;
    *state.ring.write().unwrap_or_else(|p| p.into_inner()) = ring;
    *state.keys.write().unwrap_or_else(|p| p.into_inner()) = keys;
    // Verification results are keyed only by token text. Keep none across a refreshed ring or
    // a token proven valid before its signing key was retired would bypass the refresh bound.
    state.tokens.clear();
    share_with_forge(state);
    Ok(())
}
/// Refreshes this replica after the configured cache interval.
pub async fn refresh(state: &AppState) -> ApiResult<()> {
    let (_, r) = current(state).await?;
    install(state, &r)
}
/// Rotates with an optimistic compare-and-swap, preserving simultaneous rotations.
pub async fn rotate(state: &AppState) -> ApiResult<KeyInfo> {
    loop {
        let (raw, mut r) = current(state).await?;
        let id = r
            .keys
            .iter()
            .map(|k| k.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let key = new_key(id, state.now_ms())?;
        let result = info(&key)?;
        r.version += 1;
        r.keys.push(key);
        if state
            .kv
            .apply(vec![
                KvOp::expect(NS, KEY, Some(raw)),
                KvOp::put(NS, KEY, encode(&r)?),
            ])
            .await
            .map_err(ApiError::from)?
        {
            install(state, &r)?;
            return Ok(result);
        }
    }
}
/// Retires a verification key using the same compare-and-swap protocol.
pub async fn retire(state: &AppState, id: u32) -> ApiResult<()> {
    loop {
        let (raw, mut r) = current(state).await?;
        let (_, infos) = materialize(&r)?;
        if infos
            .iter()
            .filter(|k| k.retired_ms == 0)
            .max_by_key(|k| (k.created_ms, k.id))
            .is_some_and(|k| k.id == id)
        {
            return Err(ApiError::invalid(
                "cannot_retire_signing_key",
                "cannot retire the signing key",
            ));
        }
        let key = r.keys.iter_mut().find(|k| k.id == id).ok_or_else(|| {
            ApiError::invalid("unknown_signing_key", format!("no key with id {id}"))
        })?;
        if key.retired_ms != 0 {
            return Ok(());
        }
        key.retired_ms = state.now_ms();
        r.version += 1;
        if state
            .kv
            .apply(vec![
                KvOp::expect(NS, KEY, Some(raw)),
                KvOp::put(NS, KEY, encode(&r)?),
            ])
            .await
            .map_err(ApiError::from)?
        {
            install(state, &r)?;
            return Ok(());
        }
    }
}
/// Hands current verification keys to the forge.
pub fn share_with_forge(state: &AppState) {
    state.forget_anonymous_token();
    if let Some(w) = state.hub.get() {
        w.forge.authn.set_keys(
            state
                .ring
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .verifying_keys(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::SqliteKv;

    #[tokio::test]
    async fn replicas_import_or_create_one_shared_ring() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path().to_path_buf(), None);
        let db = dir.path().join("server.db");
        let a = SqliteKv::open(&db).unwrap();
        let b = SqliteKv::open(&db).unwrap();
        let (_, a_keys) = load(&cfg, &a, 1).await.unwrap();
        let (_, b_keys) = load(&cfg, &b, 2).await.unwrap();
        assert_eq!(a_keys.len(), 1);
        assert_eq!(a_keys[0].id, b_keys[0].id);
        assert_eq!(a_keys[0].public_key, b_keys[0].public_key);
    }
}
