//! Linearizability checking (SPEC §14.7) of recorded concurrent histories.
//!
//! A [`History`] is a list of operations, each with the logical time it was invoked and the
//! time its response arrived (from one shared counter, so the order is real-time order across
//! clients). An operation whose outcome is unknown (a timeout, a retryable error after which
//! the write may or may not have committed) has no response time: it may take effect at any
//! point after its invocation, or never.
//!
//! [`check`] searches for a sequential order of the operations that respects real-time order
//! and that a sequential [`Model`] accepts, in the style of Wing and Gong's algorithm with
//! Lowe's memoization of `(linearized set, model state)` pairs: at each step only operations
//! that no un-linearized operation precedes in real time are candidates, and a configuration
//! already explored is never explored again. Histories are checked per key (a ref) with
//! [`check_partitioned`], since operations on different refs commute (P-compositionality).
//!
//! [`RefSpec`] is the sequential specification of refs, written over the reference model
//! [`bee_testkit::model::refs::RefModel`]: reads, compare-and-swap updates and lands with
//! the landed index. Rebase results come from an oracle (the final landed index read from the
//! store), and every result must sit directly on the tip it was rebased onto, so the check
//! also proves that each stack landed "as if alone at that position" (SPEC §7.4).

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};

use bee_core::{ChangeId, ObjectId, RefName};
use bee_store::Expected;
use bee_testkit::model::refs::{LandOutcome, RefModel, RefModelError};

/// One recorded operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation<I, O> {
    /// Which client issued it (for reports only).
    pub client: usize,
    /// Logical invocation time.
    pub call: u64,
    /// Logical response time; `None` when the outcome is unknown.
    pub ret: Option<u64>,
    /// What was asked.
    pub input: I,
    /// What was answered; `None` when the outcome is unknown.
    pub output: Option<O>,
}

/// A sequential specification.
pub trait Model {
    /// The model state.
    type State: Clone + PartialEq + Debug;
    /// Operation inputs.
    type Input: Debug;
    /// Operation outputs.
    type Output: Debug;

    /// The initial state.
    fn init(&self) -> Self::State;

    /// The states after applying `input` to `state` and answering `output`. An empty result
    /// means the operation cannot answer `output` in `state`. With `output = None` (unknown
    /// outcome) the result lists every state the operation could have produced if it took
    /// effect; not taking effect at all is always allowed and need not be listed.
    fn step(
        &self,
        state: &Self::State,
        input: &Self::Input,
        output: Option<&Self::Output>,
    ) -> Vec<Self::State>;

    /// A hash of the state (equal states must have equal fingerprints).
    fn fingerprint(&self, state: &Self::State) -> u64;
}

/// The verdict of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A linearization exists; the operation indices in linearization order (operations with
    /// unknown outcome that never took effect are absent).
    Linearizable(Vec<usize>),
    /// No linearization exists. `longest` is the longest prefix the search could linearize
    /// and `stuck` the operations that were candidates after it, none of which the model
    /// accepted.
    NotLinearizable {
        /// The longest linearizable prefix found.
        longest: Vec<usize>,
        /// The candidates that could not follow it.
        stuck: Vec<usize>,
    },
    /// The search explored `limit` configurations without deciding.
    Undecided {
        /// The exploration limit that was reached.
        limit: u64,
    },
}

impl Verdict {
    /// True for [`Verdict::Linearizable`].
    pub fn is_linearizable(&self) -> bool {
        matches!(self, Verdict::Linearizable(_))
    }
}

/// Statistics of one check, for reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// Operations in the history.
    pub operations: usize,
    /// Operations with an unknown outcome.
    pub ambiguous: usize,
    /// Configurations explored.
    pub explored: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn get(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }
    fn with(&self, i: usize) -> Bits {
        let mut b = self.clone();
        b.0[i / 64] |= 1 << (i % 64);
        b
    }
}

struct Frame<S> {
    bits: Bits,
    order: Vec<usize>,
    candidates: Vec<(usize, S)>,
}

