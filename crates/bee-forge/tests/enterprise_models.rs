//! Model gateway records: catalog and org policy, provider keys, the prompt file index, eval
//! runs, inference admission and usage, and the `ModelService` permission matrix. Store flows
//! run on memory and SQLite (and Postgres with `BEE_TEST_PG`).

use crate::billing_models_backends as backends;
use crate::common;
use crate::enterprise_support as support;

use std::sync::Arc;

use backends::{DAY, HOUR, T0, on_backends};
use bee_forge::checks::RunFilter;
use bee_forge::enterprise::billing::{Billing, BillingConfig, UsageInput};
use bee_forge::enterprise::common::{OrgScopeResolver, ScopeResolver, SpendGate};
use bee_forge::enterprise::models::{
    EvalFilter, EvalResult, InferenceEstimate, InferenceUsage, Models, ModelsService,
    OrgModelPolicy,
};
use bee_forge::events::org_log;
use bee_forge::ids::{ManualClock, hex};
use bee_forge::service::ForgeServices;
use bee_forge::store::ForgeStore;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::model_service_server::ModelService as ModelRpc;
use bee_proto::v1::{
    BillingProduct, CancelEvalRunRequest, CheckStatus, Cost, CreateBillingBudgetRequest,
    CreateModelProviderKeyRequest, DeleteModelProviderKeyRequest, ErrorReason, EvalRunState,
    EvalScore, Event, GetEvalRunRequest, GetModelRequest, GetModelUsageRequest,
    GetPromptFileRequest, ListEvalRunsRequest, ListModelProviderKeysRequest, ListModelsRequest,
    ListPromptFilesRequest, ModelInfo, ModelMessage, ModelMessageRole, ModelParams, ModelPricing,
    ObjectId, Org, PageRequest, PromptFile, Repo, RepoRef, Revision, Role,
    RunModelInferenceRequest, RunPromptEvalRequest, StreamModelInferenceRequest, repo_ref,
};

fn owner() -> Cx {
    support::cx("owner")
}

struct Fx {
    env: common::Env,
    models: Arc<Models>,
    clock: Arc<ManualClock>,
    org: Org,
    repo: Repo,
}

async fn fx(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>) -> Fx {
    fx_with(store, clock, false).await
}

/// `with_billing` wires a [`Billing`] in as the models' spend gate.
async fn fx_with(store: Arc<dyn ForgeStore>, clock: Arc<ManualClock>, with_billing: bool) -> Fx {
    let env = common::env_with(store, clock.clone()).await;
    let name = format!("acme{}", env.forge.db.ids().ulid().unwrap().to_lowercase());
    let org = support::make_org(&env, &name).await;
    support::add_member(&env, &org, "admin", Role::Admin).await;
    support::add_member(&env, &org, "reader", Role::Member).await;
    let repo = support::make_repo(&env, &org, "app").await;
    let gate: Option<Arc<dyn SpendGate>> = if with_billing {
        Some(Billing::new(env.forge.clone(), BillingConfig::default()))
    } else {
        None
    };
    let models = Models::new(env.forge.clone(), gate);
    Fx {
        env,
        models,
        clock,
        org,
        repo,
    }
}

fn model(id: &str, caps: &[&str]) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        provider: id.split('/').next().unwrap().into(),
        display_name: id.into(),
        capabilities: caps.iter().map(|c| c.to_string()).collect(),
        context_window_tokens: 200_000,
        max_output_tokens: 8_000,
        pricing: Some(ModelPricing {
            input_usd_micros_per_mtok: 3_000_000,
            output_usd_micros_per_mtok: 15_000_000,
            cached_input_usd_micros_per_mtok: 300_000,
        }),
        ..Default::default()
    }
}

fn prompt(path: &str, name: &str, model: &str, blob: u8) -> PromptFile {
    PromptFile {
        path: path.as_bytes().to_vec(),
        blob: Some(ObjectId {
            raw: vec![blob; 32],
        }),
        name: name.into(),
        description: "summarizes".into(),
        model: model.into(),
        params: Some(ModelParams {
            temperature: Some(0.2),
            max_output_tokens: 512,
            ..Default::default()
        }),
        messages: vec![
            ModelMessage {
                role: ModelMessageRole::System as i32,
                content: "You are {{persona}}".into(),
                ..Default::default()
            },
            ModelMessage {
                role: ModelMessageRole::User as i32,
                content: "{{question}}".into(),
                ..Default::default()
            },
        ],
        variables: vec!["persona".into(), "question".into()],
        test_data_count: 4,
        evaluators: vec!["exact".into(), "similarity".into()],
        raw: "name: summarize\nmodel: anthropic/claude-sonnet\n".into(),
    }
}

fn err<T: std::fmt::Debug>(r: Result<T, ForgeError>) -> ForgeError {
    r.expect_err("expected an error")
}

fn key_req(org: &Org, provider: &str, name: &str) -> CreateModelProviderKeyRequest {
    CreateModelProviderKeyRequest {
        org: org.id.clone(),
        provider: provider.into(),
        name: name.into(),
        secret_name: format!("{}_API_KEY", provider.to_uppercase()),
        ..Default::default()
    }
}

impl Fx {
    async fn seed_catalog(&self) {
        for m in [
            model("anthropic/claude-sonnet", &["chat", "tools", "vision"]),
            model("anthropic/claude-haiku", &["chat", "tools"]),
            model("openai/gpt-x", &["chat", "reasoning"]),
        ] {
            self.models.upsert_model(&owner(), m).await.unwrap();
        }
        let mut private = model("acme/private-embed", &["embeddings"]);
        private.byok_only = true;
        self.models.upsert_model(&owner(), private).await.unwrap();
    }

    async fn events(&self, kind: &str) -> Vec<Event> {
        self.env.forge.flush().await.unwrap();
        self.env
            .forge
            .events
            .read(
                &owner(),
                &org_log(&self.org.id),
                0,
                &[kind.to_string()],
                1000,
            )
            .await
            .unwrap()
            .0
    }

    async fn admit(
        &self,
        org: &str,
        model: &str,
        key: &str,
    ) -> Result<bee_forge::enterprise::models::Route, ForgeError> {
        self.models
            .admit_inference(
                &owner(),
                org,
                model,
                key,
                InferenceEstimate {
                    input_tokens: 1000,
                    max_output_tokens: 1000,
                },
            )
            .await
    }
}

