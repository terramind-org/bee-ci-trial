//! The dependency graph data model: ecosystems, packages, scopes and manifests.
//!
//! The types mirror `bee.v1.Dependency` and `bee.v1.DependencyManifest` in
//! `proto/bee/v1/security.proto` so the forge can map them field for field.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::purl::Purl;

/// A package ecosystem the crate understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ecosystem {
    /// Rust crates (crates.io): Cargo.toml and Cargo.lock.
    Cargo,
    /// JavaScript packages: package.json, package-lock.json, pnpm-lock.yaml, yarn.lock.
    Npm,
    /// Python distributions (PyPI): requirements, poetry, uv, Pipfile, pyproject.
    PyPI,
    /// Go modules: go.mod and go.sum.
    Go,
    /// Ruby gems: Gemfile.lock.
    RubyGems,
    /// Maven coordinates (`group:artifact`): pom.xml, Gradle lockfiles and version catalogs.
    Maven,
    /// .NET packages: packages.lock.json and project files.
    NuGet,
    /// PHP packages (Packagist): composer.lock.
    Composer,
    /// GitHub Actions referenced from workflow files.
    GitHubActions,
}

impl Ecosystem {
    /// Every ecosystem, in declaration order.
    pub const ALL: [Ecosystem; 9] = [
        Ecosystem::Cargo,
        Ecosystem::Npm,
        Ecosystem::PyPI,
        Ecosystem::Go,
        Ecosystem::RubyGems,
        Ecosystem::Maven,
        Ecosystem::NuGet,
        Ecosystem::Composer,
        Ecosystem::GitHubActions,
    ];

    /// The name used by the OSV schema (`crates.io`, `npm`, `PyPI`, ...).
    pub fn osv_name(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "crates.io",
            Ecosystem::Npm => "npm",
            Ecosystem::PyPI => "PyPI",
            Ecosystem::Go => "Go",
            Ecosystem::RubyGems => "RubyGems",
            Ecosystem::Maven => "Maven",
            Ecosystem::NuGet => "NuGet",
            Ecosystem::Composer => "Packagist",
            Ecosystem::GitHubActions => "GitHub Actions",
        }
    }

    /// The name used in `bee.v1.Dependency.ecosystem` (`cargo`, `npm`, `pip`, `maven`, `go`, ...).
    pub fn proto_name(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::PyPI => "pip",
            Ecosystem::Go => "go",
            Ecosystem::RubyGems => "rubygems",
            Ecosystem::Maven => "maven",
            Ecosystem::NuGet => "nuget",
            Ecosystem::Composer => "composer",
            Ecosystem::GitHubActions => "actions",
        }
    }

    /// The purl type for this ecosystem.
    pub fn purl_type(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::PyPI => "pypi",
            Ecosystem::Go => "golang",
            Ecosystem::RubyGems => "gem",
            Ecosystem::Maven => "maven",
            Ecosystem::NuGet => "nuget",
            Ecosystem::Composer => "composer",
            Ecosystem::GitHubActions => "githubactions",
        }
    }

    /// Looks an ecosystem up by its OSV name. An OSV ecosystem may carry a suffix after a colon
    /// (`Maven:https://repo`, `PyPI:...`); the suffix is ignored.
    pub fn from_osv_name(name: &str) -> Option<Ecosystem> {
        let base = name.split(':').next().unwrap_or(name);
        Ecosystem::ALL.into_iter().find(|e| e.osv_name() == base)
    }

    /// Looks an ecosystem up by its [`proto_name`](Ecosystem::proto_name).
    pub fn from_proto_name(name: &str) -> Option<Ecosystem> {
        Ecosystem::ALL.into_iter().find(|e| e.proto_name() == name)
    }

    /// Looks an ecosystem up by purl type.
    pub fn from_purl_type(ty: &str) -> Option<Ecosystem> {
        Ecosystem::ALL.into_iter().find(|e| e.purl_type() == ty)
    }

    /// The canonical form of a package name, used to compare names of one ecosystem.
    ///
    /// PyPI folds case and treats runs of `-`, `_` and `.` as one `-` (PEP 503). NuGet, Composer
    /// and GitHub Actions names are case-insensitive. Other ecosystems are case-sensitive.
    pub fn normalize_name(self, name: &str) -> String {
        match self {
            Ecosystem::PyPI => {
                let mut out = String::with_capacity(name.len());
                let mut sep = false;
                for c in name.chars() {
                    if matches!(c, '-' | '_' | '.') {
                        sep = true;
                    } else {
                        if sep && !out.is_empty() {
                            out.push('-');
                        }
                        sep = false;
                        out.extend(c.to_lowercase());
                    }
                }
                out
            }
            Ecosystem::NuGet | Ecosystem::Composer | Ecosystem::GitHubActions => {
                name.to_lowercase()
            }
            _ => name.to_owned(),
        }
    }

    /// Builds the purl for a package of this ecosystem.
    ///
    /// Maven names are `group:artifact`; Go names are module paths; GitHub Actions names are
    /// `owner/repo` or `owner/repo/path` (the path becomes the purl subpath).
    pub fn purl(self, name: &str, version: &str) -> Purl {
        let version = if version.is_empty() {
            None
        } else {
            Some(version)
        };
        let mut p = match self {
            Ecosystem::Maven => match name.split_once(':') {
                Some((g, a)) => {
                    let mut p = Purl::simple("maven", a, version);
                    p.namespace = vec![g.to_owned()];
                    p
                }
                None => Purl::simple("maven", name, version),
            },
            Ecosystem::Go | Ecosystem::Npm | Ecosystem::Composer => {
                let mut segs: Vec<&str> = name.split('/').collect();
                let last = segs.pop().unwrap_or(name);
                let mut p = Purl::simple(self.purl_type(), last, version);
                p.namespace = segs.iter().map(|s| (*s).to_owned()).collect();
                if self == Ecosystem::Composer {
                    p.name = p.name.to_lowercase();
                    p.namespace = p.namespace.iter().map(|s| s.to_lowercase()).collect();
                }
                p
            }
            Ecosystem::GitHubActions => {
                let mut segs = name.split('/');
                let owner = segs.next().unwrap_or("");
                let repo = segs.next().unwrap_or("");
                let mut p = Purl::simple("githubactions", repo, version);
                p.namespace = vec![owner.to_owned()];
                p.subpath = segs.map(str::to_owned).collect();
                p
            }
            Ecosystem::PyPI => Purl::simple("pypi", &self.normalize_name(name), version),
            _ => Purl::simple(self.purl_type(), name, version),
        };
        p.ty = self.purl_type().to_owned();
        p
    }
}

