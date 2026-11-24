//! Manifest and lockfile detection and parsing into a [`DependencyGraph`].
//!
//! [`DependencyGraph::from_files`] takes every file of a revision, picks out the ones it
//! understands by path, parses each ecosystem, and returns one graph. A file that fails to parse
//! is reported in [`ManifestInfo::error`] and contributes no packages; it never fails the whole
//! graph.

use crate::error::{DepsError, MAX_FILE_BYTES};
use crate::model::{DependencyGraph, Ecosystem, ManifestInfo, Package};

pub(crate) mod actions;
pub(crate) mod build;
pub(crate) mod cargo;
pub(crate) mod composer;
pub(crate) mod golang;
pub(crate) mod jvm;
pub(crate) mod npm;
pub(crate) mod nuget;
pub(crate) mod python;
pub(crate) mod ruby;

/// A file of a revision: its repo-relative path (`/` separated) and UTF-8 content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    /// Repo-relative path.
    pub path: String,
    /// File content.
    pub content: String,
}

impl SourceFile {
    /// Builds a source file.
    pub fn new(path: &str, content: &str) -> SourceFile {
        SourceFile {
            path: path.to_owned(),
            content: content.to_owned(),
        }
    }
}

/// What a recognized file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    CargoToml,
    CargoLock,
    PackageJson,
    PackageLock,
    PnpmLock,
    YarnLock,
    Requirements,
    PoetryLock,
    UvLock,
    PipfileLock,
    PyProject,
    GoMod,
    GoSum,
    GemfileLock,
    Gemfile,
    PomXml,
    GradleLock,
    VersionCatalog,
    NugetLock,
    Csproj,
    DirectoryPackages,
    ComposerLock,
    ComposerJson,
    Workflow,
}

impl Kind {
    pub(crate) fn ecosystem(self) -> Ecosystem {
        match self {
            Kind::CargoToml | Kind::CargoLock => Ecosystem::Cargo,
            Kind::PackageJson | Kind::PackageLock | Kind::PnpmLock | Kind::YarnLock => {
                Ecosystem::Npm
            }
            Kind::Requirements
            | Kind::PoetryLock
            | Kind::UvLock
            | Kind::PipfileLock
            | Kind::PyProject => Ecosystem::PyPI,
            Kind::GoMod | Kind::GoSum => Ecosystem::Go,
            Kind::GemfileLock | Kind::Gemfile => Ecosystem::RubyGems,
            Kind::PomXml | Kind::GradleLock | Kind::VersionCatalog => Ecosystem::Maven,
            Kind::NugetLock | Kind::Csproj | Kind::DirectoryPackages => Ecosystem::NuGet,
            Kind::ComposerLock | Kind::ComposerJson => Ecosystem::Composer,
            Kind::Workflow => Ecosystem::GitHubActions,
        }
    }

    pub(crate) fn is_lockfile(self) -> bool {
        matches!(
            self,
            Kind::CargoLock
                | Kind::PackageLock
                | Kind::PnpmLock
                | Kind::YarnLock
                | Kind::PoetryLock
                | Kind::UvLock
                | Kind::PipfileLock
                | Kind::GoSum
                | Kind::GemfileLock
                | Kind::GradleLock
                | Kind::NugetLock
                | Kind::ComposerLock
        )
    }
}

/// Recognizes a dependency file by its path.
pub(crate) fn classify(path: &str) -> Option<Kind> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let in_dir = |d: &str| path.split('/').rev().skip(1).any(|s| s == d);
    if path.starts_with("node_modules/")
        || path.contains("/node_modules/")
        || path.contains("/vendor/")
    {
        return None;
    }
    Some(match base {
        "Cargo.toml" => Kind::CargoToml,
        "Cargo.lock" => Kind::CargoLock,
        "package.json" => Kind::PackageJson,
        "package-lock.json" | "npm-shrinkwrap.json" => Kind::PackageLock,
        "pnpm-lock.yaml" => Kind::PnpmLock,
        "yarn.lock" => Kind::YarnLock,
        "poetry.lock" => Kind::PoetryLock,
        "uv.lock" => Kind::UvLock,
        "Pipfile.lock" => Kind::PipfileLock,
        "pyproject.toml" => Kind::PyProject,
        "go.mod" => Kind::GoMod,
        "go.sum" => Kind::GoSum,
        "Gemfile.lock" => Kind::GemfileLock,
        "Gemfile" => Kind::Gemfile,
        "pom.xml" => Kind::PomXml,
        "gradle.lockfile" => Kind::GradleLock,
        "packages.lock.json" => Kind::NugetLock,
        "Directory.Packages.props" => Kind::DirectoryPackages,
        "composer.lock" => Kind::ComposerLock,
        "composer.json" => Kind::ComposerJson,
        b if b.ends_with(".versions.toml") => Kind::VersionCatalog,
        b if b.ends_with(".csproj") || b.ends_with(".fsproj") || b.ends_with(".vbproj") => {
            Kind::Csproj
        }
        b if b.ends_with(".txt")
            && (b.starts_with("requirements")
                || b.ends_with("-requirements.txt")
                || in_dir("requirements")) =>
        {
            Kind::Requirements
        }
        b if (b.ends_with(".yml") || b.ends_with(".yaml"))
            && (path.starts_with(".github/workflows/") || path.contains("/.github/workflows/"))
            && base == b =>
        {
            Kind::Workflow
        }
        "action.yml" | "action.yaml" if path.contains(".github/actions/") => Kind::Workflow,
        _ => return None,
    })
}

/// One recognized input file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct In<'a> {
    pub path: &'a str,
    pub content: &'a str,
    pub kind: Kind,
}