/// Checks one history against `model`, exploring at most `limit` configurations.
pub fn check<M: Model>(
    model: &M,
    history: &[Operation<M::Input, M::Output>],
    limit: u64,
) -> (Verdict, CheckStats) {
    let n = history.len();
    let mut stats = CheckStats {
        operations: n,
        ambiguous: history.iter().filter(|o| o.ret.is_none()).count(),
        explored: 0,
    };
    // Operations sorted by invocation time; `by_call[k]` is an index into `history`.
    let mut by_call: Vec<usize> = (0..n).collect();
    by_call.sort_by_key(|&i| (history[i].call, i));
    let completed = history.iter().filter(|o| o.ret.is_some()).count();
    let mut seen: HashMap<(Bits, u64), Vec<M::State>> = HashMap::new();
    let mut longest: (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());

    let expand = |bits: &Bits, state: &M::State| -> Vec<(usize, M::State)> {
        // The earliest response among operations not yet linearized: only operations
        // invoked before it may come next.
        let horizon = by_call
            .iter()
            .filter(|&&i| !bits.get(i))
            .filter_map(|&i| history[i].ret)
            .min()
            .unwrap_or(u64::MAX);
        let mut out = Vec::new();
        for &i in &by_call {
            let op = &history[i];
            if op.call > horizon {
                break;
            }
            if bits.get(i) {
                continue;
            }
            for next in model.step(state, &op.input, op.output.as_ref()) {
                out.push((i, next));
            }
        }
        // Explore in reverse so that popping from the end tries earlier operations first.
        out.reverse();
        out
    };

    let root_bits = Bits::new(n);
    let root_state = model.init();
    let mut stack = vec![Frame {
        candidates: expand(&root_bits, &root_state),
        bits: root_bits,
        order: Vec::new(),
    }];
    if completed == 0 {
        return (Verdict::Linearizable(Vec::new()), stats);
    }
    while let Some(top) = stack.last_mut() {
        let Some((i, next)) = top.candidates.pop() else {
            stack.pop();
            continue;
        };
        let bits = top.bits.with(i);
        let mut order = top.order.clone();
        order.push(i);
        let fp = model.fingerprint(&next);
        let bucket = seen.entry((bits.clone(), fp)).or_default();
        if bucket.contains(&next) {
            continue;
        }
        bucket.push(next.clone());
        stats.explored += 1;
        if stats.explored > limit {
            return (Verdict::Undecided { limit }, stats);
        }
        let done = (0..n)
            .filter(|&k| history[k].ret.is_some())
            .all(|k| bits.get(k));
        if done {
            return (Verdict::Linearizable(order), stats);
        }
        let candidates = expand(&bits, &next);
        if order.len() > longest.0.len() {
            let stuck = by_call
                .iter()
                .copied()
                .filter(|&k| !bits.get(k))
                .take(8)
                .collect();
            longest = (order.clone(), stuck);
        }
        stack.push(Frame {
            bits,
            order,
            candidates,
        });
    }
    (
        Verdict::NotLinearizable {
            longest: longest.0,
            stuck: longest.1,
        },
        stats,
    )
}

/// Checks a history key by key: operations are grouped by `key(input)` and every group must
/// be linearizable on its own. Returns the first failing key's verdict (with indices into the
/// whole history) or `Linearizable` with every group's order concatenated.
pub fn check_partitioned<M: Model, K: Ord + Clone>(
    model: &M,
    history: Vec<Operation<M::Input, M::Output>>,
    key: impl Fn(&M::Input) -> K,
    limit: u64,
) -> (Verdict, CheckStats) {
    type Indexed<I, O> = Vec<(usize, Operation<I, O>)>;
    let mut groups: BTreeMap<K, Indexed<M::Input, M::Output>> = BTreeMap::new();
    for (i, op) in history.into_iter().enumerate() {
        groups.entry(key(&op.input)).or_default().push((i, op));
    }
    let mut total = CheckStats::default();
    let mut all = Vec::new();
    for (_, group) in groups {
        let (index, ops): (Vec<usize>, Vec<_>) = group.into_iter().unzip();
        let (verdict, stats) = check(model, &ops, limit);
        total.operations += stats.operations;
        total.ambiguous += stats.ambiguous;
        total.explored += stats.explored;
        let map = |v: Vec<usize>| v.into_iter().map(|k| index[k]).collect::<Vec<_>>();
        match verdict {
            Verdict::Linearizable(order) => all.extend(map(order)),
            Verdict::NotLinearizable { longest, stuck } => {
                return (
                    Verdict::NotLinearizable {
                        longest: map(longest),
                        stuck: map(stuck),
                    },
                    total,
                );
            }
            u @ Verdict::Undecided { .. } => return (u, total),
        }
    }
    (Verdict::Linearizable(all), total)
}

