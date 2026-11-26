//! The gateway object: the forge services it translates to, its ports, settings, the rate
//! limiter and the numeric-id index.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bee_forge::Forge;
use bee_forge::changes::Changes;
use bee_forge::changes::service::ChangeServices;
use bee_forge::ci::{Ci, CiServices};
use bee_forge::enterprise::Enterprise;
use bee_forge::enterprise::service::EnterpriseApi;
use bee_forge::ids::{Clock, SystemClock};
use bee_forge::service::ForgeServices;

use crate::ids::IdIndex;
use crate::memory::{GrepSearch, MemoryState, NoAssets};
use crate::ports::{AssetStore, CodeSearch, CompatState, GitPort, PassThroughTokens, TokenMap};

/// The REST API version the gateway implements (`X-GitHub-Api-Version`).
pub const API_VERSION: &str = "2022-11-28";

/// Gateway settings.
#[derive(Clone, Debug)]
pub struct CompatConfig {
    /// Public origin (`https://bee.example.com`) used in URLs; derived from the request's
    /// `Host` (and `X-Forwarded-Proto`) when `None`.
    pub public_url: Option<String>,
    /// Web origin for `html_url`; the API origin without a leading `api.` when `None`.
    pub web_url: Option<String>,
    /// Requests per hour per principal for the `core` resource.
    pub core_limit: u32,
    /// Requests per hour per principal for `search`.
    pub search_limit: u32,
    /// Requests per hour per principal for `graphql`.
    pub graphql_limit: u32,
    /// Requests per hour per client address without credentials.
    pub anonymous_limit: u32,
    /// Largest request body (release assets included).
    pub max_body_bytes: usize,
    /// X25519 secret of the Actions secrets sealed-box key (`GET …/secrets/public-key`).
    pub secret_box_key: [u8; 32],
    /// Version reported by `GET /meta` as `installed_version`.
    pub installed_version: String,
}

impl Default for CompatConfig {
    fn default() -> CompatConfig {
        CompatConfig {
            public_url: None,
            web_url: None,
            core_limit: 5000,
            search_limit: 30,
            graphql_limit: 5000,
            anonymous_limit: 60,
            max_body_bytes: 256 << 20,
            secret_box_key: *blake3::hash(b"swarm-compat development sealed-box key").as_bytes(),
            installed_version: "3.14.0".into(),
        }
    }
}

/// The ports the gateway calls.
pub struct Ports {
    /// The VCS in git terms.
    pub git: Arc<dyn GitPort>,
    /// Credential mapping.
    pub tokens: Arc<dyn TokenMap>,
    /// Gateway-only documents.
    pub state: Arc<dyn CompatState>,
    /// Release asset bytes.
    pub assets: Arc<dyn AssetStore>,
    /// Code search.
    pub code_search: Arc<dyn CodeSearch>,
    /// Time source for rate-limit windows.
    pub clock: Arc<dyn Clock>,
}

/// A rate-limit window as reported in headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateState {
    /// Requests allowed per window.
    pub limit: u32,
    /// Requests used in this window.
    pub used: u32,
    /// Seconds since the epoch when the window resets.
    pub reset: u64,
}

impl RateState {
    /// Requests left.
    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }
}

/// A rate-limit window key: client and resource.
type WindowKey = (String, &'static str);

/// Fixed one-hour windows per `(client, resource)`, starting at the first request.
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<WindowKey, (u64, u32)>>,
}

impl RateLimiter {
    /// The current window without consuming.
    pub fn peek(&self, key: &str, resource: &'static str, limit: u32, now: u64) -> RateState {
        let m = self.windows.lock().ok();
        let (start, used) = m
            .as_ref()
            .and_then(|m| m.get(&(key.to_string(), resource)).copied())
            .filter(|(start, _)| now < start + 3600)
            .unwrap_or((now, 0));
        RateState {
            limit,
            used,
            reset: start + 3600,
        }
    }

    /// Consumes one request; `Err` with the window when it is exhausted.
    pub fn take(
        &self,
        key: &str,
        resource: &'static str,
        limit: u32,
        now: u64,
    ) -> Result<RateState, RateState> {
        let Ok(mut m) = self.windows.lock() else {
            return Ok(RateState {
                limit,
                used: 0,
                reset: now + 3600,
            });
        };
        let e = m.entry((key.to_string(), resource)).or_insert((now, 0));
        if now >= e.0 + 3600 {
            *e = (now, 0);
        }
        let state = RateState {
            limit,
            used: e.1,
            reset: e.0 + 3600,
        };
        if e.1 >= limit {
            return Err(state);
        }
        e.1 += 1;
        Ok(RateState { used: e.1, ..state })
    }