/// How a dependency is used (`bee.v1.DependencyScope`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Needed at run time or to build the product.
    Runtime,
    /// Needed only for development, tests or tooling.
    Development,
}

/// Whether a dependency is declared directly (`bee.v1.DependencyRelationship`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Relationship {
    /// Declared in a manifest of the project.
    Direct,
    /// Pulled in by another dependency.
    Indirect,
}

/// One package in the dependency graph (`bee.v1.Dependency`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// The ecosystem.
    pub ecosystem: Ecosystem,
    /// The package name in the ecosystem's own form (`group:artifact` for Maven).
    pub name: String,
    /// The resolved version, or the lowest version the declared requirement allows when
    /// [`resolved`](Package::resolved) is false. Empty when nothing is known.
    pub version: String,
    /// The package URL, `pkg:type/namespace/name@version`.
    pub purl: String,
    /// Runtime or development use.
    pub scope: Scope,
    /// Direct or indirect.
    pub relationship: Relationship,
    /// The manifest or lockfile that declared the package.
    pub source_path: String,
    /// The version requirement declared by the project (direct dependencies only).
    pub requirement: Option<String>,
    /// True when the version came from a lockfile or an exact pin.
    pub resolved: bool,
    /// Purls of the packages this one depends on, sorted.
    pub dependencies: Vec<String>,
    /// SPDX license expressions known from the lockfile, sorted.
    pub licenses: Vec<String>,
    /// A content hash or integrity string from the lockfile, if any.
    pub integrity: Option<String>,
}

