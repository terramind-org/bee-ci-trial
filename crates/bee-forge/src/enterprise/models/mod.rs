//! Model gateway records: catalog, provider keys, prompt file index, eval runs and usage
//! (PRODUCT §4.20). Serves `ModelService`. Never on the critical path of source control.
//!
//! Owns the `models` surface. No model is called here: the gateway process serves the
//! inference RPCs and records what it did with [`Models::record_inference`]. What lives here:
//!
//! - **Catalog** ([`Models::upsert_model`]) and org policy ([`Models::set_org_policy`]: an
//!   optional allow list and a monthly budget for gateway-paid inference).
//! - **Provider keys** (BYOK): registrations that name an org secret, never the secret.
//! - **Prompt file index** ([`Models::index_prompt_file`]): the VCS-side indexer parses
//!   `.prompt.yml` content and registers it; list and get are served from the index.
//! - **Eval runs**: a queued record, a state machine driven by the executor
//!   ([`Models::record_eval_progress`], [`Models::record_eval_result`]) and an optional check
//!   run on the changeset.
//! - **Admission** ([`Models::admit_inference`]) for the inference RPCs: org policy, BYOK
//!   availability, the org model budget and the optional [`SpendGate`].

mod catalog;
mod evals;
mod prompts;
mod records;
mod service;
mod usage;

use std::sync::Arc;

use bee_proto::v1::{Event, SubjectKind, SubjectRef};

use crate::audit::AuditDraft;
use crate::enterprise::common::SpendGate;
use crate::error::Result;
use crate::forge::Forge;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, Index, Migration, MigrationOp, Phase, Surface, TableDef, Tx};

pub use catalog::{InferenceEstimate, OrgModelPolicy, Route};
pub use evals::{EvalFilter, EvalResult};
pub use service::ModelsService;
pub use usage::InferenceUsage;

/// The models surface.
pub const SURFACE: Surface = Surface("models");

/// The catalog, scope `""`, id the model id; `k1` provider.
pub static CATALOG: TableDef = TableDef {
    name: "forge_models_catalog",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Org policy, scoped by org id, row `policy`.
pub static POLICY: TableDef = TableDef {
    name: "forge_models_org_policy",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Provider keys, scoped by org id; `k1` provider, `k2` name (unique per org).
pub static KEYS: TableDef = TableDef {
    name: "forge_models_provider_keys",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: true,
    indexes: &[Index::ScopeK1, Index::ScopeK2],
};

/// The prompt file index, scoped by repo id, id the path; `k1` blob (hex).
pub static PROMPTS: TableDef = TableDef {
    name: "forge_models_prompt_files",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

/// Eval runs, scoped by repo id; `k1` prompt path, `k2` changeset (hex), `n1` creation time.
pub static EVALS: TableDef = TableDef {
    name: "forge_models_eval_runs",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK1, Index::ScopeK2],
};

/// Inference usage, scoped by org id; `k1` model, `k2` principal, `n1` time.
pub static USAGE: TableDef = TableDef {
    name: "forge_models_usage",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::ScopeK1, Index::ScopeK2],
};

static M1: Migration = Migration {
    module: "models",
    version: 1,
    name: "create_models_tables",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&CATALOG),
        MigrationOp::CreateTable(&POLICY),
        MigrationOp::CreateTable(&KEYS),
        MigrationOp::CreateTable(&PROMPTS),
        MigrationOp::CreateTable(&EVALS),
        MigrationOp::CreateTable(&USAGE),
    ],
};

/// What the models module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "models",
    surfaces: &[SURFACE],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::new(
            "model.catalog_updated",
            "A catalog model was added, changed or removed",
        ),
        EventKind::new("model.policy_updated", "An org's model policy changed"),
        EventKind::new("model.key_created", "A provider key was registered"),
        EventKind::new("model.key_deleted", "A provider key was removed"),
        EventKind::new(
            "model.prompt_indexed",
            "A prompt file was indexed or removed",
        ),
        EventKind::new("model.eval_queued", "An eval run was queued"),
        EventKind::new("model.eval_updated", "An eval run changed state"),
    ],
    audit_prefixes: &["model."],
};

/// The model gateway records over the store.
pub struct Models {
    pub(crate) forge: Arc<Forge>,
    pub(crate) db: Db,
    pub(crate) gate: Option<Arc<dyn SpendGate>>,
}

impl Models {
    /// Models over `forge`. With a `gate` (billing), gateway-paid inference is admitted only
    /// when the org's `AGENT_TOKENS` budgets allow it.
    pub fn new(forge: Arc<Forge>, gate: Option<Arc<dyn SpendGate>>) -> Arc<Models> {
        Arc::new(Models {
            db: forge.db.clone(),
            forge,
            gate,
        })
    }
}

/// What a change emits and audits.
pub(crate) struct Change<'a> {
    /// Event kind.
    pub kind: &'a str,
    /// Audit action.
    pub action: &'a str,
    /// Org of the event and audit entry (empty for the system log).
    pub org_id: &'a str,
    /// Repo, or empty.
    pub repo_id: &'a str,
    /// Id of the changed object.
    pub target: &'a str,
    /// Proto3-JSON before, if any.
    pub before: Option<&'a str>,
    /// Proto3-JSON after.
    pub after: &'a str,
}

/// Emits the event and audits the change in the caller's transaction.
pub(crate) async fn record(tx: &mut Tx, c: Change<'_>) -> Result<()> {
    tx.emit_event(Event {
        kind: c.kind.into(),
        org_id: c.org_id.into(),
        repo_id: c.repo_id.into(),
        payload_json: c.after.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::Unspecified as i32,
            repo: None,
            id: c.target.into(),
        }),
        ..Default::default()
    })
    .await?;
    let mut d = AuditDraft::new(c.action, c.org_id)
        .repo_id(c.repo_id)
        .target_id(c.target)
        .after(c.after);
    if let Some(b) = c.before {
        d = d.before(b);
    }
    tx.audit(d).await
}