    /// Gives back one request (a `304 Not Modified` is free, as on GitHub).
    pub fn refund(&self, key: &str, resource: &'static str) {
        if let Ok(mut m) = self.windows.lock()
            && let Some(e) = m.get_mut(&(key.to_string(), resource))
        {
            e.1 = e.1.saturating_sub(1);
        }
    }
}

/// The GitHub compatibility gateway over one forge.
pub struct Compat {
    pub(crate) forge: Arc<Forge>,
    pub(crate) svc: ForgeServices,
    pub(crate) changes: Option<ChangeServices>,
    pub(crate) ci: Option<CiServices>,
    pub(crate) teams: Option<EnterpriseApi>,
    pub(crate) ports: Ports,
    pub(crate) config: CompatConfig,
    pub(crate) limiter: RateLimiter,
    pub(crate) ids: IdIndex,
}

impl Compat {
    /// A builder over `forge` and the VCS port `git`.
    pub fn builder(forge: Arc<Forge>, git: Arc<dyn GitPort>) -> CompatBuilder {
        CompatBuilder {
            forge,
            changes: None,
            ci: None,
            enterprise: None,
            ports: Ports {
                code_search: Arc::new(GrepSearch::new(git.clone())),
                git,
                tokens: Arc::new(PassThroughTokens),
                state: Arc::new(MemoryState::default()),
                assets: Arc::new(NoAssets),
                clock: Arc::new(SystemClock),
            },
            config: CompatConfig::default(),
        }
    }

    /// The forge.
    pub fn forge(&self) -> &Arc<Forge> {
        &self.forge
    }

    /// The settings.
    pub fn config(&self) -> &CompatConfig {
        &self.config
    }

    /// The ports.
    pub fn ports(&self) -> &Ports {
        &self.ports
    }
}

/// Builds a [`Compat`].
pub struct CompatBuilder {
    forge: Arc<Forge>,
    changes: Option<Arc<Changes>>,
    ci: Option<Arc<Ci>>,
    enterprise: Option<Arc<Enterprise>>,
    ports: Ports,
    config: CompatConfig,
}

impl CompatBuilder {
    /// Serves pull requests over these change requests.
    pub fn changes(mut self, changes: Arc<Changes>) -> CompatBuilder {
        self.changes = Some(changes);
        self
    }

    /// Serves Actions, secrets, environments and deployments over this CI module.
    pub fn ci(mut self, ci: Arc<Ci>) -> CompatBuilder {
        self.ci = Some(ci);
        self
    }

    /// Serves teams over this enterprise module.
    pub fn enterprise(mut self, ent: Arc<Enterprise>) -> CompatBuilder {
        self.enterprise = Some(ent);
        self
    }

    /// Replaces the credential mapping (default: [`PassThroughTokens`]).
    pub fn tokens(mut self, tokens: Arc<dyn TokenMap>) -> CompatBuilder {
        self.ports.tokens = tokens;
        self
    }

    /// Replaces the gateway state store (default: in memory).
    pub fn state(mut self, state: Arc<dyn CompatState>) -> CompatBuilder {
        self.ports.state = state;
        self
    }

    /// Sets where release asset bytes go (default: uploads are refused).
    pub fn assets(mut self, assets: Arc<dyn AssetStore>) -> CompatBuilder {
        self.ports.assets = assets;
        self
    }

    /// Replaces code search (default: a scan of default branches through the git port).
    pub fn code_search(mut self, search: Arc<dyn CodeSearch>) -> CompatBuilder {
        self.ports.code_search = search;
        self
    }

    /// Replaces the clock used for rate-limit windows.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> CompatBuilder {
        self.ports.clock = clock;
        self
    }

    /// Replaces the settings.
    pub fn config(mut self, config: CompatConfig) -> CompatBuilder {
        self.config = config;
        self
    }

    /// The gateway.
    pub fn build(self) -> Arc<Compat> {
        let forge = self.forge;
        Arc::new(Compat {
            svc: ForgeServices::new(forge.clone()),
            changes: self.changes.map(|c| ChangeServices::new(forge.clone(), c)),
            ci: self.ci.map(|c| CiServices::new(forge.clone(), c)),
            teams: self.enterprise.map(EnterpriseApi::new),
            forge,
            ports: self.ports,
            config: self.config,
            limiter: RateLimiter::default(),
            ids: IdIndex::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_reset_after_an_hour() {
        let l = RateLimiter::default();
        assert_eq!(l.take("a", "core", 2, 100).unwrap().remaining(), 1);
        assert_eq!(l.take("a", "core", 2, 101).unwrap().remaining(), 0);
        let err = l.take("a", "core", 2, 102).unwrap_err();
        assert_eq!(err.reset, 3700);
        assert_eq!(l.take("b", "core", 2, 102).unwrap().used, 1);
        assert_eq!(l.take("a", "core", 2, 3700).unwrap().used, 1);
        l.refund("a", "core");
        assert_eq!(l.peek("a", "core", 2, 3701).used, 0);
    }
}
