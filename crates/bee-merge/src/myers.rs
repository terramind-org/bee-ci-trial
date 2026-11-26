//! Exact forward greedy Myers diff over interned line tokens.
//!
//! The contract (`docs/contracts/diff3-v1.md` §2) fixes every choice:
//!
//! - Round `d` computes, for each diagonal `k = x - y` in `-d..=d` step 2, the furthest
//!   reaching x after `d` edits, followed by the longest snake of equal lines.
//! - A candidate move is valid only if it stays inside the edit grid (`x <= n`, `y <= m`).
//! - When both moves are valid, the insertion move (from diagonal `k + 1`) is taken iff
//!   `V[k+1] > V[k-1]`; on a tie (`V[k-1] == V[k+1]`) the deletion move is taken.
//! - The first round that reaches `(n, m)` ends the search; the path is recovered backwards
//!   by replaying the same choices.
//! - If round [`MAX_EDIT_DISTANCE`] completes without reaching `(n, m)`, only the common
//!   prefix and suffix are matched.
//!
//! The trace of furthest-reaching values is stored in full while it fits a fixed budget.
//! Beyond that the search is run again keeping only every `K`-th round, and blocks of rounds
//! are recomputed during backtracking. Both modes replay identical rounds, so the result is
//! the same; only memory and time differ.

/// Marks an unreachable diagonal in a round.
const INVALID: i64 = -1;

/// Maximum number of stored trace entries before switching to checkpointed rounds.
const FULL_TRACE_BUDGET: usize = 1 << 21;

/// Largest edit distance searched exactly (contract §2.2). Beyond it the diff falls back to
/// matching only the common prefix and suffix, which bounds the work on adversarial inputs.
pub(crate) const MAX_EDIT_DISTANCE: usize = 1 << 14;

/// Returns the matched line pairs `(i, j)` (`a[i] == b[j]`) of the forward greedy Myers path,
/// in increasing order.
pub(crate) fn matched_pairs(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    matched_pairs_capped(a, b, MAX_EDIT_DISTANCE)
}

/// The common prefix and suffix as matched pairs (the suffix never overlaps the prefix).
fn prefix_suffix_pairs(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (0..prefix)
        .map(|i| (i, i))
        .chain((0..suffix).map(|j| (n - suffix + j, m - suffix + j)))
        .collect()
}

fn matched_pairs_capped(a: &[u32], b: &[u32], cap: usize) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 || !shares_a_token(a, b) {
        // No path through the grid can contain a match.
        return Vec::new();
    }
    let grid = Grid { a, b };

    // Pass 1: keep every round while the trace stays within budget.
    let mut rounds: Vec<Vec<i64>> = Vec::new();
    let mut stored = 0usize;
    let mut keep = true;
    let mut prev: Vec<i64> = Vec::new();
    let mut d = 0usize;
    let end_index = loop {
        let mut cur = Vec::with_capacity(d + 1);
        let done = grid.round(d, &prev, &mut cur);
        if keep {
            stored += cur.len();
            if stored > FULL_TRACE_BUDGET {
                keep = false;
                rounds.clear();
                rounds.shrink_to_fit();
            } else {
                rounds.push(cur.clone());
            }
        }
        if let Some(i) = done {
            break i;
        }
        if d == cap {
            return prefix_suffix_pairs(a, b);
        }
        prev = cur;
        d += 1;
    };
    let total = d;

    let trace = if keep {
        Trace::full(rounds)
    } else {
        // Pass 2: replay the search keeping only every K-th round.
        let interval = isqrt(total) + 1;
        let mut checkpoints = Vec::new();
        let mut prev: Vec<i64> = Vec::new();
        for d in 0..=total {
            let mut cur = Vec::with_capacity(d + 1);
            grid.round(d, &prev, &mut cur);
            if d % interval == 0 {
                checkpoints.push(cur.clone());
            }
            prev = cur;
        }
        Trace::checkpointed(interval, checkpoints)
    };
    grid.backtrack(trace, total, end_index)
}

/// Integer square root (floor).
fn isqrt(v: usize) -> usize {
    let mut r = (v as f64).sqrt() as usize;
    while r * r > v {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= v {
        r += 1;
    }
    r
}

/// True when some token occurs in both sequences.
fn shares_a_token(a: &[u32], b: &[u32]) -> bool {
    let max = a.iter().copied().max().unwrap_or(0) as usize;
    let mut seen = vec![false; max + 1];
    for &t in a {
        seen[t as usize] = true;
    }
    b.iter()
        .any(|&t| (t as usize) < seen.len() && seen[t as usize])
}

/// The move that entered a diagonal in a round.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Move {
    /// Down: insert `b[y]`, coming from diagonal `k + 1`.
    Insert,
    /// Right: delete `a[x]`, coming from diagonal `k - 1`.
    Delete,
}

