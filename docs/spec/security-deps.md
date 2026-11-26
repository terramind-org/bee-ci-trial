# Dependency graph, SBOM, alerts, review and updates

Status: implemented in `crates/bee-deps` (layer 1, depends only on `bee-core`). Scope:
PRODUCT §4.18 (dependency graph, SBOM export, vulnerability alerts, dependency review, automated
security updates; scenario AS-SEC-01). The crate is a pure library: no network, no clock, no
randomness, no hash-map order in any output. The forge supplies the files of a revision, the
advisory dump and the creation time, and maps the results onto `proto/bee/v1/security.proto`.

When security updates are enabled, every open dependency alert without an update change-request
link is reconciled. Raising an alert and enabling the setting therefore have the same result in
either order. The link is durable; subsequent triggers skip linked alerts and do not queue a
second security-update run. The security store's unique live `(repo, entry)` queue lease
enforces that invariant across forge replicas; terminal runs release the lease.

## 1. Dependency graph

`DependencyGraph::from_files(&[SourceFile])` detects files by path, parses every ecosystem and
returns one graph, independent of input order. A file that fails to parse is reported in
`ManifestInfo::error` and adds no packages. Files larger than 64 MiB are reported as `TOO_LARGE`
(`DepsError::to_bee_error`). `node_modules/` and `vendor/` are skipped.

Each `Package` maps to `bee.v1.Dependency`: ecosystem, name, version, purl, scope
(runtime or development), relationship (direct or indirect), the declaring file, edges (purls it
depends on), licenses, an integrity string and whether the version is `resolved` from a lock.

| Ecosystem | Files | Notes |
|---|---|---|
| Cargo | `Cargo.toml`, `Cargo.lock` | Workspace members are the project, not dependencies. `workspace = true` requirements are read from `[workspace.dependencies]`. Dev scope from `[dev-dependencies]`. |
| npm | `package.json`, `package-lock.json` and `npm-shrinkwrap.json` (v1, v2, v3), `pnpm-lock.yaml` (v5, v6, v9), `yarn.lock` (v1 and Berry) | Nested `node_modules` lookup is resolved exactly, so two versions of one package keep separate edges. Workspaces: members' dependencies are roots. |
| Python | `requirements*.txt` (also `requirements/*.txt`, `*-requirements.txt`), `poetry.lock`, `uv.lock`, `Pipfile.lock`, `pyproject.toml` | Hashes, markers, extras and `\` continuations are read. `-r`, `-c`, `-e` and URL requirements are not followed. A requirements file counts as a lockfile when every entry is pinned with `==`. Poetry `^` and `~` become PEP 440 ranges. Names follow PEP 503. |
| Go | `go.mod`, `go.sum` | `// indirect` marks indirect requires. Local `replace` targets are dropped. `go.sum` supplies `h1:` hashes. |
| RubyGems | `Gemfile.lock`, `Gemfile` | Platform suffixes are stripped. Gemfile `group` blocks and `group:` options give development scope. |
| Maven | `pom.xml`, `gradle.lockfile`, `*.versions.toml` | `pom.xml`: declared dependencies, `${property}` interpolation, `dependencyManagement` and parent poms present in the revision. Gradle lock entries are development-only when every configuration contains `test`. Catalog libraries are the direct dependencies of a Gradle lock. |
| NuGet | `packages.lock.json`, `*.csproj` (`.fsproj`, `.vbproj`), `Directory.Packages.props` | Central versions and `$(Property)` versions are resolved; floating versions use their lowest bound. Test projects are development scope. |
| Composer | `composer.lock`, `composer.json` | Platform requirements (`php`, `ext-*`) are skipped. `packages-dev` is development. |
| GitHub Actions | `.github/workflows/*.yml`, `.github/actions/**/action.yml` | Every distinct `owner/repo[/path]@ref` of a `uses:`. `./` and `docker://` are skipped. |