/// An operation on refs, as issued through the API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefInput {
    /// `ResolveRef` (strong read).
    Read {
        /// The ref.
        name: RefName,
    },
    /// `UpdateRef`: compare-and-swap on a non-protected ref.
    Cas {
        /// The ref.
        name: RefName,
        /// The precondition.
        expected: Expected,
        /// The new target.
        target: ObjectId,
    },
    /// `Land` of a linear stack, members `(change id, source changeset)` in stack order.
    Land {
        /// The ref.
        name: RefName,
        /// The stack, oldest first.
        stack: Vec<(ChangeId, ObjectId)>,
    },
}

impl RefInput {
    /// The ref the operation is on.
    pub fn name(&self) -> &RefName {
        match self {
            RefInput::Read { name } | RefInput::Cas { name, .. } | RefInput::Land { name, .. } => {
                name
            }
        }
    }
}

/// A definite answer to a [`RefInput`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefOutput {
    /// A read: target and version (`None, 0` for a ref that does not exist).
    Read {
        /// Current target.
        target: Option<ObjectId>,
        /// Current version.
        version: u64,
    },
    /// A successful CAS: the version written.
    CasOk {
        /// The new version.
        version: u64,
    },
    /// `VERSION_MISMATCH` with the current version from the payload.
    CasMismatch {
        /// The version the ref was at.
        current: u64,
    },
    /// A land answered: the ref version naming it and each member's result in stack order.
    /// `skipped` is true when every member had already landed from the same source.
    Landed {
        /// The version reported.
        version: u64,
        /// Each member's result changeset.
        results: Vec<ObjectId>,
        /// Every member was already recorded (nothing new published).
        skipped: bool,
    },
    /// `ALREADY_LANDED`: a member's ChangeId landed from a different source.
    AlreadyLanded,
}

/// The sequential specification of refs over [`RefModel`].
///
/// `results` maps each `(ref, ChangeId)` that ended up landed to its result changeset (from
/// the final landed index) and `parents` maps a result to its first parent. A land may only
/// produce the recorded result, and that result's first parent must be the tip it landed on.
#[derive(Clone, Debug, Default)]
pub struct RefSpec {
    /// The initial refs: name, target and version.
    pub initial: Vec<(RefName, ObjectId, u64)>,
    /// Final landed index: `(ref, ChangeId)` → result.
    pub results: BTreeMap<(RefName, ChangeId), ObjectId>,
    /// First parent of each result changeset.
    pub parents: BTreeMap<ObjectId, ObjectId>,
}

impl RefSpec {
    /// Lands `stack` on `next` with recorded results; `Invalid` when a recorded result does
    /// not sit on the tip it would land on (or the change never landed).
    fn land_on(
        &self,
        next: &mut RefModel,
        name: &RefName,
        stack: &[(ChangeId, ObjectId)],
    ) -> Result<LandOutcome, RefModelError> {
        let mut ok = true;
        let mut rebase = |tip: Option<ObjectId>, source: ObjectId| -> ObjectId {
            let cid = stack
                .iter()
                .find(|(_, s)| *s == source)
                .map(|(c, _)| *c)
                .unwrap_or(ChangeId::new([0; 16]));
            match self.results.get(&(name.clone(), cid)) {
                Some(r) if tip.is_some() && self.parents.get(r) == tip.as_ref() => *r,
                Some(r) => {
                    ok = false;
                    *r
                }
                None => {
                    ok = false;
                    source
                }
            }
        };
        let out = next.land(name, stack, &mut rebase)?;
        if !ok {
            // The recorded result does not sit on this tip (or the change never landed): this
            // land cannot take effect here.
            return Err(RefModelError::Invalid);
        }
        Ok(out)
    }
}

