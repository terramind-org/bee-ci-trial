//! Pull request attribution for both independent runs and swarm members. The change RPC
//! verifies the issuer's run token before linking. A live command runner may open a
//! change through the RPC without returning its number in the runner's final report.

use bee_auth::VerifiedToken;
use bee_proto::v1::{PrincipalKind, Repo};

use crate::agents::{RUNS, SURFACE as AGENTS, model::RunRec};
use crate::auth::Caller;
use crate::changes::SURFACE;
use crate::cx::Cx;
use crate::error::Result;
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, TableDef, Tx,
};

static RUN_CHANGES: TableDef = TableDef {
    name: "forge_run_outcome_changes",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};

// Scoped to the changes surface so a link is durable in the same commit as its CR.
pub(crate) static MIGRATION: Migration = Migration {
    module: "changes",
    version: 6,
    name: "create_run_outcome_changes",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&RUN_CHANGES)],
};

pub(crate) async fn verified_run(db: &Db, caller: &Caller, repo: &Repo) -> Result<Option<String>> {
    verified_run_token(db, &caller.cx, &caller.token, repo).await
}

// Safety's native write handlers hold an already verified token without a forge Caller.
pub(crate) async fn verified_run_token(
    db: &Db,
    cx: &Cx,
    token: &VerifiedToken,
    repo: &Repo,
) -> Result<Option<String>> {
    if cx.actor_kind != PrincipalKind::Agent {
        return Ok(None);
    }
    let Some(id) = token.session().and_then(|s| s.strip_prefix("run-")) else {
        return Ok(None);
    };
    let id = id.to_ascii_uppercase();
    let mut tx = db.begin(AGENTS, cx).await?;
    let Some((rec, _)) = tx.get_msg::<RunRec>(&RUNS, &repo.id, &id).await? else {
        return Ok(None);
    };
    let run = rec.run();
    if !run.agent.is_some_and(|p| p.id == cx.actor_id)
        || !token.block_ids().iter().any(|t| t.0 == rec.token_id)
    {
        return Ok(None);
    }
    Ok(Some(run.id))
}

pub(crate) async fn record_change(tx: &mut Tx, repo: &str, run: &str, cr: &str) -> Result<()> {
    tx.insert(&RUN_CHANGES, &Row::new(repo, cr).k1(run)).await
}

pub(crate) async fn changes(db: &Db, cx: &Cx, repo: &str, run: &str) -> Result<Vec<String>> {
    let mut tx = db.begin(SURFACE, cx).await?;
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let rows = tx
            .scan(
                &RUN_CHANGES,
                &Scan::scope(repo, 128)
                    .key(Key::K1(run.into()))
                    .after(cursor),
            )
            .await?;
        cursor = rows.last().map(Cursor::after);
        let done = rows.len() < 128;
        ids.extend(rows.into_iter().map(|row| row.id));
        if done {
            return Ok(ids);
        }
    }
}