impl Package {
    /// A resolved, direct, runtime package with no edges.
    pub fn new(ecosystem: Ecosystem, name: &str, version: &str, source_path: &str) -> Package {
        Package {
            ecosystem,
            name: name.to_owned(),
            version: version.to_owned(),
            purl: ecosystem.purl(name, version).to_string(),
            scope: Scope::Runtime,
            relationship: Relationship::Direct,
            source_path: source_path.to_owned(),
            requirement: None,
            resolved: true,
            dependencies: Vec::new(),
            licenses: Vec::new(),
            integrity: None,
        }
    }

    /// The canonical form of the name (see [`Ecosystem::normalize_name`]).
    pub fn normalized_name(&self) -> String {
        self.ecosystem.normalize_name(&self.name)
    }

    fn sort_key(&self) -> (&str, Ecosystem, String, &str) {
        (
            &self.source_path,
            self.ecosystem,
            self.normalized_name(),
            &self.version,
        )
    }
}

/// A manifest or lockfile the detector found (`bee.v1.DependencyManifest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestInfo {
    /// Repo-relative path.
    pub path: String,
    /// The ecosystem.
    pub ecosystem: Ecosystem,
    /// True for lockfiles, false for manifests.
    pub is_lockfile: bool,
    /// How many graph packages this file contributed.
    pub dependency_count: u32,
    /// Why the file could not be parsed, when it could not.
    pub error: Option<String>,
}

/// A resolved dependency graph for one revision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DependencyGraph {
    packages: Vec<Package>,
    manifests: Vec<ManifestInfo>,
}

impl DependencyGraph {
    /// Builds a graph from packages and manifests, sorting both into canonical order.
    pub fn new(mut packages: Vec<Package>, mut manifests: Vec<ManifestInfo>) -> DependencyGraph {
        for p in &mut packages {
            p.dependencies.sort();
            p.dependencies.dedup();
            p.licenses.sort();
            p.licenses.dedup();
        }
        packages.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        packages.dedup_by(|a, b| a.source_path == b.source_path && a.purl == b.purl);
        manifests.sort_by(|a, b| a.path.cmp(&b.path));
        DependencyGraph {
            packages,
            manifests,
        }
    }

    /// Every package, sorted by source file, ecosystem, name and version.
    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    /// Every detected manifest and lockfile, sorted by path.
    pub fn manifests(&self) -> &[ManifestInfo] {
        &self.manifests
    }

    /// True when the graph has no packages.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Mutable access to the packages, for enrichment such as licenses.
    pub fn packages_mut(&mut self) -> &mut [Package] {
        &mut self.packages
    }