// ---------- catalog and policy ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_catalog_filters_and_org_policy() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.seed_catalog().await;
        let list = |org: &str, provider: &str, cap: &str, after: &str, limit: usize| {
            let (m, org, provider, cap, after) = (
                f.models.clone(),
                org.to_string(),
                provider.to_string(),
                cap.to_string(),
                after.to_string(),
            );
            async move {
                m.list_models(&owner(), &org, &provider, &cap, &after, limit)
                    .await
                    .unwrap()
            }
        };
        let (all, next) = list("", "", "", "", 50).await;
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "acme/private-embed",
                "anthropic/claude-haiku",
                "anthropic/claude-sonnet",
                "openai/gpt-x"
            ]
        );
        assert!(next.is_empty());
        // Without an org, BYOK-only models are not enabled.
        assert_eq!(all.iter().filter(|m| m.enabled_for_org).count(), 3);
        let (anthropic, _) = list("", "anthropic", "", "", 50).await;
        assert_eq!(anthropic.len(), 2);
        let (vision, _) = list("", "", "vision", "", 50).await;
        assert_eq!(vision.len(), 1);
        assert_eq!(vision[0].id, "anthropic/claude-sonnet");
        let (none, _) = list("", "openai", "vision", "", 50).await;
        assert!(none.is_empty());
        let (page, next) = list("", "", "", "", 2).await;
        assert_eq!((page.len(), next.as_str()), (2, "anthropic/claude-haiku"));
        let (rest, next) = list("", "", "", &next, 2).await;
        assert_eq!((rest.len(), next.is_empty()), (2, true));

        // Org view: everything allowed, BYOK-only needs a key.
        let (view, _) = list(&f.org.id, "", "", "", 50).await;
        let on: Vec<&str> = view
            .iter()
            .filter(|m| m.enabled_for_org)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(
            on,
            [
                "anthropic/claude-haiku",
                "anthropic/claude-sonnet",
                "openai/gpt-x"
            ]
        );
        f.models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "acme", "acme key"))
            .await
            .unwrap();
        let m = f
            .models
            .get_model(&owner(), &f.org.id, "acme/private-embed")
            .await
            .unwrap();
        assert!(
            m.enabled_for_org,
            "a key for the provider enables BYOK-only models"
        );
        // An allow list narrows it.
        let p = f
            .models
            .set_org_policy(
                &owner(),
                &f.org.id,
                OrgModelPolicy {
                    allowed_models: Some(vec![
                        "anthropic/claude-sonnet".into(),
                        "anthropic/claude-sonnet".into(),
                    ]),
                    budget_usd_micros: 5_000_000,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            p.allowed_models.as_deref(),
            Some(&["anthropic/claude-sonnet".to_string()][..])
        );
        let (view, _) = list(&f.org.id, "", "", "", 50).await;
        let on: Vec<&str> = view
            .iter()
            .filter(|m| m.enabled_for_org)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(on, ["anthropic/claude-sonnet"]);
        assert!(
            !f.models
                .get_model(&owner(), &f.org.id, "openai/gpt-x")
                .await
                .unwrap()
                .enabled_for_org
        );
        assert!(
            f.models
                .get_model(&owner(), "", "openai/gpt-x")
                .await
                .unwrap()
                .enabled_for_org,
            "no org, no policy"
        );
        assert_eq!(
            f.models
                .org_policy(&owner(), &f.org.id)
                .await
                .unwrap()
                .budget_usd_micros,
            5_000_000
        );
        // Lifting the allow list.
        f.models
            .set_org_policy(&owner(), &f.org.id, OrgModelPolicy::default())
            .await
            .unwrap();
        assert_eq!(
            f.models
                .org_policy(&owner(), &f.org.id)
                .await
                .unwrap()
                .allowed_models,
            None
        );
        assert_eq!(
            err(f
                .models
                .set_org_policy(
                    &owner(),
                    &f.org.id,
                    OrgModelPolicy {
                        allowed_models: Some(vec!["nope/x".into()]),
                        budget_usd_micros: 0
                    }
                )
                .await)
            .reason,
            ErrorReason::NotFound
        );

        // Admin validation, replacement and removal.
        let mut bad = model("anthropic/x", &["chat"]);
        bad.provider = "openai".into();
        assert_eq!(
            err(f.models.upsert_model(&owner(), bad).await).subcode,
            "invalid_model"
        );
        assert_eq!(
            err(f
                .models
                .upsert_model(&owner(), model("openai/y", &["telepathy"]))
                .await)
            .subcode,
            "unknown_capability"
        );
        let mut zero = model("openai/z", &["chat"]);
        zero.context_window_tokens = 0;
        assert_eq!(
            err(f.models.upsert_model(&owner(), zero).await).subcode,
            "invalid_model"
        );
        let mut sonnet = model("anthropic/claude-sonnet", &["chat", "chat", "tools"]);
        sonnet.display_name = "Claude Sonnet".into();
        sonnet.enabled_for_org = true; // computed, never stored
        let stored = f.models.upsert_model(&owner(), sonnet).await.unwrap();
        assert_eq!(stored.capabilities, ["chat", "tools"]);
        let got = f
            .models
            .get_model(&owner(), "", "anthropic/claude-sonnet")
            .await
            .unwrap();
        assert_eq!(got.display_name, "Claude Sonnet");
        f.models
            .remove_model(&owner(), "openai/gpt-x")
            .await
            .unwrap();
        assert_eq!(
            err(f.models.get_model(&owner(), "", "openai/gpt-x").await).reason,
            ErrorReason::NotFound
        );
        assert_eq!(
            err(f.models.remove_model(&owner(), "openai/gpt-x").await).reason,
            ErrorReason::NotFound
        );
        assert!(f.events("model.policy_updated").await.len() >= 2);
        f.env.forge.flush().await.unwrap();
    })
    .await;
}

// ---------- provider keys ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_provider_keys_defaults_and_validation() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.seed_catalog().await;
        let k1 = f
            .models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "openai", "main"))
            .await
            .unwrap();
        assert!(k1.is_default, "the first key of a provider is its default");
        assert_eq!(
            (k1.org.as_str(), k1.secret_name.as_str()),
            (f.org.id.as_str(), "OPENAI_API_KEY")
        );
        assert_eq!(k1.created_by.as_ref().unwrap().id, "owner");
        let k2 = f
            .models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "openai", "backup"))
            .await
            .unwrap();
        assert!(!k2.is_default);
        let other = f
            .models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "anthropic", "ant"))
            .await
            .unwrap();
        assert!(other.is_default, "defaults are per provider");
        let mut third = key_req(&f.org, "openai", "third");
        third.is_default = true;
        third.endpoint = "https://llm.internal/v1".into();
        third.models = vec!["openai/gpt-x".into(), "openai/gpt-x".into()];
        let k3 = f
            .models
            .create_provider_key(&owner(), &f.org.id, third)
            .await
            .unwrap();
        assert!(k3.is_default);
        assert_eq!(k3.models, ["openai/gpt-x"]);
        let (keys, next) = f
            .models
            .list_provider_keys(&owner(), &f.org.id, "", 50)
            .await
            .unwrap();
        assert!(next.is_empty());
        let defaults: Vec<&str> = keys
            .iter()
            .filter(|k| k.is_default)
            .map(|k| k.name.as_str())
            .collect();
        assert_eq!(defaults, ["ant", "third"], "the new default replaced main");
        let (page, next) = f
            .models
            .list_provider_keys(&owner(), &f.org.id, "", 2)
            .await
            .unwrap();
        assert_eq!((page.len(), next.is_empty()), (2, false));
        let (rest, next) = f
            .models
            .list_provider_keys(&owner(), &f.org.id, &next, 2)
            .await
            .unwrap();
        assert_eq!((rest.len(), next.is_empty()), (2, true));

        // Validation.
        let e = |req: CreateModelProviderKeyRequest| {
            let m = f.models.clone();
            let org = f.org.id.clone();
            async move { err(m.create_provider_key(&owner(), &org, req).await) }
        };
        assert_eq!(
            e(key_req(&f.org, "openai", "main")).await.reason,
            ErrorReason::AlreadyExists
        );
        assert_eq!(
            e(key_req(&f.org, "OpenAI", "x")).await.subcode,
            "invalid_provider"
        );
        assert_eq!(
            e(key_req(&f.org, "openai", "")).await.subcode,
            "invalid_name"
        );
        let mut r = key_req(&f.org, "openai", "x");
        r.secret_name = "has space".into();
        assert_eq!(e(r).await.subcode, "invalid_secret_name");
        let mut r = key_req(&f.org, "openai", "x");
        r.endpoint = "ftp://x".into();
        assert_eq!(e(r).await.subcode, "bad_url");
        let mut r = key_req(&f.org, "openai", "x");
        r.models = vec!["anthropic/claude-haiku".into()];
        assert_eq!(
            e(r).await.subcode,
            "unknown_model",
            "a model of another provider"
        );
        let mut r = key_req(&f.org, "openai", "x");
        r.models = vec!["openai/missing".into()];
        assert_eq!(e(r).await.reason, ErrorReason::NotFound);

        // Deleting the default promotes the oldest remaining key of the provider.
        f.models
            .delete_provider_key(&owner(), &f.org.id, &k3.id)
            .await
            .unwrap();
        let (keys, _) = f
            .models
            .list_provider_keys(&owner(), &f.org.id, "", 50)
            .await
            .unwrap();
        let openai: Vec<(&str, bool)> = keys
            .iter()
            .filter(|k| k.provider == "openai")
            .map(|k| (k.name.as_str(), k.is_default))
            .collect();
        assert_eq!(openai, [("main", true), ("backup", false)]);
        assert_eq!(
            err(f
                .models
                .delete_provider_key(&owner(), &f.org.id, &k3.id)
                .await)
            .reason,
            ErrorReason::NotFound
        );
        assert_eq!(f.events("model.key_created").await.len(), 4);
        assert_eq!(f.events("model.key_deleted").await.len(), 1);
        // Keys of another org are invisible.
        let other_org = support::make_org(&f.env, &format!("o{}", f.org.name)).await;
        assert!(
            f.models
                .list_provider_keys(&owner(), &other_org.id, "", 50)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(
            err(f
                .models
                .delete_provider_key(&owner(), &other_org.id, &k1.id)
                .await)
            .reason,
            ErrorReason::NotFound
        );
    })
    .await;
}