Direct versus indirect: a lockfile is authoritative where it records roots (npm v2/v3, pnpm,
uv, `go.mod`, `Gemfile.lock` `DEPENDENCIES`); otherwise the project manifests under the lock's
directory name the roots. A package no root reaches is indirect. With no roots at all, packages
nothing depends on are direct. Scope: a package is runtime when any runtime root reaches it.
Without a lockfile the version is the exact pin, else the lowest version the requirement allows
(`resolved = false`).

Package URLs follow the purl spec (`pkg:cargo/…`, `pkg:npm/%40scope/name@v`, `pkg:pypi/…`,
`pkg:golang/…`, `pkg:gem/…`, `pkg:maven/group/artifact@v`, `pkg:nuget/…`, `pkg:composer/…`,
`pkg:githubactions/owner/repo@ref#path`). `Purl::parse` rejects malformed URLs, for the
submission API's `INVALID_ARGUMENT`.

Version ordering (`version::compare`) is a total order per ecosystem: SemVer (Cargo, npm, Go
pseudo-versions, NuGet, Composer), PEP 440, Maven `ComparableVersion`, RubyGems `Gem::Version`.
Unparseable versions sort below parseable ones. `version::satisfies` evaluates npm, Cargo,
Composer, PEP 440, RubyGems, Maven and NuGet requirement grammars.

## 2. SBOM

`sbom::spdx_json` produces SPDX 2.3 JSON; `sbom::cyclonedx_json` produces CycloneDX 1.6 JSON.
Both are validated in tests against the published schemas vendored in
`crates/bee-deps/tests/schemas/` (SPDX 2.3.1 from the `spdx-spec` repository, CC0; CycloneDX
1.6 with its SPDX and JSF companions, Apache-2.0).

Output depends only on the graph and `SbomOptions` (name, creation time, namespace base, tool).
Components are sorted by purl and merged across files. The SPDX `documentNamespace` and the
CycloneDX `serialNumber` derive from a hash of the components and edges. SPDX: root package
`SPDXRef-Root`; direct runtime dependencies `DEPENDS_ON`, direct development dependencies
`DEV_DEPENDENCY_OF` the root, transitive edges `DEPENDS_ON`. CycloneDX: `metadata.component` is
`root`, components use the purl as `bom-ref`, scope is `required` or `optional`, and
`bee:scope` carries the exact scope. Several licenses on one package (Composer) are an `OR`
expression. npm/SRI and `sha256:` integrity strings become component hashes.

## 3. Vulnerability matching

Advisories are OSV records (`osv::Advisory`): `affected[].ranges` of type `SEMVER`, `ECOSYSTEM`
and `GIT`, explicit `versions`, CVSS v3 and v4 severities and `database_specific.severity`.
`AdvisorySource` is the lookup trait; `FileAdvisorySource` loads OSV JSON offline (one record, an
array, newline-delimited, or a directory tree) and never touches the network.

`matching::match_graph` returns one `Finding` per vulnerable package occurrence: advisory id (a
GHSA id when the alias group has one), aliases (CVE, GHSA, vendor), package, installed version,
fixed versions newer than the installed one, severity, CVSS score and vector, scope,
relationship, declaring file and the shortest dependency path from a direct dependency. Rules:

- Withdrawn advisories never match. Advisories that alias each other and hit the same package
  are one finding.
- Events are sorted and evaluated per the OSV algorithm (`introduced`, `fixed`, `last_affected`;
  `0` means from the start). `SEMVER` ranges always use SemVer; `ECOSYSTEM` ranges use the
  package ecosystem's ordering (PEP 440, Maven, RubyGems, Go pseudo-versions, npm and Cargo
  SemVer).
- `GIT` ranges need a commit: a Go pseudo-version commit or a hex version (a pinned action SHA).
  With a `GitAncestry` oracle the range is evaluated by ancestry; without one only an exact
  `introduced` or `last_affected` commit matches.