struct Grid<'s> {
    a: &'s [u32],
    b: &'s [u32],
}

impl Grid<'_> {
    fn n(&self) -> i64 {
        self.a.len() as i64
    }

    fn m(&self) -> i64 {
        self.b.len() as i64
    }

    /// Chooses the move into slot `i` of round `d` (diagonal `k = 2i - d`), given round
    /// `d - 1`. Returns the move and the x reached by it, before the snake.
    fn choose(&self, d: usize, i: usize, prev: &[i64]) -> Option<(Move, i64)> {
        let k = 2 * i as i64 - d as i64;
        // Insertion: from diagonal k + 1, which is slot i of round d - 1.
        let down = (i < d)
            .then(|| prev[i])
            .filter(|&x| x != INVALID && x - k <= self.m());
        // Deletion: from diagonal k - 1, which is slot i - 1 of round d - 1.
        let right = (i >= 1)
            .then(|| prev[i - 1])
            .filter(|&x| x != INVALID && x < self.n());
        match (down, right) {
            (Some(xd), Some(xr)) => {
                if xr < xd {
                    Some((Move::Insert, xd))
                } else {
                    Some((Move::Delete, xr + 1))
                }
            }
            (Some(xd), None) => Some((Move::Insert, xd)),
            (None, Some(xr)) => Some((Move::Delete, xr + 1)),
            (None, None) => None,
        }
    }

    /// Follows equal lines diagonally from `(x, y)`.
    fn snake(&self, mut x: i64, mut y: i64) -> i64 {
        while x < self.n() && y < self.m() && self.a[x as usize] == self.b[y as usize] {
            x += 1;
            y += 1;
        }
        x
    }

    /// Computes round `d` into `cur` from round `d - 1` (`prev`, empty for `d == 0`).
    /// Returns the slot that reached `(n, m)`, if any.
    fn round(&self, d: usize, prev: &[i64], cur: &mut Vec<i64>) -> Option<usize> {
        cur.clear();
        if d == 0 {
            let x = self.snake(0, 0);
            cur.push(x);
            return (x == self.n() && x == self.m()).then_some(0);
        }
        let mut done = None;
        for i in 0..=d {
            let k = 2 * i as i64 - d as i64;
            let x = match self.choose(d, i, prev) {
                Some((_, x)) => self.snake(x, x - k),
                None => INVALID,
            };
            if done.is_none() && x == self.n() && x - k == self.m() {
                done = Some(i);
            }
            cur.push(x);
        }
        done
    }

    /// Recovers the matched pairs of the path ending in slot `end` of round `total`.
    fn backtrack(&self, mut trace: Trace, total: usize, end: usize) -> Vec<(usize, usize)> {
        let mut pairs = Vec::new();
        let mut i = end;
        let mut x = self.n();
        let mut d = total;
        while d > 0 {
            let k = 2 * i as i64 - d as i64;
            let prev = trace.round(self, d - 1);
            let (mv, mid_x) = self
                .choose(d, i, prev)
                .expect("a reached slot always has a valid entering move");
            // Snake from (mid_x, mid_x - k) to (x, x - k).
            let mut sx = x;
            while sx > mid_x {
                sx -= 1;
                pairs.push((sx as usize, (sx - k) as usize));
            }
            let prev_i = match mv {
                Move::Insert => i,
                Move::Delete => i - 1,
            };
            x = prev[prev_i];
            i = prev_i;
            d -= 1;
        }
        // Round 0: the initial snake from (0, 0).
        while x > 0 {
            x -= 1;
            pairs.push((x as usize, x as usize));
        }
        pairs.reverse();
        pairs
    }
}

/// Access to stored rounds during backtracking.
struct Trace {
    interval: usize,
    checkpoints: Vec<Vec<i64>>,
    block_start: usize,
    block: Vec<Vec<i64>>,
}

impl Trace {
    fn full(rounds: Vec<Vec<i64>>) -> Self {
        Self {
            interval: 1,
            checkpoints: rounds,
            block_start: usize::MAX,
            block: Vec::new(),
        }
    }

    fn checkpointed(interval: usize, checkpoints: Vec<Vec<i64>>) -> Self {
        Self {
            interval,
            checkpoints,
            block_start: usize::MAX,
            block: Vec::new(),
        }
    }

