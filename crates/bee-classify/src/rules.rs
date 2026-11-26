//! Path rules: glob sets and the default docs, test and dependency lists.

use bee_core::{BeeError, Result, Subcode};

/// A set of path globs with the same syntax as policy path globs (SPEC §9.4).
///
/// - `**` as a whole component matches zero or more components.
/// - `*` matches any run of characters inside one component, `?` exactly one character.
/// - A glob is anchored at the repository root; write `**/x` to match at any depth.
/// - A glob matches a path when it matches the path itself or any ancestor directory of it, so
///   `schema` covers `schema/a.sql`.
/// - `[`, `]`, `{`, `}` and `!` are rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobSet {
    globs: Vec<Vec<String>>,
}

impl GlobSet {
    /// Parses every glob, failing with `INVALID_ARGUMENT` on the first malformed one.
    pub fn new<I, S>(globs: I) -> Result<GlobSet>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for raw in globs {
            out.push(parse_glob(raw.as_ref())?);
        }
        Ok(GlobSet { globs: out })
    }

    /// The empty set; it matches nothing.
    pub fn empty() -> GlobSet {
        GlobSet::default()
    }

    /// True when the set has no globs.
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    /// True when any glob matches `path` or one of its ancestor directories.
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 1 && parts[0].is_empty() {
            return false;
        }
        self.globs
            .iter()
            .any(|g| (1..=parts.len()).any(|n| match_components(g, &parts[..n])))
    }
}

fn bad(raw: &str, why: &str) -> BeeError {
    BeeError::invalid(Subcode::InvalidField, format!("glob `{raw}`: {why}"))
}

fn parse_glob(raw: &str) -> Result<Vec<String>> {
    if let Some(c) = raw
        .chars()
        .find(|c| matches!(c, '[' | ']' | '{' | '}' | '!'))
    {
        return Err(bad(raw, &format!("unsupported character `{c}`")));
    }
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Err(bad(raw, "empty"));
    }
    let mut parts: Vec<String> = Vec::new();
    for c in trimmed.split('/') {
        if c.is_empty() {
            return Err(bad(raw, "empty component"));
        }
        if c.contains("**") && c != "**" {
            return Err(bad(raw, "`**` must be a whole component"));
        }
        if c == "**" && parts.last().is_some_and(|l| l == "**") {
            continue;
        }
        parts.push(c.to_string());
    }
    Ok(parts)
}

fn match_components(pattern: &[String], parts: &[&str]) -> bool {
    match pattern.split_first() {
        None => parts.is_empty(),
        Some((head, rest)) if head == "**" => {
            (0..=parts.len()).any(|skip| match_components(rest, &parts[skip..]))
        }
        Some((head, rest)) => match parts.split_first() {
            Some((part, tail)) => match_component(head, part) && match_components(rest, tail),
            None => false,
        },
    }
}

fn match_component(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Documentation paths used by [`ClassRules::default`].
pub const DEFAULT_DOCS: &[&str] = &[
    "**/*.md",
    "**/*.markdown",
    "**/*.mdx",
    "**/*.rst",
    "**/*.txt",
    "**/*.adoc",
    "**/doc",
    "**/docs",
    "**/LICENSE*",
    "**/LICENCE*",
    "**/COPYING*",
    "**/NOTICE*",
    "**/AUTHORS*",
    "**/CHANGELOG*",
    "**/CONTRIBUTING*",
    "**/README*",
];

/// Test paths used by [`ClassRules::default`].
pub const DEFAULT_TESTS: &[&str] = &[
    "**/tests",
    "**/test",
    "**/__tests__",
    "**/spec",
    "**/testdata",
    "**/fixtures",
    "**/*_test.go",
    "**/*_test.py",
    "**/test_*.py",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.test.js",
    "**/*.test.jsx",
    "**/*.spec.ts",
    "**/*.spec.tsx",
    "**/*.spec.js",
    "**/*.spec.jsx",
    "**/*_spec.rb",
    "**/*_test.rb",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*Test.kt",
    "**/*Tests.cs",
    "**/*_test.rs",
    "**/*_test.cc",
    "**/*_test.cpp",
];

/// Dependency manifests and lockfiles used by [`ClassRules::default`].
pub const DEFAULT_DEPENDENCIES: &[&str] = &[
    "**/Cargo.toml",
    "**/Cargo.lock",
    "**/package.json",
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/bun.lockb",
    "**/go.mod",
    "**/go.sum",
    "**/requirements*.txt",
    "**/constraints*.txt",
    "**/pyproject.toml",
    "**/poetry.lock",
    "**/uv.lock",
    "**/Pipfile",
    "**/Pipfile.lock",
    "**/setup.py",
    "**/setup.cfg",
    "**/Gemfile",
    "**/Gemfile.lock",
    "**/*.gemspec",
    "**/pom.xml",
    "**/build.gradle",
    "**/build.gradle.kts",
    "**/settings.gradle",
    "**/settings.gradle.kts",
    "**/gradle.lockfile",
    "**/*.csproj",
    "**/*.fsproj",
    "**/*.vbproj",
    "**/packages.lock.json",
    "**/Directory.Packages.props",
    "**/composer.json",
    "**/composer.lock",
    "**/mix.exs",
    "**/mix.lock",
    "**/Package.swift",
    "**/Package.resolved",
    "**/Podfile",
    "**/Podfile.lock",
    "**/pubspec.yaml",
    "**/pubspec.lock",
];

/// The path rules a classification runs against. Policy supplies `generated` and `sensitive`
/// (SPEC §9.4); the others default to broad per-ecosystem lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassRules {
    /// Generated paths (lockfiles, snapshots, codegen); always from policy.
    pub generated: GlobSet,
    /// Documentation paths.
    pub docs: GlobSet,
    /// Test paths.
    pub tests: GlobSet,
    /// Dependency manifests and lockfiles.
    pub dependencies: GlobSet,
    /// Sensitive paths (auth, payments, policy itself, CI config); always from policy.
    pub sensitive: GlobSet,
}

impl ClassRules {
    /// Default docs, test and dependency lists with the given policy-supplied generated and
    /// sensitive globs.
    pub fn with_policy<G, S, A, B>(generated: G, sensitive: S) -> Result<ClassRules>
    where
        G: IntoIterator<Item = A>,
        S: IntoIterator<Item = B>,
        A: AsRef<str>,
        B: AsRef<str>,
    {
        Ok(ClassRules {
            generated: GlobSet::new(generated)?,
            sensitive: GlobSet::new(sensitive)?,
            ..ClassRules::default()
        })
    }
}

impl Default for ClassRules {
    fn default() -> ClassRules {
        let set = |g: &[&str]| GlobSet::new(g.iter().copied()).unwrap_or_default();
        ClassRules {
            generated: GlobSet::empty(),
            docs: set(DEFAULT_DOCS),
            tests: set(DEFAULT_TESTS),
            dependencies: set(DEFAULT_DEPENDENCIES),
            sensitive: GlobSet::empty(),
        }
    }
}