impl Model for RefSpec {
    type State = RefModel;
    type Input = RefInput;
    type Output = RefOutput;

    fn init(&self) -> RefModel {
        let mut m = RefModel::new();
        for (name, target, version) in &self.initial {
            // Reach the recorded version with empty steps, then the target.
            let mut steps: Vec<(Option<ObjectId>, Vec<_>)> =
                (1..*version).map(|_| (Some(*target), Vec::new())).collect();
            steps.push((Some(*target), Vec::new()));
            let _ = m.update(name, Expected::Absent, &steps);
        }
        m
    }

    fn step(
        &self,
        state: &RefModel,
        input: &RefInput,
        output: Option<&RefOutput>,
    ) -> Vec<RefModel> {
        match input {
            RefInput::Read { name } => {
                let (target, version) = state.get(name).unwrap_or((None, 0));
                match output {
                    Some(RefOutput::Read {
                        target: t,
                        version: v,
                    }) if *t == target && *v == version => vec![state.clone()],
                    // A read with no answer has no effect.
                    _ => Vec::new(),
                }
            }
            RefInput::Cas {
                name,
                expected,
                target,
            } => {
                let mut next = state.clone();
                let r = next.update(name, *expected, &[(Some(*target), Vec::new())]);
                match (r, output) {
                    (Ok((_, v)), Some(RefOutput::CasOk { version })) if v == *version => {
                        vec![next]
                    }
                    (Ok(_), None) => vec![next],
                    (
                        Err(RefModelError::VersionMismatch { current }),
                        Some(RefOutput::CasMismatch { current: c }),
                    ) if current == *c => vec![state.clone()],
                    _ => Vec::new(),
                }
            }
            RefInput::Land { name, stack } => {
                let mut next = state.clone();
                let r = self.land_on(&mut next, name, stack);
                match (r, output) {
                    (
                        Ok(LandOutcome::Landed { version, results }),
                        Some(RefOutput::Landed {
                            version: v,
                            results: rs,
                            skipped: false,
                        }),
                    ) if version == *v && results == *rs => vec![next],
                    (Ok(LandOutcome::Landed { .. }), None) => vec![next],
                    (
                        Ok(LandOutcome::AlreadyRecorded { results }),
                        Some(RefOutput::Landed {
                            version: v,
                            results: rs,
                            skipped: true,
                        }),
                    ) if results == *rs
                        && stack
                            .last()
                            .and_then(|(c, _)| state.landed(name, c))
                            .map(|x| x.2)
                            == Some(*v) =>
                    {
                        vec![state.clone()]
                    }
                    (Err(RefModelError::AlreadyLanded { .. }), Some(RefOutput::AlreadyLanded)) => {
                        vec![state.clone()]
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    fn fingerprint(&self, state: &RefModel) -> u64 {
        let mut h = DefaultHasher::new();
        state.refs().hash(&mut h);
        state.landed_all().len().hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::ObjectKind;

    /// A single register: write(v) and read() -> v.
    struct Register;

    #[derive(Debug)]
    enum RegIn {
        Write(u8),
        Read,
    }

    impl Model for Register {
        type State = u8;
        type Input = RegIn;
        type Output = u8;
        fn init(&self) -> u8 {
            0
        }
        fn step(&self, s: &u8, i: &RegIn, o: Option<&u8>) -> Vec<u8> {
            match (i, o) {
                (RegIn::Write(v), _) => vec![*v],
                (RegIn::Read, Some(v)) if v == s => vec![*s],
                _ => Vec::new(),
            }
        }
        fn fingerprint(&self, s: &u8) -> u64 {
            u64::from(*s)
        }
    }

    fn op(
        client: usize,
        call: u64,
        ret: Option<u64>,
        input: RegIn,
        out: Option<u8>,
    ) -> Operation<RegIn, u8> {
        Operation {
            client,
            call,
            ret,
            input,
            output: out,
        }
    }

    #[test]
    fn overlapping_write_and_read_is_linearizable_either_way() {
        // w(1) overlaps r -> 1 and r -> 0: both reads are fine.
        let h = vec![
            op(0, 0, Some(10), RegIn::Write(1), Some(0)),
            op(1, 1, Some(2), RegIn::Read, Some(0)),
            op(2, 3, Some(4), RegIn::Read, Some(1)),
        ];
        assert!(check(&Register, &h, 1_000).0.is_linearizable());
    }

    #[test]
    fn stale_read_after_a_completed_write_is_caught() {
        let h = vec![
            op(0, 0, Some(1), RegIn::Write(1), Some(0)),
            op(1, 2, Some(3), RegIn::Read, Some(0)),
        ];
        match check(&Register, &h, 1_000).0 {
            Verdict::NotLinearizable { longest, stuck } => {
                assert_eq!(longest, vec![0]);
                assert_eq!(stuck, vec![1]);
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn reads_may_not_go_back_in_time() {
        // Two sequential reads see 1 then 0 while one write of 1 overlaps both: impossible.
        let h = vec![
            op(0, 0, Some(100), RegIn::Write(1), Some(0)),
            op(1, 1, Some(2), RegIn::Read, Some(1)),
            op(1, 3, Some(4), RegIn::Read, Some(0)),
        ];
        assert!(!check(&Register, &h, 1_000).0.is_linearizable());
    }

    #[test]
    fn unknown_outcomes_may_or_may_not_take_effect() {
        // A write with no response: a later read may see it ...
        let seen = vec![
            op(0, 0, None, RegIn::Write(7), None),
            op(1, 5, Some(6), RegIn::Read, Some(7)),
        ];
        assert!(check(&Register, &seen, 1_000).0.is_linearizable());
        // ... or not ...
        let unseen = vec![
            op(0, 0, None, RegIn::Write(7), None),
            op(1, 5, Some(6), RegIn::Read, Some(0)),
        ];
        assert!(check(&Register, &unseen, 1_000).0.is_linearizable());
        // ... but not before it was invoked.
        let early = vec![
            op(1, 0, Some(1), RegIn::Read, Some(7)),
            op(0, 2, None, RegIn::Write(7), None),
        ];
        assert!(!check(&Register, &early, 1_000).0.is_linearizable());
    }

    #[test]
    fn the_exploration_limit_is_reported() {
        let h: Vec<_> = (0..12)
            .map(|k| op(k, k as u64, Some(1_000), RegIn::Write(k as u8 + 1), Some(0)))
            .chain([op(99, 2_000, Some(2_001), RegIn::Read, Some(200))])
            .collect();
        assert_eq!(check(&Register, &h, 50).0, Verdict::Undecided { limit: 50 });
    }

    fn id(n: u8) -> ObjectId {
        ObjectId::new(ObjectKind::Changeset, [n; 32])
    }

    fn cid(n: u8) -> ChangeId {
        ChangeId::new([n; 16])
    }

    fn main_ref() -> RefName {
        RefName::new("main").unwrap()
    }

    fn spec() -> RefSpec {
        // main starts at id(1) v1. Change 1 (source 10) landed as 11 on 1; change 2 (source
        // 20) landed as 21 on 11.
        RefSpec {
            initial: vec![(main_ref(), id(1), 1)],
            results: [
                ((main_ref(), cid(1)), id(11)),
                ((main_ref(), cid(2)), id(21)),
            ]
            .into_iter()
            .collect(),
            parents: [(id(11), id(1)), (id(21), id(11))].into_iter().collect(),
        }
    }

    fn land(c: u8, src: u8) -> RefInput {
        RefInput::Land {
            name: main_ref(),
            stack: vec![(cid(c), id(src))],
        }
    }

    fn landed(version: u64, result: u8, skipped: bool) -> Option<RefOutput> {
        Some(RefOutput::Landed {
            version,
            results: vec![id(result)],
            skipped,
        })
    }

    fn rop(
        client: usize,
        call: u64,
        ret: Option<u64>,
        input: RefInput,
        output: Option<RefOutput>,
    ) -> Operation<RefInput, RefOutput> {
        Operation {
            client,
            call,
            ret,
            input,
            output,
        }
    }

    #[test]
    fn concurrent_lands_linearize_in_result_order() {
        let h = vec![
            rop(0, 0, Some(10), land(2, 20), landed(3, 21, false)),
            rop(1, 1, Some(9), land(1, 10), landed(2, 11, false)),
            rop(
                2,
                11,
                Some(12),
                RefInput::Read { name: main_ref() },
                Some(RefOutput::Read {
                    target: Some(id(21)),
                    version: 3,
                }),
            ),
            // A retry of change 1 is answered from the landed index.
            rop(1, 13, Some(14), land(1, 10), landed(2, 11, true)),
        ];
        let (v, _) = check(&spec(), &h, 10_000);
        assert_eq!(v, Verdict::Linearizable(vec![1, 0, 2, 3]));
    }

    #[test]
    fn a_result_not_built_on_the_tip_it_landed_on_is_caught() {
        // Change 2's result sits on 11, but here it is the first land after the initial tip.
        let h = vec![
            rop(0, 0, Some(1), land(2, 20), landed(2, 21, false)),
            rop(1, 2, Some(3), land(1, 10), landed(3, 11, false)),
        ];
        assert!(!check(&spec(), &h, 10_000).0.is_linearizable());
    }

    #[test]
    fn a_lost_response_land_is_placed_where_the_index_says() {
        let h = vec![
            rop(0, 0, None, land(1, 10), None),
            rop(1, 5, Some(6), land(2, 20), landed(3, 21, false)),
        ];
        assert!(check(&spec(), &h, 10_000).0.is_linearizable());
        // Two different sources for one change: only one may land.
        let h = vec![
            rop(0, 0, Some(1), land(1, 10), landed(2, 11, false)),
            rop(1, 2, Some(3), land(1, 12), Some(RefOutput::AlreadyLanded)),
        ];
        assert!(check(&spec(), &h, 10_000).0.is_linearizable());
        // A duplicate land reported as new is caught.
        let h = vec![
            rop(0, 0, Some(1), land(1, 10), landed(2, 11, false)),
            rop(1, 2, Some(3), land(1, 10), landed(3, 11, false)),
        ];
        assert!(!check(&spec(), &h, 10_000).0.is_linearizable());
    }

    #[test]
    fn cas_and_partitioning() {
        let ws = RefName::new("ws/a").unwrap();
        let cas = |expected, target: u8| RefInput::Cas {
            name: ws.clone(),
            expected,
            target: id(target),
        };
        let h = vec![
            rop(
                0,
                0,
                Some(5),
                cas(Expected::Absent, 50),
                Some(RefOutput::CasOk { version: 1 }),
            ),
            rop(
                1,
                1,
                Some(6),
                cas(Expected::Absent, 51),
                Some(RefOutput::CasMismatch { current: 1 }),
            ),
            rop(
                2,
                7,
                Some(8),
                cas(Expected::Version(1), 52),
                Some(RefOutput::CasOk { version: 2 }),
            ),
            rop(3, 2, Some(3), land(1, 10), landed(2, 11, false)),
            rop(4, 9, None, cas(Expected::Version(2), 53), None),
            rop(
                5,
                10,
                Some(11),
                RefInput::Read { name: ws.clone() },
                Some(RefOutput::Read {
                    target: Some(id(53)),
                    version: 3,
                }),
            ),
        ];
        let (v, stats) = check_partitioned(&spec(), h.clone(), |i| i.name().clone(), 10_000);
        assert!(v.is_linearizable(), "{v:?}");
        assert_eq!((stats.operations, stats.ambiguous), (6, 1));
        // A lost update: both absent-creates succeed.
        let mut bad = h;
        bad[1].output = Some(RefOutput::CasOk { version: 1 });
        let (v, _) = check_partitioned(&spec(), bad, |i| i.name().clone(), 10_000);
        assert!(!v.is_linearizable());
    }
}
