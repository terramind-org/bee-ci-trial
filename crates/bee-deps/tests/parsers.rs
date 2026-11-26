//! Golden tests for every manifest and lockfile parser, plus checks of the facts the goldens
//! are meant to protect (direct versus indirect, scope, resolved edges).

use crate::common;
use bee_deps::{DependencyGraph, Ecosystem, Package, Relationship, Scope, SourceFile};
use common::{assert_golden, dump, fixture_graph};

fn get<'a>(g: &'a DependencyGraph, name: &str) -> &'a Package {
    g.packages()
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no package {name}"))
}

fn golden(name: &str) -> DependencyGraph {
    let g = fixture_graph(name);
    assert_golden(name, &dump(&g));
    for m in g.manifests() {
        assert!(m.error.is_none(), "{}: {:?}", m.path, m.error);
    }
    g
}

#[test]
fn cargo_workspace_with_lock() {
    let g = golden("cargo");
    assert_eq!(get(&g, "serde").requirement.as_deref(), Some("1.0.200"));
    assert_eq!(get(&g, "regex").relationship, Relationship::Direct);
    assert_eq!(get(&g, "aho-corasick").relationship, Relationship::Indirect);
    assert_eq!(get(&g, "tempfile").scope, Scope::Development);
    // memchr is needed at runtime through regex even though tempfile (dev) also pulls it in.
    assert_eq!(get(&g, "memchr").scope, Scope::Runtime);
    // Workspace members are the project, not dependencies.
    assert!(
        g.packages()
            .iter()
            .all(|p| p.name != "app" && p.name != "util")
    );
}

#[test]
fn npm_lockfile_v3() {
    let g = golden("npm-v3");
    assert_eq!(get(&g, "express").relationship, Relationship::Direct);
    assert_eq!(get(&g, "accepts").relationship, Relationship::Indirect);
    assert_eq!(get(&g, "jest").scope, Scope::Development);
    // Two mime-types versions: the nested one belongs to jest, the hoisted one to accepts.
    let mimes: Vec<_> = g
        .packages()
        .iter()
        .filter(|p| p.name == "mime-types")
        .collect();
    assert_eq!(mimes.len(), 2);
    let accepts = get(&g, "accepts");
    assert!(
        accepts
            .dependencies
            .contains(&"pkg:npm/mime-types@2.1.35".to_owned())
    );
    let jest = get(&g, "jest");
    assert!(
        jest.dependencies
            .contains(&"pkg:npm/mime-types@2.1.0".to_owned())
    );
    assert_eq!(accepts.licenses, vec!["MIT".to_owned()]);
}

#[test]
fn npm_lockfile_v1_nested_resolution() {
    let g = golden("npm-v1");
    let mocha = get(&g, "mocha");
    assert_eq!(mocha.scope, Scope::Development);
    assert!(
        mocha
            .dependencies
            .contains(&"pkg:npm/minimist@0.0.8".to_owned())
    );
    let minimists: Vec<_> = g
        .packages()
        .iter()
        .filter(|p| p.name == "minimist")
        .collect();
    assert_eq!(minimists.len(), 2);
    let old = minimists.iter().find(|p| p.version == "0.0.8").unwrap();
    assert_eq!(old.scope, Scope::Development);
    assert_eq!(old.relationship, Relationship::Indirect);
}

#[test]
fn pnpm_v9_and_v6() {
    let g = golden("pnpm-v9");
    assert_eq!(get(&g, "react").relationship, Relationship::Direct);
    assert_eq!(get(&g, "loose-envify").relationship, Relationship::Indirect);
    assert_eq!(get(&g, "typescript").scope, Scope::Development);
    let g6 = golden("pnpm-v6");
    assert_eq!(get(&g6, "@babel/core").version, "7.23.0");
    assert_eq!(get(&g6, "semver").relationship, Relationship::Indirect);
    assert_eq!(get(&g6, "rimraf").scope, Scope::Development);
    assert_eq!(get(&g6, "@babel/core").purl, "pkg:npm/%40babel/core@7.23.0");
}