// ---------- prompt file index ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_prompt_index_lists_gets_and_validates() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let rid = f.repo.id.clone();
        for (p, n) in [
            ("prompts/summarize.prompt.yml", "summarize"),
            ("prompts/translate.prompt.yml", "translate"),
            ("evals/judge.prompt.yaml", "judge"),
        ] {
            f.models
                .index_prompt_file(&owner(), &rid, prompt(p, n, "anthropic/claude-sonnet", 1))
                .await
                .unwrap();
        }
        let (all, next) = f
            .models
            .list_prompt_files(&owner(), &rid, b"", "", 50)
            .await
            .unwrap();
        let paths: Vec<String> = all
            .iter()
            .map(|p| String::from_utf8_lossy(&p.path).into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "evals/judge.prompt.yaml",
                "prompts/summarize.prompt.yml",
                "prompts/translate.prompt.yml"
            ]
        );
        assert!(next.is_empty());
        assert!(
            all.iter()
                .all(|p| p.messages.is_empty() && p.raw.is_empty()),
            "headers only"
        );
        assert_eq!(all[0].variables, ["persona", "question"]);
        assert_eq!(all[0].test_data_count, 4);
        let (some, _) = f
            .models
            .list_prompt_files(&owner(), &rid, b"prompts/", "", 50)
            .await
            .unwrap();
        assert_eq!(some.len(), 2);
        let (page, next) = f
            .models
            .list_prompt_files(&owner(), &rid, b"", "", 2)
            .await
            .unwrap();
        assert_eq!(
            (page.len(), next.as_str()),
            (2, "prompts/summarize.prompt.yml")
        );
        let (rest, next) = f
            .models
            .list_prompt_files(&owner(), &rid, b"", &next, 2)
            .await
            .unwrap();
        assert_eq!((rest.len(), next.is_empty()), (1, true));

        let with_raw = f
            .models
            .get_prompt_file(&owner(), &rid, b"prompts/summarize.prompt.yml", true)
            .await
            .unwrap();
        assert!(with_raw.raw.starts_with("name: summarize"));
        assert_eq!(with_raw.messages.len(), 2);
        let bare = f
            .models
            .get_prompt_file(&owner(), &rid, b"prompts/summarize.prompt.yml", false)
            .await
            .unwrap();
        assert!(bare.raw.is_empty() && bare.messages.len() == 2);
        assert_eq!(
            err(f
                .models
                .get_prompt_file(&owner(), &rid, b"nope.prompt.yml", false)
                .await)
            .reason,
            ErrorReason::NotFound
        );
        // Another repo has its own index.
        let other = support::make_repo(&f.env, &f.org, "other").await;
        assert!(
            f.models
                .list_prompt_files(&owner(), &other.id, b"", "", 50)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // A new revision replaces the entry.
        let mut v2 = prompt(
            "prompts/summarize.prompt.yml",
            "summarize v2",
            "openai/gpt-x",
            2,
        );
        v2.test_data_count = 9;
        f.models
            .index_prompt_file(&owner(), &rid, v2)
            .await
            .unwrap();
        let got = f
            .models
            .get_prompt_file(&owner(), &rid, b"prompts/summarize.prompt.yml", false)
            .await
            .unwrap();
        assert_eq!(
            (got.name.as_str(), got.model.as_str(), got.test_data_count),
            ("summarize v2", "openai/gpt-x", 9)
        );
        assert_eq!(got.blob.unwrap().raw, vec![2u8; 32]);
        f.models
            .remove_prompt_file(&owner(), &rid, b"evals/judge.prompt.yaml")
            .await
            .unwrap();
        assert_eq!(
            f.models
                .list_prompt_files(&owner(), &rid, b"", "", 50)
                .await
                .unwrap()
                .0
                .len(),
            2
        );
        assert_eq!(
            err(f
                .models
                .remove_prompt_file(&owner(), &rid, b"evals/judge.prompt.yaml")
                .await)
            .reason,
            ErrorReason::NotFound
        );
        assert_eq!(f.events("model.prompt_indexed").await.len(), 5);

        // Malformed registrations.
        let bad = |edit: &dyn Fn(&mut PromptFile)| {
            let mut p = prompt("prompts/x.prompt.yml", "x", "anthropic/claude-sonnet", 3);
            edit(&mut p);
            let (m, rid) = (f.models.clone(), rid.clone());
            async move { err(m.index_prompt_file(&owner(), &rid, p).await) }
        };
        for (what, e) in [
            ("empty name", bad(&|p| p.name = "  ".into()).await),
            ("empty model", bad(&|p| p.model.clear()).await),
            (
                "duplicate variables",
                bad(&|p| p.variables = vec!["a".into(), "a".into()]).await,
            ),
            (
                "variable not an identifier",
                bad(&|p| p.variables = vec!["a b".into()]).await,
            ),
            ("no messages", bad(&|p| p.messages.clear()).await),
            (
                "message without a role",
                bad(&|p| p.messages[0].role = 0).await,
            ),
            (
                "not a prompt path",
                bad(&|p| p.path = b"docs/readme.md".to_vec()).await,
            ),
            (
                "escaping path",
                bad(&|p| p.path = b"../x.prompt.yml".to_vec()).await,
            ),
            (
                "absolute path",
                bad(&|p| p.path = b"/x.prompt.yml".to_vec()).await,
            ),
            ("binary path", bad(&|p| p.path = vec![0xff, 0xfe]).await),
            ("no blob", bad(&|p| p.blob = None).await),
            (
                "hot temperature",
                bad(&|p| p.params.as_mut().unwrap().temperature = Some(3.0)).await,
            ),
            (
                "duplicate evaluators",
                bad(&|p| p.evaluators = vec!["a".into(), "a".into()]).await,
            ),
        ] {
            assert_eq!(e.reason, ErrorReason::InvalidArgument, "{what}");
            assert_eq!(e.subcode, "bad_prompt_file", "{what}");
        }
        let big = bad(&|p| p.raw = "x".repeat(300 * 1024)).await;
        assert_eq!(big.reason, ErrorReason::TooLarge);
        // Nothing malformed was stored.
        assert_eq!(
            f.models
                .list_prompt_files(&owner(), &rid, b"", "", 50)
                .await
                .unwrap()
                .0
                .len(),
            2
        );
    })
    .await;
}

