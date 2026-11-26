//! Semantic diff classes (SPEC §11.3).
//!
//! Every changed path gets one [`Class`]; a set of changes gets the highest class present.
//! Classification is a pure function of the changes and the [`ClassRules`] (SPEC §9.4 supplies
//! the generated and sensitive path lists), so it carries no clock, randomness or I/O. The full
//! algorithm is specified in SPEC §11.3 and versioned by [`CLASSIFIER_VERSION`].

mod rules;
mod strip;

use bee_core::{Mode, RepoPath};

pub use rules::{ClassRules, DEFAULT_DEPENDENCIES, DEFAULT_DOCS, DEFAULT_TESTS, GlobSet};
pub use strip::{Language, normalize};

/// The classifier version recorded next to every stored classification.
pub const CLASSIFIER_VERSION: &str = "classify-v1";

/// A semantic diff class, declared from lowest to highest risk (`Ord` follows that order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Matches a generated-path rule from policy.
    Generated,
    /// Documentation paths.
    Docs,
    /// Only comments and whitespace changed.
    CommentOnly,
    /// Only test paths changed.
    TestOnly,
    /// A content-identical move.
    RenameOnly,
    /// Dependency manifests and lockfiles.
    Dependency,
    /// Anything else.
    Logic,
    /// Touches a path policy marks sensitive.
    Sensitive,
}

impl Class {
    /// Every class, lowest risk first.
    pub const ALL: [Class; 8] = [
        Class::Generated,
        Class::Docs,
        Class::CommentOnly,
        Class::TestOnly,
        Class::RenameOnly,
        Class::Dependency,
        Class::Logic,
        Class::Sensitive,
    ];

    /// The stable wire string (the `diff_class` field).
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Generated => "generated",
            Class::Docs => "docs",
            Class::CommentOnly => "comment_only",
            Class::TestOnly => "test_only",
            Class::RenameOnly => "rename_only",
            Class::Dependency => "dependency",
            Class::Logic => "logic",
            Class::Sensitive => "sensitive",
        }
    }

    /// Parses the wire string.
    pub fn parse(text: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

/// Why a path got its class: the rule that decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Basis {
    /// The path (or the path it was renamed from) matches a sensitive rule.
    SensitivePath,
    /// The path matches a generated rule.
    GeneratedPath,
    /// The path matches a docs rule.
    DocsPath,
    /// The path matches a test rule.
    TestPath,
    /// The path is a dependency manifest or lockfile.
    DependencyPath,
    /// Nothing changed (same path, bytes and mode).
    Unchanged,
    /// After removing comments and whitespace the two sides are equal.
    CommentsAndWhitespace,
    /// Same bytes and mode at a different path.
    IdenticalMove,
    /// No rule applied.
    Fallback,
}

/// The bytes and mode of a file on one side of a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content {
    /// File bytes (the target for a symlink).
    pub bytes: Vec<u8>,
    /// Tree entry mode.
    pub mode: Mode,
}

impl Content {
    /// A regular file.
    pub fn file(bytes: impl Into<Vec<u8>>) -> Content {
        Content {
            bytes: bytes.into(),
            mode: Mode::File,
        }
    }
}

/// One changed path. `path` is the path in the new tree, or the deleted path when `new` is
/// `None`. Rename detection is the caller's job: a pair is passed as one change with
/// `renamed_from` set and `old` holding the content at that earlier path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    /// The changed path.
    pub path: RepoPath,
    /// Content before the change; `None` when the path is added.
    pub old: Option<Content>,
    /// Content after the change; `None` when the path is deleted.
    pub new: Option<Content>,
    /// The earlier path, when this change is a rename or move.
    pub renamed_from: Option<RepoPath>,
}

/// The class of one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    /// The path that was classified.
    pub path: RepoPath,
    /// Its class.
    pub class: Class,
    /// The rule that decided it.
    pub basis: Basis,
}

/// The classes of a set of changes and the highest-risk one present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeClassification {
    /// One entry per input change, in input order.
    pub per_path: Vec<Classification>,
    /// The maximum class in `per_path`; `None` for an empty change.
    pub highest: Option<Class>,
}

