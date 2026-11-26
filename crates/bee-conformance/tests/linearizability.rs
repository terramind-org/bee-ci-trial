//! Linearizability of ResolveRef / UpdateRef / Land through the real server (SPEC §14.7,
//! §7.4): concurrent clients record histories of lands on a protected ref (through the land
//! coalescer) and on a plain ref, compare-and-swap updates of workspace refs and strong reads;
//! the history is checked against the reference model of refs with the Wing–Gong checker in
//! `bee_conformance::load::history`.
//!
//! Variants: no faults; injected store faults (errors, "committed, response lost", delays);
//! and coalescer owner failover (the owner process crashes mid-batch while another process
//! keeps landing, then a third process joins), with the fencing token checked afterwards.
//!
//! Gate sizes are bounded (8 clients × 30 operations); `BEE_SCALE_FULL=1` runs 32 × 150.
//! Every run prints its seed; `BEE_SCALE_SEED` replays a seed's choices.

use crate::scale_support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bee_conformance::load::client::{Client, RpcError, change_id};
use bee_conformance::load::history::{
    Operation, RefInput, RefOutput, RefSpec, Verdict, check_partitioned,
};
use bee_core::{ChangeId, ObjectId, RefName};
use bee_store::{Expected, Fault, FaultInjector, FaultRule};
use bee_testkit::TestRng;
use bee_testkit::linearizability::{
    Capture, CapturedInput, CapturedOperation, CapturedOutput, FaultHit, GroundTruth, InitialRef,
    LandResponse, LandedResult, LandedTruth, Parent, RefLogTruth, RefTruth, StackMember,
};
use scale_support::*;

type Op = Operation<RefInput, RefOutput>;