// ---------- evals ----------

fn cs(n: u8) -> ObjectId {
    ObjectId { raw: vec![n; 32] }
}

fn eval_req(path: &str) -> RunPromptEvalRequest {
    RunPromptEvalRequest {
        prompt_path: path.as_bytes().to_vec(),
        ..Default::default()
    }
}

fn score(name: &str, mean: f64) -> EvalScore {
    EvalScore {
        evaluator: name.into(),
        mean,
        passed: 3,
        failed: 1,
    }
}

async fn eval_fixture(f: &Fx) {
    f.seed_catalog().await;
    f.models
        .index_prompt_file(
            &owner(),
            &f.repo.id,
            prompt(
                "prompts/a.prompt.yml",
                "summarize",
                "anthropic/claude-sonnet",
                1,
            ),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_eval_lifecycle_and_check_reporting() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        eval_fixture(&f).await;
        let rid = f.repo.id.clone();

        // Queued from the index; the model defaults to the prompt's.
        let q = f
            .models
            .run_prompt_eval(&owner(), &f.repo, None, &eval_req("prompts/a.prompt.yml"))
            .await
            .unwrap();
        assert_eq!(q.state, EvalRunState::Queued as i32);
        assert_eq!(q.models, ["anthropic/claude-sonnet"]);
        assert_eq!((q.cases_total, q.cases_done), (4, 0));
        assert_eq!(q.started_by.as_ref().unwrap().id, "owner");
        assert_eq!(
            q.repo,
            Some(RepoRef {
                repo: Some(repo_ref::Repo::Id(rid.clone()))
            })
        );
        assert!(q.created_at.is_some() && q.finished_at.is_none());
        assert_eq!(
            f.models.get_eval_run(&owner(), &rid, &q.id).await.unwrap(),
            q
        );

        // Not finished before it started.
        let e = err(f
            .models
            .record_eval_result(&owner(), &rid, &q.id, EvalResult::default())
            .await);
        assert_eq!(e.reason, ErrorReason::InvalidState);
        let r = f
            .models
            .record_eval_progress(&owner(), &rid, &q.id, None, 2)
            .await
            .unwrap();
        assert_eq!((r.state, r.cases_done), (EvalRunState::Running as i32, 2));
        assert_eq!(
            err(f
                .models
                .record_eval_progress(&owner(), &rid, &q.id, None, 1)
                .await)
            .subcode,
            "invalid_progress"
        );
        assert_eq!(
            err(f
                .models
                .record_eval_progress(&owner(), &rid, &q.id, None, 9)
                .await)
            .subcode,
            "invalid_progress"
        );
        f.clock.advance(HOUR);
        let done = f
            .models
            .record_eval_result(
                &owner(),
                &rid,
                &q.id,
                EvalResult {
                    scores: vec![score("exact", 0.5), score("similarity", 0.9)],
                    cases_total: 4,
                    cases_done: 4,
                    cost: Some(Cost {
                        input_tokens: 1000,
                        output_tokens: 400,
                        usd_micros: 9_000,
                    }),
                    results_url: "https://results.example/r.json".into(),
                    error: String::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(done.state, EvalRunState::Succeeded as i32);
        assert_eq!(done.scores.len(), 2);
        assert_eq!(done.cost.as_ref().unwrap().usd_micros, 9_000);
        assert!(done.finished_at.is_some());
        assert!(done.check_run_id.is_empty(), "no check was requested");
        // A finished run takes no more reports and cannot be cancelled.
        assert_eq!(
            err(f
                .models
                .record_eval_progress(&owner(), &rid, &q.id, None, 4)
                .await)
            .reason,
            ErrorReason::InvalidState
        );
        assert_eq!(
            err(f
                .models
                .record_eval_result(&owner(), &rid, &q.id, EvalResult::default())
                .await)
            .reason,
            ErrorReason::InvalidState
        );
        assert_eq!(
            err(f.models.cancel_eval_run(&owner(), &rid, &q.id).await).reason,
            ErrorReason::InvalidState
        );
        assert_eq!(
            f.models
                .get_eval_run(&owner(), &rid, &q.id)
                .await
                .unwrap()
                .state,
            EvalRunState::Succeeded as i32
        );

        // A failing run.
        let failing = f
            .models
            .run_prompt_eval(&owner(), &f.repo, None, &eval_req("prompts/a.prompt.yml"))
            .await
            .unwrap();
        f.models
            .record_eval_progress(&owner(), &rid, &failing.id, Some(4), 0)
            .await
            .unwrap();
        let failed = f
            .models
            .record_eval_result(
                &owner(),
                &rid,
                &failing.id,
                EvalResult {
                    error: "provider unavailable".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (failed.state, failed.error.as_str()),
            (EvalRunState::Failed as i32, "provider unavailable")
        );

        // Cancelling queued and running runs.
        let queued = f
            .models
            .run_prompt_eval(&owner(), &f.repo, None, &eval_req("prompts/a.prompt.yml"))
            .await
            .unwrap();
        let c = f
            .models
            .cancel_eval_run(&owner(), &rid, &queued.id)
            .await
            .unwrap();
        assert_eq!(c.state, EvalRunState::Cancelled as i32);
        assert!(c.finished_at.is_some());
        assert_eq!(
            err(f.models.cancel_eval_run(&owner(), &rid, &queued.id).await).reason,
            ErrorReason::InvalidState
        );
        assert_eq!(
            err(f
                .models
                .record_eval_progress(&owner(), &rid, &queued.id, None, 1)
                .await)
            .reason,
            ErrorReason::InvalidState
        );
        let running = f
            .models
            .run_prompt_eval(&owner(), &f.repo, None, &eval_req("prompts/a.prompt.yml"))
            .await
            .unwrap();
        f.models
            .record_eval_progress(&owner(), &rid, &running.id, None, 1)
            .await
            .unwrap();
        assert_eq!(
            f.models
                .cancel_eval_run(&owner(), &rid, &running.id)
                .await
                .unwrap()
                .state,
            EvalRunState::Cancelled as i32
        );
        assert_eq!(
            err(f.models.cancel_eval_run(&owner(), &rid, "nope").await).reason,
            ErrorReason::NotFound
        );

        // Listing: newest first, filtered, paged.
        let all = |filter: EvalFilter, after: &str, limit: usize| {
            let (m, rid, after) = (f.models.clone(), rid.clone(), after.to_string());
            async move {
                m.list_eval_runs(&owner(), &rid, &filter, &after, limit)
                    .await
                    .unwrap()
            }
        };
        let (runs, next) = all(EvalFilter::default(), "", 50).await;
        assert_eq!(
            runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [
                running.id.as_str(),
                queued.id.as_str(),
                failing.id.as_str(),
                q.id.as_str()
            ]
        );
        assert!(next.is_empty());
        let (cancelled, _) = all(
            EvalFilter {
                state: EvalRunState::Cancelled,
                ..Default::default()
            },
            "",
            50,
        )
        .await;
        assert_eq!(cancelled.len(), 2);
        let (page, next) = all(EvalFilter::default(), "", 3).await;
        assert_eq!((page.len(), next.as_str()), (3, failing.id.as_str()));
        let (rest, next) = all(EvalFilter::default(), &next, 3).await;
        assert_eq!((rest.len(), next.is_empty()), (1, true));
        let (by_path, _) = all(
            EvalFilter {
                prompt_path: b"prompts/a.prompt.yml".to_vec(),
                ..Default::default()
            },
            "",
            50,
        )
        .await;
        assert_eq!(by_path.len(), 4);
        let (none, _) = all(
            EvalFilter {
                prompt_path: b"prompts/zzz.prompt.yml".to_vec(),
                ..Default::default()
            },
            "",
            50,
        )
        .await;
        assert!(none.is_empty());

        // Check reporting on a changeset: passes when every evaluator reaches the threshold.
        let mut with_check = eval_req("prompts/a.prompt.yml");
        with_check.report_check = true;
        with_check.pass_threshold = 0.8;
        let pass = f
            .models
            .run_prompt_eval(&owner(), &f.repo, Some(cs(7)), &with_check)
            .await
            .unwrap();
        f.models
            .record_eval_progress(&owner(), &rid, &pass.id, None, 4)
            .await
            .unwrap();
        let pass = f
            .models
            .record_eval_result(
                &owner(),
                &rid,
                &pass.id,
                EvalResult {
                    scores: vec![score("exact", 0.9), score("similarity", 0.85)],
                    cases_total: 4,
                    cases_done: 4,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!pass.check_run_id.is_empty());
        let filter = RunFilter {
            changeset_hex: Some(hex(&[7u8; 32])),
            ..Default::default()
        };
        let checks = f
            .env
            .forge
            .checks
            .list(&owner(), &rid, &filter)
            .await
            .unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].id, pass.check_run_id);
        assert_eq!(checks[0].name, "prompt-eval: summarize");
        assert_eq!(checks[0].status, CheckStatus::Passed as i32);
        assert!(checks[0].summary.contains("exact: mean 0.900"));

        let mut strict = with_check.clone();
        strict.pass_threshold = 0.95;
        strict.check_name = "eval/strict".into();
        let miss = f
            .models
            .run_prompt_eval(&owner(), &f.repo, Some(cs(8)), &strict)
            .await
            .unwrap();
        f.models
            .record_eval_progress(&owner(), &rid, &miss.id, None, 4)
            .await
            .unwrap();
        let miss = f
            .models
            .record_eval_result(
                &owner(),
                &rid,
                &miss.id,
                EvalResult {
                    scores: vec![score("exact", 0.9)],
                    cases_total: 4,
                    cases_done: 4,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let c = f
            .env
            .forge
            .checks
            .get(&owner(), &rid, &miss.check_run_id)
            .await
            .unwrap();
        assert_eq!(
            (c.name.as_str(), c.status),
            ("eval/strict", CheckStatus::Failed as i32)
        );

        let broke = f
            .models
            .run_prompt_eval(&owner(), &f.repo, Some(cs(9)), &with_check)
            .await
            .unwrap();
        f.models
            .record_eval_progress(&owner(), &rid, &broke.id, None, 0)
            .await
            .unwrap();
        let broke = f
            .models
            .record_eval_result(
                &owner(),
                &rid,
                &broke.id,
                EvalResult {
                    error: "timeout".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let c = f
            .env
            .forge
            .checks
            .get(&owner(), &rid, &broke.check_run_id)
            .await
            .unwrap();
        assert_eq!(c.status, CheckStatus::Failed as i32);
        assert!(c.summary.contains("timeout"));
        // By changeset.
        let (by_cs, _) = all(
            EvalFilter {
                changeset: Some(cs(8)),
                ..Default::default()
            },
            "",
            50,
        )
        .await;
        assert_eq!(by_cs.len(), 1);
        assert_eq!(by_cs[0].id, miss.id);

        // Validation.
        let run = |req: RunPromptEvalRequest, cs: Option<ObjectId>| {
            let (m, repo) = (f.models.clone(), f.repo.clone());
            async move { err(m.run_prompt_eval(&owner(), &repo, cs, &req).await) }
        };
        let mut r = eval_req("prompts/a.prompt.yml");
        r.pass_threshold = 1.5;
        assert_eq!(run(r.clone(), None).await.subcode, "invalid_threshold");
        r.pass_threshold = f64::NAN;
        assert_eq!(run(r, None).await.subcode, "invalid_threshold");
        let mut r = eval_req("prompts/a.prompt.yml");
        r.pass_threshold = -0.1;
        assert_eq!(run(r, None).await.subcode, "invalid_threshold");
        let mut r = eval_req("prompts/a.prompt.yml");
        r.models = vec!["anthropic/claude-haiku".into(), "nope/model".into()];
        assert_eq!(run(r, None).await.reason, ErrorReason::NotFound);
        assert_eq!(
            run(eval_req("prompts/missing.prompt.yml"), None)
                .await
                .reason,
            ErrorReason::NotFound
        );
        assert_eq!(run(eval_req(""), None).await.subcode, "missing_field");
        let mut r = eval_req("prompts/a.prompt.yml");
        r.report_check = true;
        assert_eq!(run(r, None).await.subcode, "changeset_required");
        let mut r = eval_req("prompts/a.prompt.yml");
        r.models = vec![
            "anthropic/claude-haiku".into(),
            "openai/gpt-x".into(),
            "anthropic/claude-haiku".into(),
        ];
        let multi = f
            .models
            .run_prompt_eval(&owner(), &f.repo, None, &r)
            .await
            .unwrap();
        assert_eq!(
            multi.models,
            ["anthropic/claude-haiku", "openai/gpt-x"],
            "deduplicated, in order"
        );
        let mut r = eval_req("prompts/a.prompt.yml");
        r.dataset_path = b"evals/data.jsonl".to_vec();
        assert_eq!(
            f.models
                .run_prompt_eval(&owner(), &f.repo, None, &r)
                .await
                .unwrap()
                .cases_total,
            0,
            "unknown until the executor reports"
        );
        assert!(f.events("model.eval_queued").await.len() >= 8);
        assert!(f.events("model.eval_updated").await.len() >= 12);
    })
    .await;
}

// ---------- usage ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_usage_aggregates_per_model_and_principal() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        let u = |model: &str, who: &str, id: &str, byok: bool, usd: u64, at: u64| InferenceUsage {
            org_id: f.org.id.clone(),
            principal_id: who.into(),
            model: model.into(),
            request_id: id.into(),
            requests: 1,
            input_tokens: 1000,
            output_tokens: 200,
            usd_micros: usd,
            byok,
            at_ms: at,
        };
        let s = "anthropic/claude-sonnet";
        assert!(
            f.models
                .record_inference(&owner(), u(s, "owner", "r1", false, 5_000, T0))
                .await
                .unwrap()
        );
        assert!(
            !f.models
                .record_inference(&owner(), u(s, "owner", "r1", false, 5_000, T0))
                .await
                .unwrap(),
            "the gateway's request id dedupes retries"
        );
        f.models
            .record_inference(&owner(), u(s, "owner", "r2", false, 7_000, T0 + HOUR))
            .await
            .unwrap();
        f.models
            .record_inference(&owner(), u(s, "reader", "r3", false, 1_000, T0 + 2 * HOUR))
            .await
            .unwrap();
        f.models
            .record_inference(
                &owner(),
                u("openai/gpt-x", "reader", "r4", true, 9_000, T0 + DAY),
            )
            .await
            .unwrap();
        let mut batch = u(s, "owner", "", false, 100, T0 + DAY);
        batch.requests = 0; // counts as one
        f.models.record_inference(&owner(), batch).await.unwrap();

        // Per model.
        let all = f
            .models
            .usage(&owner(), &f.org.id, "", None, None, false)
            .await
            .unwrap();
        assert_eq!(all.org, f.org.id);
        let lines: Vec<(&str, u64, bool, u64)> = all
            .lines
            .iter()
            .map(|l| {
                (
                    l.model.as_str(),
                    l.requests,
                    l.byok,
                    l.cost.as_ref().unwrap().usd_micros,
                )
            })
            .collect();
        assert_eq!(
            lines,
            [(s, 4, false, 13_100), ("openai/gpt-x", 1, true, 9_000)]
        );
        assert!(all.lines.iter().all(|l| l.principal_id.is_empty()));
        let t = all.total.unwrap();
        assert_eq!(
            (t.input_tokens, t.output_tokens, t.usd_micros),
            (5000, 1000, 22_100)
        );
        assert!(all.window_start.is_none());
        // Per principal.
        let by_who = f
            .models
            .usage(&owner(), &f.org.id, "", None, None, true)
            .await
            .unwrap();
        let lines: Vec<(&str, &str, u64)> = by_who
            .lines
            .iter()
            .map(|l| (l.model.as_str(), l.principal_id.as_str(), l.requests))
            .collect();
        assert_eq!(
            lines,
            [
                (s, "owner", 3),
                (s, "reader", 1),
                ("openai/gpt-x", "reader", 1)
            ]
        );
        // One principal, and a window [start, end).
        let reader = f
            .models
            .usage(&owner(), &f.org.id, "reader", None, None, false)
            .await
            .unwrap();
        assert_eq!(reader.total.unwrap().usd_micros, 10_000);
        assert_eq!(reader.lines.len(), 2);
        let win = f
            .models
            .usage(
                &owner(),
                &f.org.id,
                "",
                Some(T0 + HOUR),
                Some(T0 + DAY),
                false,
            )
            .await
            .unwrap();
        assert_eq!(win.total.unwrap().usd_micros, 8_000);
        assert_eq!(
            win.window_start.unwrap().seconds,
            ((T0 + HOUR) / 1000) as i64
        );
        assert_eq!(
            err(f
                .models
                .usage(&owner(), &f.org.id, "", Some(T0 + DAY), Some(T0), false)
                .await)
            .subcode,
            "invalid_window"
        );
        // The org budget.
        assert_eq!(all.budget_usd_micros, 0);
        f.models
            .set_org_policy(
                &owner(),
                &f.org.id,
                OrgModelPolicy {
                    allowed_models: None,
                    budget_usd_micros: 50_000,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            f.models
                .usage(&owner(), &f.org.id, "", None, None, false)
                .await
                .unwrap()
                .budget_usd_micros,
            50_000
        );
        // Another org has none of it.
        let other = support::make_org(&f.env, &format!("x{}", f.org.name)).await;
        assert!(
            f.models
                .usage(&owner(), &other.id, "", None, None, false)
                .await
                .unwrap()
                .lines
                .is_empty()
        );
        assert_eq!(
            err(f
                .models
                .record_inference(
                    &owner(),
                    InferenceUsage {
                        model: "m".into(),
                        ..Default::default()
                    }
                )
                .await)
            .subcode,
            "missing_field"
        );
    })
    .await;
}

// ---------- admission ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_admission_applies_policy_byok_and_budget() {
    on_backends(|store, clock| async move {
        let f = fx(store, clock).await;
        f.seed_catalog().await;
        let org = f.org.id.clone();
        let sonnet = "anthropic/claude-sonnet";

        // Gateway-paid by default; 1000 in + 1000 out tokens at 3 and 15 USD per Mtok.
        let r = f.admit(&org, sonnet, "").await.unwrap();
        assert!(!r.byok && r.provider_key_id.is_empty());
        assert_eq!(r.estimated_usd_micros, 3_000 + 15_000);
        // 0 output tokens means the model's own limit (8000).
        let r = f
            .models
            .admit_inference(&owner(), &org, sonnet, "", InferenceEstimate::default())
            .await
            .unwrap();
        assert_eq!(r.estimated_usd_micros, 8_000 * 15);
        assert_eq!(
            err(f.admit(&org, "nope/model", "").await).reason,
            ErrorReason::NotFound
        );
        assert_eq!(err(f.admit("", sonnet, "").await).subcode, "missing_field");
        assert_eq!(err(f.admit(&org, "", "").await).subcode, "missing_field");

        // BYOK-only models need a key.
        let e = err(f.admit(&org, "acme/private-embed", "").await);
        assert_eq!(
            (e.reason, e.subcode.as_str()),
            (ErrorReason::InvalidState, "byok_key_required")
        );
        let key = f
            .models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "acme", "acme"))
            .await
            .unwrap();
        let r = f.admit(&org, "acme/private-embed", "").await.unwrap();
        assert!(r.byok);
        assert_eq!(r.provider_key_id, key.id);
        assert_eq!(r.estimated_usd_micros, 0);
        // A named key must exist and serve the model.
        assert_eq!(
            err(f.admit(&org, sonnet, "nope").await).reason,
            ErrorReason::NotFound
        );
        assert_eq!(
            err(f.admit(&org, sonnet, &key.id).await).subcode,
            "key_does_not_cover_model"
        );
        let mut scoped = key_req(&f.org, "anthropic", "ant scoped");
        scoped.models = vec!["anthropic/claude-haiku".into()];
        let ant = f
            .models
            .create_provider_key(&owner(), &f.org.id, scoped)
            .await
            .unwrap();
        assert!(
            !f.admit(&org, sonnet, "").await.unwrap().byok,
            "the default key does not cover sonnet"
        );
        assert!(
            f.admit(&org, "anthropic/claude-haiku", "")
                .await
                .unwrap()
                .byok
        );
        assert_eq!(
            f.admit(&org, "anthropic/claude-haiku", &ant.id)
                .await
                .unwrap()
                .provider_key_id,
            ant.id
        );

        // The org allow list.
        f.models
            .set_org_policy(
                &owner(),
                &org,
                OrgModelPolicy {
                    allowed_models: Some(vec!["anthropic/claude-haiku".into()]),
                    budget_usd_micros: 40_000,
                },
            )
            .await
            .unwrap();
        let e = err(f.admit(&org, sonnet, "").await);
        assert_eq!(e.reason, ErrorReason::PolicyDenied);
        assert!(
            f.admit(&org, "anthropic/claude-haiku", "")
                .await
                .unwrap()
                .byok
        );
        f.models
            .set_org_policy(
                &owner(),
                &org,
                OrgModelPolicy {
                    allowed_models: None,
                    budget_usd_micros: 40_000,
                },
            )
            .await
            .unwrap();

        // The org's monthly budget counts gateway-paid usage only.
        let usage = |usd: u64, byok: bool, id: &str| InferenceUsage {
            org_id: org.clone(),
            principal_id: "owner".into(),
            model: sonnet.into(),
            request_id: id.into(),
            requests: 1,
            usd_micros: usd,
            byok,
            ..Default::default()
        };
        f.models
            .record_inference(&owner(), usage(20_000, false, "a"))
            .await
            .unwrap();
        f.models
            .record_inference(&owner(), usage(900_000, true, "b"))
            .await
            .unwrap();
        f.admit(&org, sonnet, "").await.unwrap(); // 20k + 18k <= 40k
        f.models
            .record_inference(&owner(), usage(5_000, false, "c"))
            .await
            .unwrap();
        let e = err(f.admit(&org, sonnet, "").await); // 25k + 18k > 40k
        assert_eq!(
            (e.reason, e.subcode.as_str()),
            (ErrorReason::QuotaExceeded, "model_budget_exhausted")
        );
        assert!(
            f.admit(&org, "anthropic/claude-haiku", "")
                .await
                .unwrap()
                .byok,
            "own keys are not budgeted"
        );
        // A new month starts again.
        f.clock.set(T0 + 15 * DAY);
        f.admit(&org, sonnet, "").await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_admission_asks_billing_for_agent_token_budgets() {
    on_backends(|store, clock| async move {
        let f = fx_with(store, clock, true).await;
        f.seed_catalog().await;
        let org = f.org.id.clone();
        let sonnet = "anthropic/claude-sonnet";
        let billing = Billing::new(f.env.forge.clone(), BillingConfig::default());
        let scope = OrgScopeResolver::new(f.env.forge.clone())
            .resolve_org(&owner(), &f.org.name)
            .await
            .unwrap();
        // The models in `f` hold their own Billing as the gate; budgets live in the same store.
        billing
            .create_budget(
                &owner(),
                &scope,
                CreateBillingBudgetRequest {
                    name: "agent tokens".into(),
                    product: BillingProduct::AgentTokens as i32,
                    monthly_amount_micro_usd: 1_000_000,
                    hard_stop: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.admit(&org, sonnet, "").await.unwrap();
        // 400k input tokens at 3 micro-USD each: 1.2M micro-USD over the free plan's 100k
        // included tokens is 900k billable.
        billing
            .record_usage(
                &owner(),
                UsageInput {
                    org_id: org.clone(),
                    principal_id: "owner".into(),
                    product: BillingProduct::AgentTokens,
                    sku: "agent_tokens_input".into(),
                    quantity: 400_000.0,
                    unit: "token".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.admit(&org, sonnet, "").await.unwrap(); // 900k + 18k fits
        let o = billing
            .record_usage(
                &owner(),
                UsageInput {
                    org_id: org.clone(),
                    product: BillingProduct::AgentTokens,
                    sku: "agent_tokens_output".into(),
                    quantity: 10_000.0,
                    unit: "token".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(o.limit_hit);
        let e = err(f.admit(&org, sonnet, "").await);
        assert_eq!(
            (e.reason, e.subcode.as_str()),
            (ErrorReason::QuotaExceeded, "budget_exhausted")
        );
        // Own keys are not metered by the gateway.
        f.models
            .create_provider_key(&owner(), &f.org.id, key_req(&f.org, "anthropic", "mine"))
            .await
            .unwrap();
        assert!(f.admit(&org, sonnet, "").await.unwrap().byok);
    })
    .await;
}

// ---------- ModelService ----------

struct Svc {
    f: Fx,
    svc: ModelsService,
}

async fn svc() -> Svc {
    let f = fx(
        Arc::new(bee_forge::store::memory::MemoryStore::new()),
        backends::clock(),
    )
    .await;
    let svc = ModelsService::new(f.models.clone(), ForgeServices::new(f.env.forge.clone()));
    Svc { f, svc }
}

fn detail(st: &tonic::Status) -> (String, String) {
    let d = bee_proto::errors::detail_from_status(st).expect("detail");
    (d.reason, d.subcode)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_service_permission_matrix_and_all_fourteen_rpcs() {
    let s = svc().await;
    let (f, svc) = (&s.f, &s.svc);
    f.seed_catalog().await;
    f.models
        .index_prompt_file(
            &owner(),
            &f.repo.id,
            prompt(
                "prompts/a.prompt.yml",
                "summarize",
                "anthropic/claude-sonnet",
                1,
            ),
        )
        .await
        .unwrap();
    let (owner_t, admin_t, reader_t, outsider_t) = (
        f.env.full("owner"),
        f.env.full("admin"),
        f.env.full("reader"),
        f.env.full("outsider"),
    );
    let repo = Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(f.repo.full_name.clone())),
    });
    let org = f.org.name.clone();

    // Catalog.
    let l = svc
        .list_models(common::req(
            &reader_t,
            ListModelsRequest {
                org: org.clone(),
                provider: "anthropic".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.models.len(), 2);
    assert!(l.models.iter().all(|m| m.enabled_for_org));
    assert_eq!(
        common::reason(
            svc.list_models(common::req(
                &outsider_t,
                ListModelsRequest {
                    org: org.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    let all = svc
        .list_models(common::req(
            &outsider_t,
            ListModelsRequest {
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: String::new(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        all.models.len(),
        2,
        "the bare catalog is open to any caller"
    );
    assert!(!all.page.unwrap().next_page_token.is_empty());
    let m = svc
        .get_model(common::req(
            &reader_t,
            GetModelRequest {
                org: org.clone(),
                id: "openai/gpt-x".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(m.id, "openai/gpt-x");
    assert_eq!(
        common::reason(
            svc.get_model(common::req(
                &reader_t,
                GetModelRequest {
                    org: String::new(),
                    id: "no/pe".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );

    // Provider keys: owners and admins only.
    for (t, name) in [(&owner_t, "k-owner"), (&admin_t, "k-admin")] {
        svc.create_model_provider_key(common::req(
            t,
            CreateModelProviderKeyRequest {
                org: org.clone(),
                provider: "openai".into(),
                name: name.into(),
                secret_name: "OPENAI_API_KEY".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    }
    for t in [&reader_t, &outsider_t] {
        assert_eq!(
            common::reason(
                svc.create_model_provider_key(common::req(
                    t,
                    CreateModelProviderKeyRequest {
                        org: org.clone(),
                        provider: "openai".into(),
                        name: "x".into(),
                        secret_name: "S".into(),
                        ..Default::default()
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
        assert_eq!(
            common::reason(
                svc.list_model_provider_keys(common::req(
                    t,
                    ListModelProviderKeysRequest {
                        org: org.clone(),
                        page: None
                    }
                ))
                .await
            ),
            "PERMISSION_DENIED"
        );
    }
    let keys = svc
        .list_model_provider_keys(common::req(
            &admin_t,
            ListModelProviderKeysRequest {
                org: org.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(keys.keys.len(), 2);
    assert_eq!(
        common::reason(
            svc.create_model_provider_key(common::req(
                &owner_t,
                CreateModelProviderKeyRequest {
                    org: String::new(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        common::reason(
            svc.delete_model_provider_key(common::req(
                &reader_t,
                DeleteModelProviderKeyRequest {
                    org: org.clone(),
                    id: keys.keys[0].id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    svc.delete_model_provider_key(common::req(
        &admin_t,
        DeleteModelProviderKeyRequest {
            org: org.clone(),
            id: keys.keys[1].id.clone(),
        },
    ))
    .await
    .unwrap();
    f.env.forge.flush().await.unwrap();
    let (entries, _) = f
        .env
        .forge
        .audit
        .query(
            &owner(),
            &bee_forge::audit::org_scope(&f.org.id),
            &Default::default(),
            true,
            "",
            200,
        )
        .await
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.action == "model.key.create" && e.result == "PERMISSION_DENIED"),
        "denied key changes are audited"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.action == "model.key.create" && e.result == "ok")
    );

    // Prompt files and evals: reads for readers, runs need write access.
    let pf = svc
        .list_prompt_files(common::req(
            &reader_t,
            ListPromptFilesRequest {
                repo: repo.clone(),
                path_prefix: b"prompts/".to_vec(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(pf.prompt_files.len(), 1);
    let one = svc
        .get_prompt_file(common::req(
            &reader_t,
            GetPromptFileRequest {
                repo: repo.clone(),
                path: b"prompts/a.prompt.yml".to_vec(),
                include_raw: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!one.raw.is_empty());
    assert_eq!(
        common::reason(
            svc.get_prompt_file(common::req(
                &reader_t,
                GetPromptFileRequest {
                    repo: repo.clone(),
                    path: b"x.prompt.yml".to_vec(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        common::reason(
            svc.list_prompt_files(common::req(
                &outsider_t,
                ListPromptFilesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let run_req = RunPromptEvalRequest {
        repo: repo.clone(),
        revision: Some(Revision {
            expr: hex(&[5u8; 32]),
        }),
        prompt_path: b"prompts/a.prompt.yml".to_vec(),
        report_check: true,
        pass_threshold: 0.5,
        ..Default::default()
    };
    assert_eq!(
        common::reason(
            svc.run_prompt_eval(common::req(&reader_t, run_req.clone()))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let run = svc
        .run_prompt_eval(common::req(&owner_t, run_req.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(run.state, EvalRunState::Queued as i32);
    assert_eq!(run.changeset, Some(ObjectId { raw: vec![5u8; 32] }));
    let got = svc
        .get_eval_run(common::req(
            &reader_t,
            GetEvalRunRequest {
                repo: repo.clone(),
                id: run.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.id, run.id);
    let listed = svc
        .list_eval_runs(common::req(
            &reader_t,
            ListEvalRunsRequest {
                repo: repo.clone(),
                changeset: Some(ObjectId { raw: vec![5u8; 32] }),
                state: EvalRunState::Queued as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.runs.len(), 1);
    assert_eq!(
        common::reason(
            svc.cancel_eval_run(common::req(
                &reader_t,
                CancelEvalRunRequest {
                    repo: repo.clone(),
                    id: run.id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    let c = svc
        .cancel_eval_run(common::req(
            &owner_t,
            CancelEvalRunRequest {
                repo: repo.clone(),
                id: run.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.state, EvalRunState::Cancelled as i32);
    assert_eq!(
        common::reason(
            svc.cancel_eval_run(common::req(
                &owner_t,
                CancelEvalRunRequest {
                    repo: repo.clone(),
                    id: run.id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let mut bad_rev = run_req.clone();
    bad_rev.revision = Some(Revision {
        expr: "main".into(),
    });
    assert_eq!(
        common::reason(svc.run_prompt_eval(common::req(&owner_t, bad_rev)).await),
        "INVALID_ARGUMENT"
    );

    // Usage: one's own as a member, anyone's as an admin.
    f.models
        .record_inference(
            &owner(),
            InferenceUsage {
                org_id: f.org.id.clone(),
                principal_id: "reader".into(),
                model: "anthropic/claude-sonnet".into(),
                requests: 2,
                usd_micros: 40,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mine = svc
        .get_model_usage(common::req(
            &reader_t,
            GetModelUsageRequest {
                org: org.clone(),
                principal_id: "reader".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.total.unwrap().usd_micros, 40);
    assert_eq!(
        common::reason(
            svc.get_model_usage(common::req(
                &reader_t,
                GetModelUsageRequest {
                    org: org.clone(),
                    principal_id: "owner".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(
            svc.get_model_usage(common::req(
                &reader_t,
                GetModelUsageRequest {
                    org: org.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    let all = svc
        .get_model_usage(common::req(
            &admin_t,
            GetModelUsageRequest {
                org: org.clone(),
                group_by_principal: true,
                window_start: Some(bee_forge::ids::ts(T0)),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.lines.len(), 1);
    assert_eq!(all.lines[0].principal_id, "reader");
    assert!(all.window_start.is_some());
    assert_eq!(
        common::reason(
            svc.get_model_usage(common::req(
                &outsider_t,
                GetModelUsageRequest {
                    org: org.clone(),
                    principal_id: "outsider".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );

    // Inference: admitted requests are served by the gateway process.
    let msg = vec![ModelMessage {
        role: ModelMessageRole::User as i32,
        content: "hello".repeat(100),
        ..Default::default()
    }];
    let run_inf = |model: &str| RunModelInferenceRequest {
        org: org.clone(),
        model: model.into(),
        messages: msg.clone(),
        ..Default::default()
    };
    let st = svc
        .run_model_inference(common::req(&reader_t, run_inf("anthropic/claude-sonnet")))
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unimplemented);
    assert_eq!(
        detail(&st),
        ("UNIMPLEMENTED".to_string(), "served_elsewhere".to_string())
    );
    let st = svc
        .stream_model_inference(common::req(
            &reader_t,
            StreamModelInferenceRequest {
                org: org.clone(),
                model: "anthropic/claude-sonnet".into(),
                messages: msg.clone(),
                ..Default::default()
            },
        ))
        .await
        .err()
        .expect("streams are served elsewhere");
    assert_eq!(detail(&st).1, "served_elsewhere");
    assert_eq!(
        common::reason(
            svc.run_model_inference(common::req(&outsider_t, run_inf("anthropic/claude-sonnet")))
                .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        common::reason(
            svc.run_model_inference(common::req(&reader_t, run_inf("nope/x")))
                .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        common::reason(
            svc.run_model_inference(common::req(
                &reader_t,
                RunModelInferenceRequest {
                    model: "anthropic/claude-sonnet".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Policy and budget are enforced before the gateway is asked.
    f.models
        .set_org_policy(
            &owner(),
            &f.org.id,
            OrgModelPolicy {
                allowed_models: Some(vec!["openai/gpt-x".into()]),
                budget_usd_micros: 0,
            },
        )
        .await
        .unwrap();
    let st = svc
        .run_model_inference(common::req(&reader_t, run_inf("anthropic/claude-sonnet")))
        .await
        .unwrap_err();
    assert_eq!(detail(&st).0, "POLICY_DENIED");
    f.models
        .set_org_policy(
            &owner(),
            &f.org.id,
            OrgModelPolicy {
                allowed_models: None,
                budget_usd_micros: 10,
            },
        )
        .await
        .unwrap();
    let st = svc
        .run_model_inference(common::req(&reader_t, run_inf("anthropic/claude-sonnet")))
        .await
        .unwrap_err();
    assert_eq!(
        detail(&st),
        (
            "QUOTA_EXCEEDED".to_string(),
            "model_budget_exhausted".to_string()
        )
    );
}

#[test]
fn models_registry_is_consistent() {
    let r = bee_forge::module::Registry::core().unwrap();
    assert!(r.event_kind("model.eval_queued").is_some());
    assert!(r.surfaces().iter().any(|s| s.name() == "models"));
}