#[test]
fn yarn_v1_and_berry() {
    let g = golden("yarn-v1");
    assert_eq!(
        get(&g, "@babel/highlight").relationship,
        Relationship::Indirect
    );
    assert_eq!(get(&g, "debug").scope, Scope::Development);
    // ms is declared at runtime and also needed by dev-only debug: runtime wins.
    assert_eq!(get(&g, "ms").scope, Scope::Runtime);
    let b = golden("yarn-berry");
    assert_eq!(get(&b, "is-number").relationship, Relationship::Indirect);
    assert_eq!(get(&b, "is-odd").scope, Scope::Development);
    assert_eq!(get(&b, "left-pad").version, "1.3.0");
    assert!(b.packages().iter().all(|p| p.name != "berry-app"));
}

#[test]
fn python_requirements_and_locks() {
    let g = golden("python-req");
    let req = get(&g, "requests");
    assert!(req.resolved);
    assert_eq!(req.version, "2.25.0");
    assert_eq!(req.integrity.as_deref(), Some("sha256:aaaaaaaa"));
    let django = get(&g, "Django");
    assert!(!django.resolved);
    assert_eq!(django.version, "3.2");
    assert_eq!(django.purl, "pkg:pypi/django@3.2");
    assert_eq!(get(&g, "pytest").scope, Scope::Development);
    let manifest = |p: &str| g.manifests().iter().find(|m| m.path == p).unwrap();
    assert!(
        !manifest("requirements.txt").is_lockfile,
        "Django is a range, so not a lockfile"
    );

    let p = golden("poetry");
    assert_eq!(get(&p, "flask").relationship, Relationship::Direct);
    assert_eq!(get(&p, "click").relationship, Relationship::Indirect);
    assert_eq!(get(&p, "pytest").scope, Scope::Development);

    let u = golden("uv");
    assert_eq!(get(&u, "httpx").relationship, Relationship::Direct);
    assert_eq!(
        get(&u, "annotated-types").relationship,
        Relationship::Indirect
    );
    assert_eq!(get(&u, "ruff").scope, Scope::Development);
    assert!(u.packages().iter().all(|p| p.name != "agent-tool"));

    let pf = golden("pipenv");
    assert_eq!(get(&pf, "black").scope, Scope::Development);
    assert_eq!(get(&pf, "jinja2").version, "3.0.1");
}

#[test]
fn go_modules() {
    let g = golden("go");
    assert_eq!(
        get(&g, "github.com/gin-gonic/gin").relationship,
        Relationship::Direct
    );
    assert_eq!(
        get(&g, "golang.org/x/text").relationship,
        Relationship::Indirect
    );
    // The replaced-by-path module is local and not a dependency.
    assert!(
        g.packages()
            .iter()
            .all(|p| p.name != "github.com/local/lib")
    );
    let yaml = get(&g, "gopkg.in/yaml.v3");
    assert_eq!(yaml.integrity.as_deref(), Some("h1:yamlv3="));
    assert_eq!(
        yaml.purl,
        "pkg:golang/gopkg.in/yaml.v3@v3.0.0-20210107192922-496545a6307b"
    );
}

#[test]
fn ruby_gemfile_lock() {
    let g = golden("ruby");
    assert_eq!(get(&g, "rails").relationship, Relationship::Direct);
    assert_eq!(get(&g, "actionpack").relationship, Relationship::Indirect);
    assert_eq!(get(&g, "rspec-rails").scope, Scope::Development);
    assert_eq!(get(&g, "nokogiri").version, "1.14.0");
    // rack is needed at runtime through actionpack even though rspec-rails (dev) also uses it.
    assert_eq!(get(&g, "rack").scope, Scope::Runtime);
}