const PLAIN: &str = "green/lin";
const WS: [&str; 2] = ["ws/lin-0", "ws/lin-1"];
static DUMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What the clients work on: candidate land heads (some ChangeIds have two different
/// sources, so exactly one of them may land per ref) and CAS targets.
#[derive(Clone)]
struct Workload {
    heads: Vec<(ChangeId, ObjectId)>,
}

fn name(s: &str) -> RefName {
    RefName::new(s).expect("ref name")
}

/// Creates the plain and workspace refs on the main tip and `n` candidate heads on it.
async fn prepare(c: &Client, n: usize, tag: u64) -> (Workload, Vec<(RefName, ObjectId, u64)>) {
    let (base, version) = main_tip(c).await;
    let mut initial = vec![(name("main"), base, version)];
    for r in std::iter::once(PLAIN).chain(WS) {
        let v = c.create_ref(r, &base).await.expect("create ref");
        initial.push((name(r), base, v));
    }
    let mut heads = Vec::new();
    for i in 0..n {
        // Every fifth head reuses the previous head's ChangeId with different content.
        let k = if i % 5 == 4 { i - 1 } else { i };
        let cid = change_id(tag * 1_000_000 + k as u64);
        let file = (
            format!("lin/{tag}/{i}.txt"),
            format!("head {tag}-{i}\n").into_bytes(),
        );
        let head = c
            .commit(&base, &[file], cid, &format!("lin {i}"))
            .await
            .expect("commit");
        heads.push((cid, head));
    }
    (Workload { heads }, initial)
}

fn land_output(r: Result<bee_conformance::load::client::Landed, RpcError>) -> Option<RefOutput> {
    match r {
        Ok(l) => Some(RefOutput::Landed {
            version: l.version,
            results: l.members.iter().map(|m| m.result).collect(),
            skipped: !l.members.is_empty() && l.members.iter().all(|m| m.skipped),
        }),
        Err(e) if e.reason() == "ALREADY_LANDED" => Some(RefOutput::AlreadyLanded),
        // Anything else may or may not have taken effect.
        Err(_) => None,
    }
}

/// One client's loop. `clients` are the servers it may talk to, in order of preference: on
/// a transport failure (the process died) it moves to the next one.
async fn run_client(
    id: usize,
    clients: Vec<Client>,
    mut rng: TestRng,
    ops: usize,
    ticks: Arc<Ticks>,
    wl: Workload,
) -> Vec<Op> {
    let mut history = Vec::new();
    let mut at = 0usize;
    let mut seen: BTreeMap<&str, u64> = WS.iter().map(|w| (*w, 1)).collect();
    for _ in 0..ops {
        let c = &clients[at.min(clients.len() - 1)];
        let roll = rng.below(100);
        let (input, call, out, failed) = if roll < 55 {
            let (cid, head) = *rng.pick(&wl.heads);
            let r = if roll < 40 { "main" } else { PLAIN };
            let call = ticks.tick();
            let res = c.land(r, &head).await;
            let failed = matches!(&res, Err(e) if e.detail.is_none());
            let input = RefInput::Land {
                name: name(r),
                stack: vec![(cid, head)],
            };
            (input, call, land_output(res), failed)
        } else if roll < 75 {
            let r = *rng.pick(&["main", PLAIN, WS[0], WS[1]]);
            let call = ticks.tick();
            let res = c.resolve(r).await;
            let ret = ticks.tick();
            match res {
                Ok(v) => history.push(Operation {
                    client: id,
                    call,
                    ret: Some(ret),
                    input: RefInput::Read { name: name(r) },
                    output: Some(RefOutput::Read {
                        target: v.map(|x| x.0),
                        version: v.map_or(0, |x| x.1),
                    }),
                }),
                Err(e) => {
                    // A read without an answer has no effect; drop it.
                    if e.detail.is_none() {
                        at += 1;
                    }
                }
            }
            continue;
        } else {
            let r = *rng.pick(&WS);
            let target = rng.pick(&wl.heads).1;
            let expected = seen[r];
            let call = ticks.tick();
            let res = c.update_ref(r, &target, expected).await;
            let out = match &res {
                Ok(v) => {
                    seen.insert(r, *v);
                    Some(RefOutput::CasOk { version: *v })
                }
                Err(e) if e.reason() == "VERSION_MISMATCH" => match e.current_version() {
                    Some(cur) => {
                        seen.insert(r, cur);
                        Some(RefOutput::CasMismatch { current: cur })
                    }
                    None => None,
                },
                Err(_) => None,
            };
            let failed = matches!(&res, Err(e) if e.detail.is_none());
            let input = RefInput::Cas {
                name: name(r),
                expected: Expected::Version(expected),
                target,
            };
            (input, call, out, failed)
        };
        let ret = out.as_ref().map(|_| ticks.tick());
        history.push(Operation {
            client: id,
            call,
            ret,
            input,
            output: out,
        });
        if failed {
            at += 1;
        }
    }
    history
}

/// Builds the specification from the final store state and checks the history.
fn captured_operation(op: &Op) -> CapturedOperation {
    let input = match &op.input {
        RefInput::Read { name } => CapturedInput::Read {
            name: name.to_string(),
        },
        RefInput::Cas {
            name,
            expected,
            target,
        } => {
            let Expected::Version(expected) = expected else {
                unreachable!("test uses version CAS")
            };
            CapturedInput::Cas {
                name: name.to_string(),
                expected: *expected,
                target: target.to_string(),
            }
        }
        RefInput::Land { name, stack } => CapturedInput::Land {
            name: name.to_string(),
            stack: stack
                .iter()
                .map(|(change_id, source)| StackMember {
                    change_id: change_id.to_string(),
                    source: source.to_string(),
                })
                .collect(),
        },
    };
    let output = op.output.as_ref().map(|output| match output {
        RefOutput::Read { target, version } => CapturedOutput::Read {
            target: target.map(|target| target.to_string()),
            version: *version,
        },
        RefOutput::CasOk { version } => CapturedOutput::CasOk { version: *version },
        RefOutput::CasMismatch { current } => CapturedOutput::CasMismatch { current: *current },
        RefOutput::Landed {
            version,
            results,
            skipped,
        } => CapturedOutput::Landed {
            version: *version,
            results: results.iter().map(ToString::to_string).collect(),
            skipped: *skipped,
        },
        RefOutput::AlreadyLanded => CapturedOutput::AlreadyLanded,
    });
    CapturedOperation {
        client: op.client,
        call: op.call,
        ret: op.ret,
        input,
        output,
    }
}

fn dump_capture(
    seed: u64,
    spec: &RefSpec,
    history: &[Op],
    faults: &[(String, u64)],
    truth: GroundTruth,
) -> std::io::Result<PathBuf> {
    let capture = Capture {
        format: 1,
        seed,
        initial: spec
            .initial
            .iter()
            .map(|(name, target, version)| InitialRef {
                name: name.to_string(),
                target: target.to_string(),
                version: *version,
            })
            .collect(),
        results: spec
            .results
            .iter()
            .map(|((name, change_id), result)| LandedResult {
                name: name.to_string(),
                change_id: change_id.to_string(),
                result: result.to_string(),
            })
            .collect(),
        parents: spec
            .parents
            .iter()
            .map(|(child, parent)| Parent {
                child: child.to_string(),
                parent: parent.to_string(),
            })
            .collect(),
        operations: history.iter().map(captured_operation).collect(),
        faults: faults
            .iter()
            .map(|(point, hit)| FaultHit {
                point: point.clone(),
                hit: *hit,
            })
            .collect(),
        truth,
    };
    let dir = std::env::var_os("BEE_LINEARIZABILITY_DUMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))
        });
    std::fs::create_dir_all(&dir)?;
    let sequence = DUMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "linearizability-failure-seed-{seed}-{sequence}.json"
    ));
    bee_testkit::linearizability::write(&path, &capture)?;
    Ok(path)
}

