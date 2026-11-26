//! A gateway over a real forge on SQLite with in-memory VCS stores, plus request helpers.
#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bee_auth::{InMemoryRevocations, KeyRing, MintRequest, Scope, SigningKey};
use bee_compat::memory::{HistoryGit, HistoryResolver, HistoryTags, MemoryAssets};
use bee_compat::{Compat, CompatConfig};
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams};
use bee_forge::ci::{Ci, CiConfig, CiHooks, HistoryCiVcs, HmacKey, MemoryCiBlobs};
use bee_forge::community::ports::Ports as CommunityPorts;
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::forge::{ForgeConfig, Hooks};
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::{Cx, Forge};
use bee_proto::v1::{Org, Principal, PrincipalKind, Role};
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};
use http::Request;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

/// People of the test org.
pub const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("agent", PrincipalKind::Agent),
];

/// The org every test uses.
pub const ORG: &str = "acme";

/// A running gateway.
pub struct World {
    pub forge: Arc<Forge>,
    pub compat: Arc<Compat>,
    pub router: Router,
    pub ring: KeyRing,
    pub clock: Arc<ManualClock>,
    pub assets: Arc<MemoryAssets>,
    pub org: Org,
    pub directory: Arc<MemoryDirectory>,
    _dir: tempfile::TempDir,
}

/// A response.
pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
    pub json: Value,
}

impl Resp {
    /// A header value.
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).and_then(|v| v.to_str().ok())
    }
}

/// Builds the world with `config`.
pub async fn world_with(config: CompatConfig) -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(ManualClock::new(1_760_000_000_000));
    let c: Arc<dyn Clock> = clock.clone();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), c.clone())
        .await
        .expect("sqlite forge store");
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(c.clone()));
    let writer = Writer::new(meta.clone(), blobs);
    let directory = Arc::new(MemoryDirectory::new());
    for (id, kind) in PEOPLE {
        directory.register(
            Principal {
                id: id.to_string(),
                kind: *kind as i32,
                display: format!("{id} person"),
                ..Default::default()
            },
            vec![id.to_string()],
            vec![format!("{id}@example.com")],
        );
    }
    let ring = KeyRing::new(SigningKey::from_secret_bytes(1, &[7u8; 32]).expect("key"));
    let hooks = Hooks {
        directory: directory.clone(),
        provisioner: Arc::new(MetaProvisioner::new(meta, c.clone())),
        resolver: Arc::new(HistoryResolver::new(writer.clone(), c.clone())),
        community: CommunityPorts {
            tags: Arc::new(HistoryTags::new(writer.clone(), c.clone())),
            vcs: Arc::new(bee_forge::community::ports::MemoryVcs::new()),
            ..CommunityPorts::default()
        },
        ..Hooks::default()
    };
    let (forge, _) = Forge::open(
        Arc::new(store),
        c.clone(),
        ring.verifying_keys(),
        Arc::new(InMemoryRevocations::new()),
        hooks,
        ForgeConfig::default(),
    )
    .await
    .expect("open forge");
    let vcs = Arc::new(HistoryVcs::new(writer.clone(), c.clone()));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    let ci = Ci::new(
        &forge,
        CiHooks {
            vcs: Arc::new(HistoryCiVcs::new(vcs.clone())),
            blobs: Arc::new(MemoryCiBlobs::new()),
            keys: Arc::new(HmacKey::new("k1", b"oidc-key")),
            secret_key: [3u8; 32],
            url_key: b"url-key".to_vec(),
            signer: Arc::new(KeyRing::new(
                SigningKey::from_secret_bytes(1, &[8u8; 32]).expect("key"),
            )),
            config: CiConfig::default(),
        },
    );
    ci.attach(&forge);
    let ent = Enterprise::new(forge.clone(), EnterpriseConfig::new("http://localhost"));
    ent.attach();
    let assets = Arc::new(MemoryAssets::default());
    let compat = Compat::builder(forge.clone(), Arc::new(HistoryGit::new(writer, c.clone())))
        .changes(changes)
        .ci(ci)
        .enterprise(ent)
        .assets(assets.clone())
        .clock(c)
        .config(config)
        .build();
    let router = compat.router().merge(compat.root_router());
    let cx = Cx::new("owner", PrincipalKind::Human);
    let owner = directory.get("owner").await.expect("owner");
    let org = forge
        .orgs
        .create_org(&cx, ORG, "", owner)
        .await
        .expect("org");
    forge
        .hooks
        .provisioner
        .create_org(&org)
        .await
        .expect("vcs org");
    for (who, role) in [
        ("writer", Role::Member),
        ("reader", Role::Member),
        ("agent", Role::Member),
    ] {
        let p = directory.get(who).await.expect("principal");
        forge
            .orgs
            .set_member(&cx, &org.id, p, role)
            .await
            .expect("member");
    }
    World {
        forge,
        compat,
        router,
        ring,
        clock,
        assets,
        org,
        directory,
        _dir: dir,
    }
}

/// The default world.
pub async fn world() -> World {
    world_with(CompatConfig::default()).await
}