#[test]
fn maven_pom_with_parent_and_properties() {
    let g = golden("maven");
    assert_eq!(
        get(&g, "org.apache.logging.log4j:log4j-core").version,
        "2.14.1"
    );
    assert_eq!(get(&g, "com.google.guava:guava").version, "30.1-jre");
    assert_eq!(get(&g, "junit:junit").scope, Scope::Development);
    let r = get(&g, "org.example:ranged");
    assert!(!r.resolved);
    assert_eq!(r.version, "1.0");
}

#[test]
fn gradle_lockfile_with_catalog() {
    let g = golden("gradle");
    assert_eq!(
        get(&g, "com.squareup.okhttp3:okhttp").relationship,
        Relationship::Direct
    );
    assert_eq!(
        get(&g, "com.squareup.okio:okio").relationship,
        Relationship::Indirect
    );
    assert_eq!(
        get(&g, "org.junit.jupiter:junit-jupiter").scope,
        Scope::Development
    );
}

#[test]
fn nuget_lock_and_project() {
    let g = golden("nuget");
    assert_eq!(
        get(&g, "System.Diagnostics.DiagnosticSource").relationship,
        Relationship::Indirect
    );
    let sl: Vec<_> = g
        .packages()
        .iter()
        .filter(|p| p.name == "Serilog")
        .collect();
    assert_eq!(
        sl.len(),
        2,
        "one from the lock, one from the csproj via Directory.Packages.props"
    );
    assert!(sl.iter().all(|p| p.version == "3.1.1"));
}

#[test]
fn composer_lock() {
    let g = golden("composer");
    assert_eq!(
        get(&g, "guzzlehttp/guzzle").relationship,
        Relationship::Direct
    );
    assert_eq!(
        get(&g, "psr/http-client").relationship,
        Relationship::Indirect
    );
    let phpunit = get(&g, "phpunit/phpunit");
    assert_eq!(phpunit.scope, Scope::Development);
    assert_eq!(phpunit.version, "10.0.19");
    assert_eq!(get(&g, "monolog/monolog").licenses, vec!["MIT".to_owned()]);
    assert!(g.packages().iter().all(|p| p.name != "php"));
}

#[test]
fn github_actions_workflow() {
    let g = golden("actions");
    assert_eq!(g.packages().len(), 3);
    let a = get(&g, "actions/checkout");
    assert_eq!(a.version, "v3");
    assert_eq!(a.purl, "pkg:githubactions/actions/checkout@v3");
    assert_eq!(a.ecosystem, Ecosystem::GitHubActions);
    assert!(
        g.packages()
            .iter()
            .any(|p| p.name == "acme/shared/.github/workflows/release.yml")
    );
}

#[test]
fn bad_files_are_reported_not_fatal() {
    let g = DependencyGraph::from_files(&[
        SourceFile::new("package.json", "{not json"),
        SourceFile::new("Cargo.toml", "[dependencies]\nserde = \"1\"\n"),
        SourceFile::new("README.md", "ignored"),
    ]);
    let bad = g
        .manifests()
        .iter()
        .find(|m| m.path == "package.json")
        .unwrap();
    assert!(bad.error.is_some());
    assert_eq!(g.packages().len(), 1);
    assert_eq!(g.packages()[0].name, "serde");
}

#[test]
fn input_order_does_not_change_the_graph() {
    let mut files = common::fixture_files("cargo");
    files.extend(common::fixture_files("npm-v3").into_iter().map(|mut f| {
        f.path = format!("web/{}", f.path);
        f
    }));
    let a = DependencyGraph::from_files(&files);
    files.reverse();
    let b = DependencyGraph::from_files(&files);
    assert_eq!(a, b);
    assert!(a.packages().len() > 10);
}

#[test]
fn dependency_path_reaches_transitive_package() {
    let g = fixture_graph("cargo");
    let target = get(&g, "aho-corasick");
    let path = g.dependency_path(target);
    assert_eq!(
        path,
        vec!["pkg:cargo/regex@1.10.4", "pkg:cargo/aho-corasick@1.1.3"]
    );
}