    /// Returns round `d`, recomputing its block from the nearest checkpoint if needed.
    fn round(&mut self, grid: &Grid<'_>, d: usize) -> &[i64] {
        if self.interval == 1 {
            return &self.checkpoints[d];
        }
        let start = d / self.interval * self.interval;
        if self.block_start != start {
            self.block.clear();
            self.block
                .push(self.checkpoints[start / self.interval].clone());
            // Backtracking asks for the highest round of a block first, so computing up to
            // `d` serves every later request in the same block.
            for r in start + 1..=d {
                let mut cur = Vec::with_capacity(r + 1);
                let prev = self.block.last().expect("block starts with its checkpoint");
                grid.round(r, prev, &mut cur);
                self.block.push(cur);
            }
            self.block_start = start;
        }
        &self.block[d - start]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcs_len(a: &[u32], b: &[u32]) -> usize {
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                dp[i][j] = if a[i] == b[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        dp[0][0]
    }

    fn check(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
        let pairs = matched_pairs(a, b);
        for w in pairs.windows(2) {
            assert!(w[0].0 < w[1].0 && w[0].1 < w[1].1);
        }
        for &(i, j) in &pairs {
            assert_eq!(a[i], b[j]);
        }
        assert_eq!(pairs.len(), lcs_len(a, b), "minimal diff for {a:?} {b:?}");
        pairs
    }

    #[test]
    fn swap_prefers_deleting_first() {
        // a b -> b a keeps "b": delete a, keep b, insert a.
        assert_eq!(check(&[0, 1], &[1, 0]), vec![(1, 0)]);
    }

    #[test]
    fn appended_duplicate_matches_the_first_line() {
        assert_eq!(check(&[0], &[0, 0]), vec![(0, 0)]);
        assert_eq!(check(&[0, 0], &[0]), vec![(0, 0)]);
    }

    #[test]
    fn minimal_on_small_exhaustive_inputs() {
        // Every pair of sequences over {0,1,2} up to length 4.
        let mut seqs: Vec<Vec<u32>> = vec![vec![]];
        for len in 1..=4 {
            let mut v = vec![0u32; len];
            loop {
                seqs.push(v.clone());
                let mut p = 0;
                while p < len && v[p] == 2 {
                    v[p] = 0;
                    p += 1;
                }
                if p == len {
                    break;
                }
                v[p] += 1;
            }
        }
        for a in &seqs {
            for b in &seqs {
                check(a, b);
            }
        }
    }

    #[test]
    fn checkpointed_trace_matches_full_trace() {
        // Replay one search with a full trace and with several checkpoint intervals; every
        // mode must recover the same path as the public entry point.
        let a: Vec<u32> = (0..1200).map(|i| (i * 7 % 13) as u32).collect();
        let b: Vec<u32> = (0..1200).map(|i| (i * 5 % 11) as u32).collect();
        let pairs = check(&a, &b);
        let grid = Grid { a: &a, b: &b };
        let mut rounds = Vec::new();
        let mut prev = Vec::new();
        let mut d = 0;
        let end = loop {
            let mut cur = Vec::new();
            let done = grid.round(d, &prev, &mut cur);
            rounds.push(cur.clone());
            if let Some(i) = done {
                break i;
            }
            prev = cur;
            d += 1;
        };
        let full = grid.backtrack(Trace::full(rounds.clone()), d, end);
        assert_eq!(full, pairs);
        let interval = isqrt(d) + 1;
        let checkpoints = rounds.iter().step_by(interval).cloned().collect();
        let cp = grid.backtrack(Trace::checkpointed(interval, checkpoints), d, end);
        assert_eq!(cp, pairs);
        for interval in [2, 3, 7] {
            let checkpoints = rounds.iter().step_by(interval).cloned().collect();
            let cp = grid.backtrack(Trace::checkpointed(interval, checkpoints), d, end);
            assert_eq!(cp, pairs, "interval {interval}");
        }
    }

    #[test]
    fn large_edit_distance_uses_checkpoints_and_stays_minimal() {
        // D is about 4000, so the full trace (about D²/2 entries) exceeds the budget.
        let mut a: Vec<u32> = (0..2000).collect();
        let mut b: Vec<u32> = (2000..4000).collect();
        a.insert(1000, 9000);
        b.push(9000);
        a.push(9001);
        b.insert(10, 9001);
        let pairs = check(&a, &b);
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn distance_cap_falls_back_to_prefix_and_suffix() {
        // D = 4 here; with a cap of 3 only the prefix and suffix match.
        let a = [0, 1, 2, 3, 9];
        let b = [0, 4, 2, 5, 9];
        assert_eq!(
            matched_pairs_capped(&a, &b, 4),
            vec![(0, 0), (2, 2), (4, 4)]
        );
        assert_eq!(matched_pairs_capped(&a, &b, 3), vec![(0, 0), (4, 4)]);
        // Suffix never overlaps the prefix.
        assert_eq!(
            prefix_suffix_pairs(&[7, 7], &[7, 7, 7]),
            vec![(0, 0), (1, 1)]
        );
    }

    #[test]
    fn isqrt_is_exact() {
        for v in 0..10_000 {
            let r = isqrt(v);
            assert!(r * r <= v && (r + 1) * (r + 1) > v);
        }
    }
}