impl World {
    /// An unrestricted token for `id`.
    pub fn token(&self, id: &str) -> String {
        let principal = match PEOPLE.iter().find(|(p, _)| *p == id).map(|(_, k)| *k) {
            Some(PrincipalKind::Agent) => bee_auth::Principal::agent(
                id,
                id,
                bee_auth::AgentInfo {
                    owner: bee_policy::PrincipalId::new("owner"),
                    runtime: "test".into(),
                    purpose: "tests".into(),
                },
            ),
            _ => bee_auth::Principal::human(id, id),
        };
        let on_behalf_of = if id == "agent" {
            vec![bee_auth::PrincipalRef::new(
                "owner",
                bee_policy::PrincipalKind::Human,
            )]
        } else {
            Vec::new()
        };
        self.ring
            .mint(
                &MintRequest {
                    principal,
                    on_behalf_of,
                    scope: Scope::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .expect("mint")
            .text
    }

    /// Sends a request with optional token and JSON body.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        self.send(method, path, token, body, &[]).await
    }

    /// Sends a request with extra headers.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Resp {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "ghe.example.test")
            .header("user-agent", "compat-tests");
        if let Some(t) = token {
            b = b.header("authorization", format!("token {t}"));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&v).expect("json"))),
            None => b.body(Body::empty()),
        }
        .expect("request");
        self.raw(req).await
    }

    /// Sends a prepared request.
    pub async fn raw(&self, req: Request<Body>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.expect("response");
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            body,
            json,
        }
    }

    /// `GET` as `who`.
    pub async fn get(&self, path: &str, who: &str) -> Resp {
        let t = self.token(who);
        self.call("GET", path, Some(&t), None).await
    }

    /// A GraphQL query as `who`.
    pub async fn gql(&self, who: &str, query: &str, vars: Value) -> Value {
        let t = self.token(who);
        let r = self
            .call(
                "POST",
                "/api/graphql",
                Some(&t),
                Some(json!({"query": query, "variables": vars})),
            )
            .await;
        assert_eq!(r.status, 200, "graphql status: {:?}", r.json);
        r.json
    }

    /// Creates `acme/<name>` with a README on `main` through the REST API; returns the repo.
    pub async fn repo(&self, name: &str) -> Value {
        let t = self.token("owner");
        let r = self
            .call(
                "POST",
                &format!("/api/v3/orgs/{ORG}/repos"),
                Some(&t),
                Some(json!({"name": name, "description": "test repo"})),
            )
            .await;
        assert_eq!(r.status, 201, "create repo: {:?}", r.json);
        let c = self
            .call(
                "PUT",
                &format!("/api/v3/repos/{ORG}/{name}/collaborators/writer"),
                Some(&t),
                Some(json!({"permission": "push"})),
            )
            .await;
        assert!(
            c.status == 201 || c.status == 204,
            "add writer: {:?}",
            c.json
        );
        self.put_file(name, "main", "README.md", "# hello\n", "Initial commit")
            .await;
        r.json
    }

    /// Writes a file through the contents API; returns the commit sha.
    pub async fn put_file(
        &self,
        repo: &str,
        branch: &str,
        path: &str,
        content: &str,
        message: &str,
    ) -> String {
        use base64::Engine as _;
        let t = self.token("owner");
        let url = format!("/api/v3/repos/{ORG}/{repo}/contents/{path}");
        let cur = self
            .call("GET", &format!("{url}?ref={branch}"), Some(&t), None)
            .await;
        let mut body = json!({
            "message": message,
            "content": base64::engine::general_purpose::STANDARD.encode(content),
            "branch": branch,
        });
        if cur.status == 200 {
            body["sha"] = cur.json["sha"].clone();
        }
        let r = self.call("PUT", &url, Some(&t), Some(body)).await;
        assert!(
            r.status == 201 || r.status == 200,
            "put file: {} {:?}",
            r.status,
            r.json
        );
        r.json["commit"]["sha"].as_str().expect("sha").to_string()
    }

    /// Creates branch `name` at the tip of `from`.
    pub async fn branch(&self, repo: &str, name: &str, from: &str) {
        let t = self.token("owner");
        let base = self
            .call(
                "GET",
                &format!("/api/v3/repos/{ORG}/{repo}/git/ref/heads/{from}"),
                Some(&t),
                None,
            )
            .await;
        assert_eq!(base.status, 200, "{:?}", base.json);
        let r = self
            .call(
                "POST",
                &format!("/api/v3/repos/{ORG}/{repo}/git/refs"),
                Some(&t),
                Some(
                    json!({"ref": format!("refs/heads/{name}"), "sha": base.json["object"]["sha"]}),
                ),
            )
            .await;
        assert_eq!(r.status, 201, "{:?}", r.json);
    }

    /// Opens PR `head` → `main` with one changed file; returns the PR JSON.
    pub async fn pull(&self, repo: &str, head: &str, title: &str) -> Value {
        self.branch(repo, head, "main").await;
        self.put_file(
            repo,
            head,
            &format!("{head}.txt"),
            "change\n",
            &format!("Add {head}"),
        )
        .await;
        let t = self.token("owner");
        let r = self
            .call(
                "POST",
                &format!("/api/v3/repos/{ORG}/{repo}/pulls"),
                Some(&t),
                Some(
                    json!({"title": title, "head": head, "base": "main", "body": "Please review"}),
                ),
            )
            .await;
        assert_eq!(r.status, 201, "create pull: {:?}", r.json);
        r.json
    }
}

/// Asserts that `v` has every key in `keys`.
pub fn has_keys(v: &Value, keys: &[&str]) {
    for k in keys {
        assert!(v.get(*k).is_some(), "missing `{k}` in {v}");
    }
}