- Severity is the highest scoring parsable CVSS v3 or v4 vector, else the label. Scores use the
  first.org equations (v3.0 and v3.1 rounding) and the v4.0 MacroVector method with its table.
  v3 temporal and environmental metrics are ignored; v4 threat and environmental metrics are
  honored. CVSS v2 vectors are ignored.

## 4. Dependency review

`review::review(base, head, source, policy, opts)` diffs two graphs per manifest and package:
added, removed and changed (old and new version paired in version order). Each change lists the
vulnerabilities that affect the head version and did not affect the package at base
(`vulnerabilities`), the advisories the change fixes (`fixed_advisories`), licenses at both
sides, `license_changed` and `license_denied`. `ReviewPolicy`: fail at or above a severity
(default low), development scope ignored unless `include_development`, deny and allow lists of
SPDX ids evaluated against the parsed expression (`OR` needs one acceptable side, `AND` all),
optional denial of unknown licenses. `DependencyReview::to_check_result()` is the required-check
output: conclusion, title and a Markdown table. The structure maps to `bee.v1.DependencyReview`.

`license::LicenseExpr` parses SPDX expressions (`AND`, `OR`, `WITH`, `+`, parentheses, `LicenseRef-`);
identifiers are checked for shape, not against the SPDX list.

## 5. Update planner

`update::plan_security_updates(files, graph, findings, source)` returns an `UpdatePlan`: groups
per ecosystem (the unit of a grouped change request), each with updates and structured
`TextEdit`s (`path`, byte span, `old`, `new`), lockfile commands and notes, plus `unfixable`
findings with a reason. `apply_edits` and `apply_plan` apply edits and refuse a file whose text
no longer matches. The target version is the lowest published fix for all findings of a package
that no known advisory still affects; pre-releases are skipped unless already installed.

| Ecosystem | Edit | Command |
|---|---|---|
| Cargo | `Cargo.toml` requirement (plain, table `version`, renamed `package`, `[workspace.dependencies]`) | `cargo update -p name@from --precise to` |
| npm | `package.json` `dependencies`, `devDependencies`, `optionalDependencies` | `npm install name@to --package-lock-only`, `pnpm update`, `yarn up` or `yarn upgrade` |
| pip | the pin or lower bound in `requirements*.txt`, in place | none |
| Go | the `require` version in `go.mod`, direct or indirect | `go mod tidy` |

Rules: with a lockfile, a direct dependency whose declared range already allows the target needs
only the command; otherwise the requirement is edited keeping its operator (`^`, `~`, `=`,
`>=`, none). Without a lockfile the declared minimum is the version in use, so it is raised.
Poetry, uv and Pipfile locks get a refresh command and no manifest edit.

Limits: only single-comparator requirements are edited; `||`, hyphen and space-joined ranges,
x-ranges, tags, URLs, caps below the fix and `==1.*` pins are reported as unfixable rather than
guessed. A transitive dependency is refreshed through the lock and succeeds only when the
parents' ranges allow the target. Maven, Gradle, NuGet, Composer, RubyGems and Actions findings
are reported as unfixable ("does not support"). Lockfiles are never edited as text (integrity
hashes would go stale); the bot runs the command. Scheduled version updates and `dependabot.yml`
parsing are not part of this crate; the planner supplies the security-update core.

## 6. Other limits

- Pom resolution has no transitive dependencies, no BOM imports and no profiles; Gradle build
  scripts are not read.
- Yarn Berry `patch:` entries and pnpm `link:`/`file:`/`workspace:` dependencies are skipped.
- Licenses come from lockfiles only (npm, Composer); enrich them with
  `DependencyGraph::packages_mut` when a registry lookup is available.
- The YAML reader (`yaml`) is a subset reader for lockfiles (no anchors, tags or multi-line
  plain scalars); the XML reader (`xml`) resolves no namespaces or external entities.
