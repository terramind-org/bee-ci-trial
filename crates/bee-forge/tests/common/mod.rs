//! Shared setup for the forge integration tests.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use bee_auth::{
    AgentInfo, InMemoryRevocations, KeyRing, MintRequest, PrincipalRef, Scope, SigningKey,
};
use bee_forge::Forge;
use bee_forge::events::delivery::RetryPolicy;
use bee_forge::events::webhooks::DeliveryConfig;
use bee_forge::forge::{ForgeConfig, Hooks};
use bee_forge::hooks::MemoryDirectory;
use bee_forge::ids::{Clock, SystemClock};
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_policy::{Action, PrincipalId};
use bee_proto::v1::{Principal, PrincipalKind};

/// A forge plus what tests need to call it.
pub struct Env {
    pub forge: Arc<Forge>,
    pub ring: KeyRing,
    pub revocations: Arc<InMemoryRevocations>,
    pub directory: Arc<MemoryDirectory>,
    pub clock: Arc<dyn Clock>,
}

pub fn test_config() -> ForgeConfig {
    ForgeConfig {
        delivery: DeliveryConfig {
            concurrency: 4,
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
            },
            timeout: Duration::from_secs(5),
            claim: Duration::from_secs(30),
        },
        ..ForgeConfig::default()
    }
}

pub async fn env_with(store: Arc<dyn ForgeStore>, clock: Arc<dyn Clock>) -> Env {
    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let revocations = Arc::new(InMemoryRevocations::new());
    let directory = Arc::new(MemoryDirectory::new());
    let hooks = Hooks {
        directory: directory.clone(),
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        store,
        clock.clone(),
        ring.verifying_keys(),
        revocations.clone(),
        hooks,
        test_config(),
    )
    .await
    .expect("open forge");
    for (id, kind) in [
        ("owner", PrincipalKind::Human),
        ("admin", PrincipalKind::Human),
        ("writer", PrincipalKind::Human),
        ("reader", PrincipalKind::Human),
        ("outsider", PrincipalKind::Human),
        ("ci", PrincipalKind::Service),
        ("agent", PrincipalKind::Agent),
    ] {
        directory.register(
            Principal {
                id: id.into(),
                kind: kind as i32,
                display: id.into(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    Env {
        forge,
        ring,
        revocations,
        directory,
        clock,
    }
}

pub async fn env() -> Env {
    env_with(Arc::new(MemoryStore::new()), Arc::new(SystemClock)).await
}

impl Env {
    fn now_secs(&self) -> u64 {
        self.clock.now_ms() / 1000
    }

    /// A token for a human or service principal.
    pub fn token(&self, id: &str, scope: Scope) -> String {
        let principal = if id == "ci" {
            bee_auth::Principal::service(id, id)
        } else {
            bee_auth::Principal::human(id, id)
        };
        self.ring
            .mint(
                &MintRequest {
                    principal,
                    on_behalf_of: vec![],
                    scope,
                },
                self.now_secs(),
            )
            .expect("mint")
            .text
    }

    /// A token for agent `agent` owned by `owner`.
    pub fn agent_token(&self, owner: &str, scope: Scope) -> String {
        self.ring
            .mint(
                &MintRequest {
                    principal: bee_auth::Principal::agent(
                        "agent",
                        "agent",
                        AgentInfo {
                            owner: PrincipalId::new(owner),
                            runtime: "claude-code".into(),
                            purpose: "tests".into(),
                        },
                    ),
                    on_behalf_of: vec![PrincipalRef::new(owner, bee_policy::PrincipalKind::Human)],
                    scope,
                },
                self.now_secs(),
            )
            .expect("mint agent")
            .text
    }

    /// An unrestricted token.
    pub fn full(&self, id: &str) -> String {
        self.token(id, Scope::new())
    }
}

/// A token scope allowing only `actions`.
pub fn actions(a: &[Action]) -> Scope {
    Scope::new().actions(a.iter().copied())
}

/// A request with a bearer token.
pub fn req<T>(token: &str, msg: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    r.metadata_mut()
        .insert("x-request-id", "test-request".parse().unwrap());
    r
}

/// The `ErrorDetail.reason` of a failed call.
pub fn reason<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> String {
    let st = r.expect_err("expected failure");
    bee_proto::errors::detail_from_status(&st)
        .map(|d| d.reason)
        .unwrap_or_else(|| format!("no detail: {st:?}"))
}