async fn ground_truth(s: &Srv, history: &[Op]) -> GroundTruth {
    let meta = &s.srv.state.meta;
    let refs = meta.list_refs(s.repo(), "").await.expect("refs");
    let mut truth = GroundTruth::default();
    for reference in refs {
        let log = meta
            .ref_log(s.repo(), &reference.name, 0, usize::MAX)
            .await
            .expect("ref log");
        let landed = meta
            .list_landed(s.repo(), Some(&reference.name))
            .await
            .expect("landed rows");
        truth.refs.push(RefTruth {
            name: reference.name.to_string(),
            log: log
                .into_iter()
                .map(|entry| RefLogTruth {
                    version: entry.version,
                    target: entry.new.map(|target| target.to_string()),
                    landed_change_ids: entry
                        .landed
                        .into_iter()
                        .map(|landed| landed.change_id.to_string())
                        .collect(),
                })
                .collect(),
            landed: landed
                .into_iter()
                .map(|row| LandedTruth {
                    change_id: row.change_id.to_string(),
                    source: row.source.to_string(),
                    result: row.result.to_string(),
                    version: row.version,
                })
                .collect(),
        });
    }
    truth.land_responses = history
        .iter()
        .filter_map(|operation| match (&operation.input, &operation.output) {
            (
                RefInput::Land { name, .. },
                Some(RefOutput::Landed {
                    version, results, ..
                }),
            ) => Some(LandResponse {
                name: name.to_string(),
                version: *version,
                results: results.iter().map(ToString::to_string).collect(),
            }),
            _ => None,
        })
        .collect();
    truth
}

async fn assert_ground_truth(s: &Srv, truth: &GroundTruth) {
    let meta = &s.srv.state.meta;
    for reference in &truth.refs {
        let expected = reference.log.first().map_or(0, |entry| entry.version);
        for (offset, entry) in reference.log.iter().enumerate() {
            assert_eq!(
                entry.version,
                expected + offset as u64,
                "I1: ref {} log has a version gap at {}",
                reference.name,
                entry.version
            );
        }
        for pair in reference.log.windows(2) {
            if pair[1].landed_change_ids.is_empty() {
                continue;
            }
            let target = pair[1].target.as_ref().expect("land update has target");
            let previous = pair[0].target.as_ref().expect("previous target");
            let target = ObjectId::from_str(target).expect("captured target");
            let parents = meta.parents(s.repo(), &target).await.expect("parents");
            assert_eq!(
                parents.first().map(ToString::to_string).as_deref(),
                Some(previous.as_str()),
                "I2: landed ref {} version {} does not extend its predecessor",
                reference.name,
                pair[1].version
            );
        }
        let mut changes = std::collections::BTreeSet::new();
        for row in &reference.landed {
            assert!(
                changes.insert(&row.change_id),
                "I3: ref {} has duplicate landed row for {}",
                reference.name,
                row.change_id
            );
            let matches = reference
                .log
                .iter()
                .filter(|entry| {
                    entry.version == row.version && entry.target.as_deref() == Some(&row.result)
                })
                .count();
            assert_eq!(
                matches, 1,
                "I3: landed row {} on {} is not its ref-log target",
                row.change_id, reference.name
            );
        }
    }
    for response in &truth.land_responses {
        let reference = truth
            .refs
            .iter()
            .find(|reference| reference.name == response.name)
            .expect("response ref");
        let entry = reference
            .log
            .iter()
            .find(|entry| entry.version == response.version)
            .expect("I4: Land response version exists in ref log");
        if let Some(result) = response.results.last() {
            assert_eq!(
                entry.target.as_deref(),
                Some(result.as_str()),
                "I4: Land response at {} version {} disagrees with ref log",
                response.name,
                response.version
            );
        }
    }
}