/// What an ecosystem parser returns.
#[derive(Default)]
pub(crate) struct Out {
    pub packages: Vec<Package>,
    pub manifests: Vec<ManifestInfo>,
}

impl Out {
    /// Records a parsed file.
    pub(crate) fn file(&mut self, input: &In<'_>, count: usize) {
        self.manifests.push(ManifestInfo {
            path: input.path.to_owned(),
            ecosystem: input.kind.ecosystem(),
            is_lockfile: input.kind.is_lockfile(),
            dependency_count: u32::try_from(count).unwrap_or(u32::MAX),
            error: None,
        });
    }

    /// Records a parsed file whose lockfile status is decided by its content.
    pub(crate) fn file_as(&mut self, input: &In<'_>, count: usize, is_lockfile: bool) {
        self.file(input, count);
        if let Some(m) = self.manifests.last_mut() {
            m.is_lockfile = is_lockfile;
        }
    }

    /// Records a file that could not be parsed.
    pub(crate) fn failed(&mut self, input: &In<'_>, err: &DepsError) {
        self.manifests.push(ManifestInfo {
            path: input.path.to_owned(),
            ecosystem: input.kind.ecosystem(),
            is_lockfile: input.kind.is_lockfile(),
            dependency_count: 0,
            error: Some(err.message.clone()),
        });
    }

    /// Adds packages from one source and records the file with the count.
    pub(crate) fn add(&mut self, input: &In<'_>, packages: Vec<Package>) {
        self.file(input, packages.len());
        self.packages.extend(packages);
    }
}

/// The directory of a path, empty for a top-level file.
pub(crate) fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// True when `path` is at or below directory `dir`.
pub(crate) fn within(dir: &str, path: &str) -> bool {
    dir.is_empty() || dir_of(path) == dir || dir_of(path).starts_with(&format!("{dir}/"))
}

/// The deepest of `dirs` that contains `path`.
pub(crate) fn nearest<'a>(dirs: &[&'a str], path: &str) -> Option<&'a str> {
    dirs.iter()
        .copied()
        .filter(|d| within(d, path))
        .max_by_key(|d| d.len())
}

impl DependencyGraph {
    /// Detects and parses every supported manifest and lockfile among `files`.
    ///
    /// Supported: Cargo, npm (package.json, package-lock.json v1-v3, pnpm, Yarn v1 and Berry),
    /// Python (requirements, poetry, uv, Pipfile.lock, pyproject.toml), Go, RubyGems, Maven
    /// (pom.xml, gradle.lockfile, version catalogs), NuGet, Composer and GitHub Actions
    /// workflows. The result is deterministic and independent of the order of `files`.
    pub fn from_files(files: &[SourceFile]) -> DependencyGraph {
        let mut sorted: Vec<&SourceFile> = files.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut inputs: Vec<In<'_>> = Vec::new();
        let mut out = Out::default();
        for f in sorted {
            let Some(kind) = classify(&f.path) else {
                continue;
            };
            let input = In {
                path: &f.path,
                content: &f.content,
                kind,
            };
            if f.content.len() > MAX_FILE_BYTES {
                let e = DepsError {
                    kind: crate::error::DepsErrorKind::TooLarge,
                    path: f.path.clone(),
                    message: format!(
                        "file is {} bytes, above the limit of {MAX_FILE_BYTES}",
                        f.content.len()
                    ),
                };
                out.failed(&input, &e);
                continue;
            }
            inputs.push(input);
        }
        let of = |e: Ecosystem| -> Vec<In<'_>> {
            inputs
                .iter()
                .copied()
                .filter(|i| i.kind.ecosystem() == e)
                .collect()
        };
        let mut merge = |o: Out| {
            out.packages.extend(o.packages);
            out.manifests.extend(o.manifests);
        };
        merge(cargo::parse(&of(Ecosystem::Cargo)));
        merge(npm::parse(&of(Ecosystem::Npm)));
        merge(python::parse(&of(Ecosystem::PyPI)));
        merge(golang::parse(&of(Ecosystem::Go)));
        merge(ruby::parse(&of(Ecosystem::RubyGems)));
        merge(jvm::parse(&of(Ecosystem::Maven)));
        merge(nuget::parse(&of(Ecosystem::NuGet)));
        merge(composer::parse(&of(Ecosystem::Composer)));
        merge(actions::parse(&of(Ecosystem::GitHubActions)));
        DependencyGraph::new(out.packages, out.manifests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths() {
        assert_eq!(classify("Cargo.lock"), Some(Kind::CargoLock));
        assert_eq!(classify("crates/a/Cargo.toml"), Some(Kind::CargoToml));
        assert_eq!(classify("requirements-dev.txt"), Some(Kind::Requirements));
        assert_eq!(classify("requirements/base.txt"), Some(Kind::Requirements));
        assert_eq!(classify("notes.txt"), None);
        assert_eq!(
            classify("gradle/libs.versions.toml"),
            Some(Kind::VersionCatalog)
        );
        assert_eq!(classify("src/App/App.csproj"), Some(Kind::Csproj));
        assert_eq!(classify(".github/workflows/ci.yml"), Some(Kind::Workflow));
        assert_eq!(classify("docs/ci.yml"), None);
        assert_eq!(classify("web/node_modules/x/package.json"), None);
    }

    #[test]
    fn directory_helpers() {
        assert_eq!(dir_of("a/b/c.txt"), "a/b");
        assert_eq!(dir_of("c.txt"), "");
        assert!(within("", "x/y"));
        assert!(within("a", "a/b/c"));
        assert!(!within("a", "ab/c"));
        assert_eq!(nearest(&["", "a", "a/b"], "a/b/c/d.json"), Some("a/b"));
        assert_eq!(nearest(&["a"], "z/d.json"), None);
    }
}
