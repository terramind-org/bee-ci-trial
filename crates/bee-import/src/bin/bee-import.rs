//! `bee-import`: imports a local git repository into in-memory stores and prints the
//! fidelity report. A dry run of what a real import carries, and the `--verify` round trip.
//!
//! ```text
//! bee-import <repo-path> [--verify] [--pull-refs] [--report <file>]
//! bee-import --pack <file.pack> --refs <ref-list> [--sha256] [--verify] [--report <file>]
//! ```
//!
//! Exit status: 0 when the report is complete ("imported 100%"), 1 when anything was dropped
//! or verification failed, 2 on usage or I/O errors.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use bee_core::Ulid;
use bee_git::ObjectFormat;
use bee_import::{
    ImportOptions, ImportTarget, import_pack, import_repository, parse_ref_list, report::category,
};
use bee_store::{
    DEFAULT_DRAFT_RETENTION_DAYS, ManualClock, MemoryBlobStore, MemoryMetaStore, MetaStore, OrgId,
    OrgRow, RepoId, RepoRow, Writer,
};

const USAGE: &str = "usage: bee-import <repo-path> [--verify] [--pull-refs] [--report <file>]\n       bee-import --pack <file.pack> --refs <ref-list> [--sha256] [--verify] [--report <file>]";

#[derive(Default)]
struct Args {
    repo: Option<PathBuf>,
    pack: Option<PathBuf>,
    refs: Option<PathBuf>,
    sha256: bool,
    verify: bool,
    pull_refs: bool,
    report: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .map(PathBuf::from)
                .ok_or(format!("{name} needs a value"))
        };
        match arg.to_str() {
            Some("--verify") => a.verify = true,
            Some("--pull-refs") => a.pull_refs = true,
            Some("--sha256") => a.sha256 = true,
            Some("--report") => a.report = Some(value("--report")?),
            Some("--pack") => a.pack = Some(value("--pack")?),
            Some("--refs") => a.refs = Some(value("--refs")?),
            Some("-h" | "--help") => return Err(String::new()),
            Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ if a.repo.is_none() => a.repo = Some(PathBuf::from(arg)),
            _ => return Err("more than one repository path".into()),
        }
    }
    if a.repo.is_none() && (a.pack.is_none() || a.refs.is_none()) {
        return Err("give a repository path, or --pack and --refs".into());
    }
    Ok(a)
}

async fn run(a: Args) -> Result<bool, String> {
    let clock = Arc::new(ManualClock::new(0));
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(clock));
    let (org, repo) = (OrgId(Ulid::from_u128(1)), RepoId(Ulid::from_u128(2)));
    let e = |e: bee_core::BeeError| format!("{}: {}", e.reason.as_str(), e.message);
    meta.create_org(OrgRow {
        id: org,
        name: "import".into(),
        created_at_ms: 0,
    })
    .await
    .map_err(e)?;
    meta.create_repo(RepoRow {
        id: repo,
        org,
        name: "import".into(),
        created_at_ms: 0,
        draft_retention_days: DEFAULT_DRAFT_RETENTION_DAYS,
    })
    .await
    .map_err(e)?;
    let target = ImportTarget {
        writer: Writer::new(meta.clone(), blobs),
        org,
        repo,
    };
    let mut opts = ImportOptions::new("svc:bee-import");
    opts.verify = a.verify;
    opts.include_pull_refs = a.pull_refs;
    let started = Instant::now();
    let out = match (&a.repo, &a.pack, &a.refs) {
        (Some(path), _, _) => import_repository(path, &target, &opts).await,
        (None, Some(pack), Some(refs)) => {
            let format = if a.sha256 {
                ObjectFormat::Sha256
            } else {
                ObjectFormat::Sha1
            };
            let pack = std::fs::read(pack).map_err(|x| format!("{}: {x}", pack.display()))?;
            let text = std::fs::read(refs).map_err(|x| format!("{}: {x}", refs.display()))?;
            let refs = parse_ref_list(&text, format).map_err(e)?;
            opts.source = a
                .pack
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            import_pack(pack, format, &refs, None, &target, &opts).await
        }
        _ => return Err(USAGE.into()),
    }
    .map_err(e)?;
    let elapsed = started.elapsed().as_secs_f64();
    let r = &out.report;
    let count = |c: &str| r.category(c).map(|c| c.imported).unwrap_or(0);
    eprintln!(
        "{}: {} commits, {} trees, {} blobs, {} refs in {elapsed:.2}s",
        r.summary,
        count(category::COMMITS),
        count(category::TREES),
        count(category::BLOBS),
        count(category::REFS),
    );
    if r.verification.performed {
        eprintln!(
            "verify: {} commits and {} refs checked, {} mismatches",
            r.verification.commits_checked,
            r.verification.refs_checked,
            r.verification.commit_id_mismatches.len()
                + r.verification.graph_mismatches.len()
                + r.verification.ref_mismatches.len()
        );
    }
    let json = r.to_json();
    match &a.report {
        Some(p) => std::fs::write(p, json + "\n").map_err(|x| format!("{}: {x}", p.display()))?,
        None => println!("{json}"),
    }
    Ok(r.complete)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("bee-import: {msg}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(x) => {
            eprintln!("bee-import: {x}");
            return ExitCode::from(2);
        }
    };
    match rt.block_on(run(args)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(msg) => {
            eprintln!("bee-import: {msg}");
            ExitCode::from(2)
        }
    }
}
