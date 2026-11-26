//! Reference blame over one file's linear history.
//!
//! Lines are split on `\n`, terminators kept (as in SPEC §7.3.1). Each version is compared
//! with the previous one by a longest common subsequence; matched lines keep their
//! attribution, every other line is attributed to the version that introduced it. Among
//! equally long common subsequences the one matching the earliest old lines is chosen, so the
//! result is deterministic. Deleting the file forgets its history.

/// Splits bytes into lines, keeping each `\n`.
pub fn lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in data.iter().enumerate() {
        if *b == b'\n' {
            out.push(&data[start..=i]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// Pairs `(old index, new index)` of one longest common subsequence.
fn lcs(old: &[&[u8]], new: &[&[u8]]) -> Vec<(usize, usize)> {
    let (n, m) = (old.len(), new.len());
    // table[i][j] = LCS length of old[i..] and new[j..]
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if old[i] == new[j] && table[i][j] == table[i + 1][j + 1] + 1 {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if table[i][j + 1] >= table[i + 1][j] {
            j += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Blames the last version of a history. `history` is `(commit, content)` in order, with
/// `None` content meaning the file is absent at that commit. Returns, for each line of the
/// final content, the commit that introduced it and the line.
pub fn blame<C: Clone>(history: &[(C, Option<Vec<u8>>)]) -> Vec<(C, Vec<u8>)> {
    let mut current: Vec<(C, Vec<u8>)> = Vec::new();
    for (commit, content) in history {
        let Some(content) = content else {
            current.clear();
            continue;
        };
        let new_lines = lines(content);
        let old_lines: Vec<&[u8]> = current.iter().map(|(_, l)| l.as_slice()).collect();
        let pairs = lcs(&old_lines, &new_lines);
        let mut next: Vec<(C, Vec<u8>)> = new_lines
            .iter()
            .map(|l| (commit.clone(), l.to_vec()))
            .collect();
        for (i, j) in pairs {
            next[j].0 = current[i].0.clone();
        }
        current = next;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_edits() {
        let h = vec![
            (1, Some(b"a\nb\n".to_vec())),
            (2, Some(b"a\nb\nc\n".to_vec())),
            (3, Some(b"a\nB\nc\n".to_vec())),
        ];
        let b = blame(&h);
        let who: Vec<i32> = b.iter().map(|(c, _)| *c).collect();
        assert_eq!(who, vec![1, 3, 2]);
        assert_eq!(b[1].1, b"B\n");
        let gone = vec![
            (1, Some(b"x".to_vec())),
            (2, None),
            (3, Some(b"x".to_vec())),
        ];
        assert_eq!(blame(&gone), vec![(3, b"x".to_vec())]);
        assert_eq!(lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert!(lines(b"").is_empty());
    }
}