    /// The packages whose purl equals `purl`, one per declaring file.
    pub fn find_by_purl<'a>(&'a self, purl: &'a str) -> impl Iterator<Item = &'a Package> + 'a {
        self.packages.iter().filter(move |p| p.purl == purl)
    }

    /// Packages of one ecosystem and canonical name, any version.
    pub fn find_by_name<'a>(
        &'a self,
        ecosystem: Ecosystem,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Package> + 'a {
        let norm = ecosystem.normalize_name(name);
        self.packages
            .iter()
            .filter(move |p| p.ecosystem == ecosystem && p.normalized_name() == norm)
    }

    /// The shortest chain of purls from a direct dependency down to `package`, inclusive.
    ///
    /// For a direct dependency the path is just its own purl. Ties break toward the
    /// lexicographically smallest purl, so the result is deterministic.
    pub fn dependency_path(&self, package: &Package) -> Vec<String> {
        let same: Vec<&Package> = self
            .packages
            .iter()
            .filter(|p| p.source_path == package.source_path)
            .collect();
        let by_purl: BTreeMap<&str, &Package> =
            same.iter().map(|p| (p.purl.as_str(), *p)).collect();
        let mut prev: BTreeMap<&str, Option<&str>> = BTreeMap::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        for p in &same {
            if p.relationship == Relationship::Direct {
                prev.insert(p.purl.as_str(), None);
                queue.push_back(p.purl.as_str());
            }
        }
        let target = package.purl.as_str();
        while let Some(cur) = queue.pop_front() {
            if cur == target {
                break;
            }
            if let Some(p) = by_purl.get(cur) {
                for d in &p.dependencies {
                    if by_purl.contains_key(d.as_str()) && !prev.contains_key(d.as_str()) {
                        prev.insert(d.as_str(), Some(cur));
                        queue.push_back(d.as_str());
                    }
                }
            }
        }
        if !prev.contains_key(target) {
            return vec![package.purl.clone()];
        }
        let mut path = vec![target.to_owned()];
        let mut cur = target;
        while let Some(Some(p)) = prev.get(cur) {
            path.push((*p).to_owned());
            cur = p;
        }
        path.reverse();
        path
    }

    /// Distinct `(ecosystem, name, version)` triples, for graph comparison and SBOMs.
    pub fn distinct_components(&self) -> BTreeSet<(Ecosystem, String, String)> {
        self.packages
            .iter()
            .map(|p| (p.ecosystem, p.name.clone(), p.version.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purls_per_ecosystem() {
        assert_eq!(
            Ecosystem::Cargo.purl("serde", "1.0.1").to_string(),
            "pkg:cargo/serde@1.0.1"
        );
        assert_eq!(
            Ecosystem::Npm.purl("@babel/core", "7.1.0").to_string(),
            "pkg:npm/%40babel/core@7.1.0"
        );
        assert_eq!(
            Ecosystem::PyPI
                .purl("Django_Rest.Framework", "3.0")
                .to_string(),
            "pkg:pypi/django-rest-framework@3.0"
        );
        assert_eq!(
            Ecosystem::Go
                .purl("github.com/spf13/cobra", "v1.8.0")
                .to_string(),
            "pkg:golang/github.com/spf13/cobra@v1.8.0"
        );
        assert_eq!(
            Ecosystem::Maven
                .purl("org.apache.logging.log4j:log4j-core", "2.14.1")
                .to_string(),
            "pkg:maven/org.apache.logging.log4j/log4j-core@2.14.1"
        );
        assert_eq!(
            Ecosystem::GitHubActions
                .purl("github/codeql-action/init", "v3")
                .to_string(),
            "pkg:githubactions/github/codeql-action@v3#init"
        );
        assert_eq!(
            Ecosystem::Composer
                .purl("Monolog/Monolog", "2.0.0")
                .to_string(),
            "pkg:composer/monolog/monolog@2.0.0"
        );
        assert_eq!(
            Ecosystem::RubyGems.purl("rails", "7.0.0").to_string(),
            "pkg:gem/rails@7.0.0"
        );
        assert_eq!(
            Ecosystem::NuGet
                .purl("Newtonsoft.Json", "13.0.1")
                .to_string(),
            "pkg:nuget/Newtonsoft.Json@13.0.1"
        );
    }

    #[test]
    fn osv_names_round_trip() {
        for e in Ecosystem::ALL {
            assert_eq!(Ecosystem::from_osv_name(e.osv_name()), Some(e));
            assert_eq!(Ecosystem::from_proto_name(e.proto_name()), Some(e));
            assert_eq!(Ecosystem::from_purl_type(e.purl_type()), Some(e));
        }
        assert_eq!(
            Ecosystem::from_osv_name("Maven:https://x"),
            Some(Ecosystem::Maven)
        );
        assert_eq!(Ecosystem::from_osv_name("Debian:12"), None);
    }

    #[test]
    fn dependency_path_is_shortest_and_deterministic() {
        let mut a = Package::new(Ecosystem::Npm, "a", "1.0.0", "package-lock.json");
        let mut b = Package::new(Ecosystem::Npm, "b", "1.0.0", "package-lock.json");
        let mut c = Package::new(Ecosystem::Npm, "c", "1.0.0", "package-lock.json");
        let d = Package::new(Ecosystem::Npm, "d", "1.0.0", "package-lock.json");
        a.dependencies = vec![b.purl.clone(), c.purl.clone()];
        b.relationship = Relationship::Indirect;
        c.relationship = Relationship::Indirect;
        b.dependencies = vec![d.purl.clone()];
        c.dependencies = vec![d.purl.clone()];
        let mut d = d;
        d.relationship = Relationship::Indirect;
        let g = DependencyGraph::new(vec![d.clone(), c, b.clone(), a.clone()], vec![]);
        let path = g.dependency_path(&d);
        assert_eq!(path, vec![a.purl, b.purl, d.purl]);
    }
}
