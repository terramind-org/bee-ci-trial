//! Rule 9: structural `file_directory` conflicts.

use std::collections::BTreeSet;

/// A file path that is also a directory in the same candidate result.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileDirectoryConflict {
    /// The path that is a file.
    pub file: Vec<u8>,
    /// The paths under `file/` in the same result, sorted bytewise.
    pub under: Vec<Vec<u8>>,
}

/// Finds rule-9 collisions in the candidate result of a merge.
///
/// `paths` are the non-directory paths the merged tree would contain (`/`-separated, no
/// leading or trailing `/`): every clean entry plus, for each conflicting path, the path itself
/// when any side has it there. A file on one side where the other side has a directory, or a
/// directory that one side replaced by a file while the other side added or kept entries under
/// it, shows up as a path `p` together with paths under `p/`. Each such `p` is reported once,
/// in bytewise order, with everything under it.
pub fn file_directory_conflicts<'p, I>(paths: I) -> Vec<FileDirectoryConflict>
where
    I: IntoIterator<Item = &'p [u8]>,
{
    let set: BTreeSet<&[u8]> = paths.into_iter().collect();
    let mut out = Vec::new();
    for &p in &set {
        let mut prefix = p.to_vec();
        prefix.push(b'/');
        let under: Vec<Vec<u8>> = set
            .range::<[u8], _>((
                std::ops::Bound::Included(&prefix[..]),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|q| q.starts_with(&prefix))
            .map(|q| q.to_vec())
            .collect();
        if !under.is_empty() {
            out.push(FileDirectoryConflict {
                file: p.to_vec(),
                under,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_free_sets_have_no_conflicts() {
        let paths: Vec<&[u8]> = vec![b"a/b", b"a/c", b"ab", b"a.b/c"];
        assert!(file_directory_conflicts(paths).is_empty());
    }

    #[test]
    fn nested_collisions_are_all_reported() {
        let paths: Vec<&[u8]> = vec![b"a", b"a/b", b"a/b/c", b"a-x", b"z"];
        assert_eq!(
            file_directory_conflicts(paths),
            vec![
                FileDirectoryConflict {
                    file: b"a".to_vec(),
                    under: vec![b"a/b".to_vec(), b"a/b/c".to_vec()]
                },
                FileDirectoryConflict {
                    file: b"a/b".to_vec(),
                    under: vec![b"a/b/c".to_vec()]
                },
            ]
        );
    }
}