fn load_capture(capture: Capture) -> Result<(RefSpec, Vec<Op>), String> {
    let object = |text: &str| ObjectId::from_str(text).map_err(|error| error.to_string());
    let change = |text: &str| ChangeId::from_str(text).map_err(|error| error.to_string());
    let reference = |text: &str| RefName::new(text).map_err(|error| error.to_string());
    let initial = capture
        .initial
        .into_iter()
        .map(|entry| {
            Ok((
                reference(&entry.name)?,
                object(&entry.target)?,
                entry.version,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let results = capture
        .results
        .into_iter()
        .map(|entry| {
            Ok((
                (reference(&entry.name)?, change(&entry.change_id)?),
                object(&entry.result)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let parents = capture
        .parents
        .into_iter()
        .map(|entry| Ok((object(&entry.child)?, object(&entry.parent)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let operations = capture
        .operations
        .into_iter()
        .map(|operation| {
            let input = match operation.input {
                CapturedInput::Read { name } => RefInput::Read {
                    name: reference(&name)?,
                },
                CapturedInput::Cas {
                    name,
                    expected,
                    target,
                } => RefInput::Cas {
                    name: reference(&name)?,
                    expected: Expected::Version(expected),
                    target: object(&target)?,
                },
                CapturedInput::Land { name, stack } => RefInput::Land {
                    name: reference(&name)?,
                    stack: stack
                        .into_iter()
                        .map(|member| Ok((change(&member.change_id)?, object(&member.source)?)))
                        .collect::<Result<Vec<_>, String>>()?,
                },
            };
            let output = operation
                .output
                .map(|output| -> Result<RefOutput, String> {
                    match output {
                        CapturedOutput::Read { target, version } => Ok(RefOutput::Read {
                            target: target.map(|target| object(&target)).transpose()?,
                            version,
                        }),
                        CapturedOutput::CasOk { version } => Ok(RefOutput::CasOk { version }),
                        CapturedOutput::CasMismatch { current } => {
                            Ok(RefOutput::CasMismatch { current })
                        }
                        CapturedOutput::Landed {
                            version,
                            results,
                            skipped,
                        } => Ok(RefOutput::Landed {
                            version,
                            results: results
                                .into_iter()
                                .map(|result| object(&result))
                                .collect::<Result<Vec<_>, String>>()?,
                            skipped,
                        }),
                        CapturedOutput::AlreadyLanded => Ok(RefOutput::AlreadyLanded),
                    }
                })
                .transpose()?;
            Ok(Operation {
                client: operation.client,
                call: operation.call,
                ret: operation.ret,
                input,
                output,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((
        RefSpec {
            initial,
            results,
            parents,
        },
        operations,
    ))
}

#[test]
fn captured_seed_9_history_remains_not_linearizable() {
    // This is a captured, unexplained failure. The next task must turn it into either a
    // product regression test or a checker fix.
    let capture = bee_testkit::linearizability::read(Path::new(
        "../bee-testkit/tests/fixtures/linearizability-seed-9.json",
    ))
    .expect("read captured history");
    let (spec, history) = load_capture(capture).expect("decode checker input");
    assert!(matches!(
        check_partitioned(&spec, history, |input| input.name().clone(), 5_000_000).0,
        Verdict::NotLinearizable { .. }
    ));
}

fn verdict(capture: &Capture) -> Verdict {
    let (spec, history) = load_capture(capture.clone()).expect("decode checker input");
    check_partitioned(&spec, history, |input| input.name().clone(), 5_000_000).0
}

fn is_rejected(capture: &Capture) -> bool {
    matches!(verdict(capture), Verdict::NotLinearizable { .. })
}

fn ref_name(operation: &CapturedOperation) -> &str {
    match &operation.input {
        CapturedInput::Read { name }
        | CapturedInput::Cas { name, .. }
        | CapturedInput::Land { name, .. } => name,
    }
}

fn with_partition(capture: &Capture, name: &str) -> Capture {
    let mut reduced = capture.clone();
    reduced
        .operations
        .retain(|operation| ref_name(operation) == name);
    reduced
}

/// Returns the smallest real-time prefix of one ref that the checker rejects.
///
/// A prefix contains all calls begun by its cut. Calls still pending at that cut retain their
/// input but lose their response, so no response can be used before it happened. This only
/// removes a suffix of observed time and cannot manufacture a state transition.
fn rejected_prefix(capture: &Capture) -> Capture {
    let mut cuts = capture
        .operations
        .iter()
        .filter_map(|operation| operation.ret)
        .collect::<Vec<_>>();
    cuts.sort_unstable();
    cuts.dedup();
    for cut in cuts {
        let mut prefix = capture.clone();
        prefix.operations.retain(|operation| operation.call <= cut);
        for operation in &mut prefix.operations {
            if operation.ret.is_none_or(|ret| ret > cut) {
                operation.ret = None;
                operation.output = None;
            }
        }
        if is_rejected(&prefix) {
            return prefix;
        }
    }
    panic!("rejected partition has no rejected real-time prefix")
}

fn prefix_is_oracle_closed(capture: &Capture) -> bool {
    let initial = capture
        .initial
        .iter()
        .map(|entry| entry.target.as_str())
        .collect::<Vec<_>>();
    let produced = capture
        .operations
        .iter()
        .filter_map(|operation| match &operation.input {
            CapturedInput::Land { name, stack } => stack.last().map(|member| (name, member)),
            _ => None,
        })
        .filter_map(|(name, member)| {
            capture
                .results
                .iter()
                .find(|result| result.name == *name && result.change_id == member.change_id)
                .map(|result| result.result.as_str())
        })
        .collect::<Vec<_>>();
    capture.operations.iter().all(|operation| {
        let CapturedInput::Land { name, stack } = &operation.input else {
            return true;
        };
        let Some(member) = stack.last() else {
            return true;
        };
        let Some(result) = capture
            .results
            .iter()
            .find(|result| result.name == *name && result.change_id == member.change_id)
        else {
            return true;
        };
        let Some(parent) = capture
            .parents
            .iter()
            .find(|parent| parent.child == result.result)
            .map(|parent| parent.parent.as_str())
        else {
            return false;
        };
        initial.contains(&parent) || produced.contains(&parent)
    })
}

fn has_no_effect(operation: &CapturedOperation) -> bool {
    matches!(
        operation.output,
        Some(CapturedOutput::Read { .. })
            | Some(CapturedOutput::CasMismatch { .. })
            | Some(CapturedOutput::AlreadyLanded)
            | Some(CapturedOutput::Landed { skipped: true, .. })
    )
}

/// Removes only completed observations which the model proves leave state unchanged.
fn remove_no_effect_operations(mut capture: Capture) -> Capture {
    let mut index = 0;
    while index < capture.operations.len() {
        if !has_no_effect(&capture.operations[index]) {
            index += 1;
            continue;
        }
        let mut candidate = capture.clone();
        candidate.operations.remove(index);
        if is_rejected(&candidate) {
            capture = candidate;
        } else {
            index += 1;
        }
    }
    capture
}

fn reduce_capture(capture: &Capture) -> Capture {
    let mut names = capture.operations.iter().map(ref_name).collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    let partition = names
        .into_iter()
        .map(|name| with_partition(capture, name))
        .find(is_rejected)
        .expect("one partition must reject when the whole history rejects");
    let prefix = rejected_prefix(&partition);
    if prefix_is_oracle_closed(&prefix) {
        remove_no_effect_operations(prefix)
    } else {
        // The final landed-index oracle names a predecessor outside the prefix.  Cutting it
        // would reject because of the missing oracle predecessor, not because of the capture.
        remove_no_effect_operations(partition)
    }
}

#[test]
fn captured_seed_9_reduced_history_remains_not_linearizable() {
    let source = Path::new("../bee-testkit/tests/fixtures/linearizability-seed-9.json");
    let expected = bee_testkit::linearizability::read(Path::new(
        "../bee-testkit/tests/fixtures/linearizability-seed-9.min.json",
    ))
    .expect("read committed reduced history");
    let actual =
        reduce_capture(&bee_testkit::linearizability::read(source).expect("read captured history"));
    assert_eq!(actual, expected, "committed reduction must be reproducible");
    assert!(
        matches!(verdict(&expected), Verdict::NotLinearizable { .. }),
        "reduced history must still reject"
    );
}

async fn check_history(
    s: &Srv,
    initial: Vec<(RefName, ObjectId, u64)>,
    history: Vec<Op>,
    seed: u64,
    faults: &[(String, u64)],
) {
    let meta = &s.srv.state.meta;
    let landed = meta.list_landed(s.repo(), None).await.expect("landed");
    let truth = ground_truth(s, &history).await;
    assert_ground_truth(s, &truth).await;
    let mut spec = RefSpec {
        initial,
        ..RefSpec::default()
    };
    for l in &landed {
        spec.results
            .insert((l.ref_name.clone(), l.change_id), l.result);
        let parents = meta.parents(s.repo(), &l.result).await.expect("parents");
        if let Some(p) = parents.first() {
            spec.parents.insert(l.result, *p);
        }
    }
    let lands = history
        .iter()
        .filter(|o| matches!(o.input, RefInput::Land { .. }))
        .count();
    let (verdict, stats) =
        check_partitioned(&spec, history.clone(), |i| i.name().clone(), 5_000_000);
    eprintln!(
        "linearizability: {} operations ({lands} lands, {} with unknown outcome), {} landed changes, {} configurations explored",
        stats.operations,
        stats.ambiguous,
        landed.len(),
        stats.explored
    );
    match verdict {
        Verdict::Linearizable(_) => {}
        Verdict::NotLinearizable { longest, stuck } => {
            let path = dump_capture(seed, &spec, &history, faults, truth)
                .expect("write rejected linearizability history");
            eprintln!("linearizability history written to {}", path.display());
            let show = |i: &usize| format!("  #{i}: {:?}", history[*i]);
            panic!(
                "history is not linearizable.\nlongest prefix ({}):\n{}\nstuck candidates:\n{}",
                longest.len(),
                longest
                    .iter()
                    .rev()
                    .take(10)
                    .map(show)
                    .collect::<Vec<_>>()
                    .join("\n"),
                stuck.iter().map(show).collect::<Vec<_>>().join("\n")
            );
        }
        Verdict::Undecided { limit } => panic!("checker undecided after {limit} configurations"),
    }
    for r in ["main", PLAIN] {
        s.assert_ref_log_continuous(r).await;
    }
    // No change landed twice on one ref, and every landed change is one of the heads.
    let mut per_ref: BTreeMap<(RefName, ChangeId), usize> = BTreeMap::new();
    for l in &landed {
        *per_ref
            .entry((l.ref_name.clone(), l.change_id))
            .or_default() += 1;
    }
    assert!(per_ref.values().all(|n| *n == 1), "duplicate lands");
}

fn install_faults(faults: &FaultInjector, seed: u64) {
    let mut rng = TestRng::seed(seed ^ 0xfa17);
    for _ in 0..size(6, 30) {
        faults.add(FaultRule::nth(
            "meta.update_ref",
            rng.below(60),
            Fault::response_lost(),
        ));
        faults.add(FaultRule::nth(
            "meta.update_ref",
            rng.below(60),
            Fault::unavailable(),
        ));
        faults.add(FaultRule::nth(
            "meta.get_ref",
            rng.below(200),
            Fault::unavailable(),
        ));
        faults.add(FaultRule::nth(
            "meta.insert_changeset",
            rng.below(60),
            Fault::unavailable(),
        ));
        faults.add(FaultRule::nth(
            "meta.insert_changeset_with_objects",
            rng.below(60),
            Fault::unavailable(),
        ));
        faults.add(FaultRule::nth(
            "meta.insert_changeset_with_objects",
            rng.below(60),
            Fault::response_lost(),
        ));
        faults.add(FaultRule::nth(
            "meta.put_batch",
            rng.below(30),
            Fault::unavailable(),
        ));
        faults.add(FaultRule::nth(
            "meta.put_batch",
            rng.below(30),
            Fault::response_lost(),
        ));
    }
    faults.add(FaultRule::always(
        "meta.update_ref",
        Fault::Delay(Duration::from_millis(3)),
    ));
}

async fn run(
    clients_of: impl Fn(usize) -> Vec<Client>,
    wl: &Workload,
    seed: u64,
    n_clients: usize,
    ops: usize,
) -> Vec<Op> {
    let ticks = Arc::new(Ticks::default());
    let mut root = TestRng::seed(seed);
    let mut handles = Vec::new();
    for id in 0..n_clients {
        let rng = TestRng::seed(root.next_u64());
        handles.push(tokio::spawn(run_client(
            id,
            clients_of(id),
            rng,
            ops,
            ticks.clone(),
            wl.clone(),
        )));
    }
    let mut history = Vec::new();
    for h in handles {
        history.extend(h.await.expect("client task"));
    }
    history
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn lands_updates_and_reads_are_linearizable() {
    let seed = seed("lands_updates_and_reads_are_linearizable", 0x5eed_0001);
    let (s, _dir) = start_fresh(|_| {}, None).await;
    let (wl, initial) = prepare(&s.client, size(40, 400), 1).await;
    let c = s.client.clone();
    let history = run(|_| vec![c.clone()], &wl, seed, size(8, 32), size(30, 150)).await;
    check_history(&s, initial, history, seed, &[]).await;
    let (clean, v) = s.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn linearizable_under_injected_store_faults() {
    let seed = seed("linearizable_under_injected_store_faults", 0x5eed_0002);
    let faults = FaultInjector::new();
    let (s, _dir) = start_fresh(|_| {}, Some(faults.clone())).await;
    let (wl, initial) = prepare(&s.client, size(40, 400), 2).await;
    faults.reset();
    // "Committed, response lost" and plain errors on the ref CAS, errors on reads of refs
    // and on changeset inserts (rebases), and short delays that widen the races.
    install_faults(&faults, seed);
    let c = s.client.clone();
    let history = run(|_| vec![c.clone()], &wl, seed, size(8, 32), size(30, 150)).await;
    let fired = faults.fired();
    faults.clear_rules();
    eprintln!("faults fired: {}", fired.len());
    assert!(
        fired.iter().any(|(p, _)| p == "meta.update_ref"),
        "the CAS faults fired: {fired:?}"
    );
    check_history(&s, initial, history, seed, &fired).await;
    let (clean, v) = s.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
}

/// Searches deterministic fault schedules only when explicitly requested.  Keeping the gate
/// inert by default makes the ordinary conformance suite's duration independent of this hunt.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hunt_injected_store_fault_histories() {
    let Ok(limit) = std::env::var("BEE_LIN_HUNT") else {
        return;
    };
    let limit: u64 = limit
        .parse()
        .expect("BEE_LIN_HUNT must be a positive seed limit");
    assert!(limit > 0, "BEE_LIN_HUNT must be a positive seed limit");
    for seed in 1..=limit {
        let faults = FaultInjector::new();
        let (s, _dir) = start_fresh(|_| {}, Some(faults.clone())).await;
        let (wl, initial) = prepare(&s.client, size(40, 400), 20_000 + seed).await;
        faults.reset();
        install_faults(&faults, seed);
        let history = run(
            |_| vec![s.client.clone()],
            &wl,
            seed,
            size(8, 32),
            size(30, 150),
        )
        .await;
        let fired = faults.fired();
        faults.clear_rules();
        let meta = &s.srv.state.meta;
        let landed = meta.list_landed(s.repo(), None).await.expect("landed");
        let truth = ground_truth(&s, &history).await;
        assert_ground_truth(&s, &truth).await;
        let mut spec = RefSpec {
            initial,
            ..RefSpec::default()
        };
        for row in &landed {
            spec.results
                .insert((row.ref_name.clone(), row.change_id), row.result);
            if let Some(parent) = meta
                .parents(s.repo(), &row.result)
                .await
                .expect("parents")
                .first()
            {
                spec.parents.insert(row.result, *parent);
            }
        }
        if matches!(
            check_partitioned(&spec, history.clone(), |i| i.name().clone(), 5_000_000).0,
            Verdict::NotLinearizable { .. }
        ) {
            let capture = Capture {
                format: 1,
                seed,
                initial: spec
                    .initial
                    .iter()
                    .map(|(name, target, version)| InitialRef {
                        name: name.to_string(),
                        target: target.to_string(),
                        version: *version,
                    })
                    .collect(),
                results: spec
                    .results
                    .iter()
                    .map(|((name, change_id), result)| LandedResult {
                        name: name.to_string(),
                        change_id: change_id.to_string(),
                        result: result.to_string(),
                    })
                    .collect(),
                parents: spec
                    .parents
                    .iter()
                    .map(|(child, parent)| Parent {
                        child: child.to_string(),
                        parent: parent.to_string(),
                    })
                    .collect(),
                operations: history.iter().map(captured_operation).collect(),
                faults: fired
                    .iter()
                    .map(|(point, hit)| FaultHit {
                        point: point.clone(),
                        hit: *hit,
                    })
                    .collect(),
                truth,
            };
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../bee-testkit/tests/fixtures/linearizability-truth-{seed}.json"
            ));
            bee_testkit::linearizability::write(&path, &capture).expect("write truth capture");
            panic!(
                "seed {seed} is not linearizable; truth capture written to {}",
                path.display()
            );
        }
        eprintln!(
            "linearizability hunt: seed {seed} passed ({} faults fired)",
            fired.len()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn linearizable_across_coalescer_owner_failover() {
    let seed = seed("linearizable_across_coalescer_owner_failover", 0x5eed_0003);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    cfg.land.owner_lease_ms = 600;
    // The first owner publishes slowly, so its crash lands in the middle of batches.
    let slow = FaultInjector::new();
    let a = start_on(cfg.clone(), Some(slow.clone())).await;
    let b = start_on(cfg.clone(), None).await;
    assert_eq!(a.repo(), b.repo(), "both processes serve one store");
    let (wl, initial) = prepare(&a.client, size(40, 400), 3).await;
    // Make A the owner of main before the run.
    let warm = a
        .client
        .commit(
            &initial[0].1,
            &[("lin/warm.txt".into(), b"w\n".to_vec())],
            change_id(3_999_999),
            "warm",
        )
        .await
        .expect("warm commit");
    let warmed = a.client.land("main", &warm).await.expect("warm land");
    let initial: Vec<_> = initial
        .into_iter()
        .map(|(n, t, v)| {
            if n.as_str() == "main" {
                (n, warmed.target, warmed.version)
            } else {
                (n, t, v)
            }
        })
        .collect();
    let lease = bee_server::coalescer::read_lease(&a.srv.state, a.repo(), "main")
        .await
        .expect("lease")
        .expect("lease row");
    assert_eq!(lease.owner, a.srv.state.instance_id);
    let first_fence = lease.fence;
    let a_instance = a.srv.state.instance_id.clone();
    slow.add(FaultRule::always(
        "meta.update_ref",
        Fault::Delay(Duration::from_millis(40)),
    ));

    let n = size(8, 32);
    let ops = size(30, 150);
    let ca = a.client.clone();
    let cb = b.client.clone();
    // Half the clients start on A and fall back to B when A dies.
    let clients_of = move |id: usize| {
        if id.is_multiple_of(2) {
            vec![ca.clone(), cb.clone()]
        } else {
            vec![cb.clone()]
        }
    };
    let crash = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        a.srv.crash().await;
    });
    let history = run(clients_of, &wl, seed, n, ops).await;
    crash.await.expect("crash task");

    // A third process joins the same store after the dead owner's lease expired.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let c3 = start_on(cfg.clone(), None).await;
    let more = run(
        |_| vec![c3.client.clone()],
        &wl,
        seed ^ 3,
        size(4, 16),
        size(10, 50),
    )
    .await;
    // Histories from both phases form one history: the second phase's ticks start over, so
    // shift them past the first phase.
    let offset = history
        .iter()
        .flat_map(|o| [Some(o.call), o.ret])
        .flatten()
        .max()
        .unwrap_or(0)
        + 1;
    let mut all = history;
    all.extend(more.into_iter().map(|mut o| {
        o.call += offset;
        o.ret = o.ret.map(|r| r + offset);
        o
    }));
    // Every operation of the first phase whose response arrived is complete before the
    // second phase starts; unanswered ones (A's crash) stay open-ended.
    check_history(&b, initial, all, seed, &[]).await;

    let lease = bee_server::coalescer::read_lease(&b.srv.state, b.repo(), "main")
        .await
        .expect("lease")
        .expect("lease row");
    assert_ne!(lease.owner, a_instance, "the crashed owner lost the ref");
    assert!(
        lease.fence > first_fence,
        "a takeover increments the fencing token ({} -> {})",
        first_fence,
        lease.fence
    );
    let (clean, v) = b.fsck_clean().await;
    assert!(clean, "fsck: {v:?}");
    c3.srv.shutdown().await;
    b.srv.shutdown().await;
}