/// Classifies one path change.
pub fn classify_path(input: PathChange, rules: &ClassRules) -> Classification {
    classify_ref(&input, rules)
}

/// Classifies every path and reports the highest-risk class.
pub fn classify_change(paths: &[PathChange], rules: &ClassRules) -> ChangeClassification {
    let per_path: Vec<Classification> = paths.iter().map(|p| classify_ref(p, rules)).collect();
    let highest = per_path.iter().map(|c| c.class).max();
    ChangeClassification { per_path, highest }
}

fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8000)];
    head.contains(&0) || std::str::from_utf8(bytes).is_err()
}

fn classify_ref(p: &PathChange, rules: &ClassRules) -> Classification {
    let done = |class, basis| Classification {
        path: p.path.clone(),
        class,
        basis,
    };
    let name = |r: &RepoPath| String::from_utf8_lossy(r.as_bytes()).into_owned();
    let path = name(&p.path);
    let from = p.renamed_from.as_ref().map(name);
    let involved: Vec<&str> = std::iter::once(path.as_str())
        .chain(from.as_deref())
        .collect();
    let all = |set: &GlobSet| involved.iter().all(|x| set.matches(x));
    let any = |set: &GlobSet| involved.iter().any(|x| set.matches(x));

    if any(&rules.sensitive) {
        return done(Class::Sensitive, Basis::SensitivePath);
    }
    if all(&rules.generated) {
        return done(Class::Generated, Basis::GeneratedPath);
    }

    let binary = p
        .old
        .iter()
        .chain(p.new.iter())
        .any(|c| is_binary(&c.bytes));
    let mut best = (Class::Logic, Basis::Fallback);
    let mut offer = |class: Class, basis: Basis| {
        if class < best.0 {
            best = (class, basis);
        }
    };

    if all(&rules.dependencies) {
        if !binary {
            offer(Class::Dependency, Basis::DependencyPath);
        }
    } else {
        if all(&rules.docs) {
            offer(Class::Docs, Basis::DocsPath);
        }
        if all(&rules.tests) && !binary {
            offer(Class::TestOnly, Basis::TestPath);
        }
    }

    let moved = p.renamed_from.as_ref().is_some_and(|f| *f != p.path);
    match (&p.old, &p.new) {
        (Some(o), Some(n)) if o == n && !moved => offer(Class::CommentOnly, Basis::Unchanged),
        (Some(o), Some(n)) if o == n => offer(Class::RenameOnly, Basis::IdenticalMove),
        _ => {}
    }
    if !binary && comment_only(p, &path, from.as_deref()) {
        // A move never classifies below `rename_only`, even when comments changed with it.
        let class = if moved {
            Class::RenameOnly
        } else {
            Class::CommentOnly
        };
        offer(class, Basis::CommentsAndWhitespace);
    }
    done(best.0, best.1)
}

/// True when both sides are text in the same known language, are plain files, and normalize to
/// the same canonical form. An absent side counts as empty.
fn comment_only(p: &PathChange, path: &str, from: Option<&str>) -> bool {
    let plain = |c: &Content| matches!(c.mode, Mode::File | Mode::Executable);
    if p.old.is_none() && p.new.is_none() {
        return false;
    }
    if !p.old.iter().chain(p.new.iter()).all(plain) {
        return false;
    }
    if let (Some(o), Some(n)) = (&p.old, &p.new)
        && o.mode != n.mode
    {
        return false;
    }
    let (Some(new_lang), Some(old_lang)) = (
        Language::detect(path),
        Language::detect(from.unwrap_or(path)),
    ) else {
        return false;
    };
    if new_lang != old_lang {
        return false;
    }
    let canon = |c: &Option<Content>| -> Option<String> {
        match c {
            None => Some(String::new()),
            Some(c) => std::str::from_utf8(&c.bytes)
                .ok()
                .map(|t| normalize(new_lang, t)),
        }
    };
    match (canon(&p.old), canon(&p.new)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}
