# Pre-publication audit

Date: 2026-11-18
Audited commit: `588a01fadb4d5beb36615240373cbab8449d57fc`
Scope: 190 local and remote refs, 788 reachable commits, and a 43.60 MiB
packed object database.

## Results

| Severity | Location and commit | Finding | Disposition |
| --- | --- | --- | --- |
| **blocks going public** | `.terramind/**`, introduced at `75066ad60a6da6dd66dfac0921f7e8ceeac2f36b`, still updated at `11a1019a0607be4a9a8459f7a63da5b03119f95a` | 219 committed agent-working files: 78 handoffs and 141 screenshots/shots, totalling 23,892,521 bytes. Handoffs contain internal operational context, host URLs, environment findings, and agent transcripts. | Remove these paths and rewrite any public history that retains them before a visibility flip. They should not be public. |
| fix before launch | `harness/agents/{fixtures,scenarios,src}` at `9aa8316e1deee7f6dd55c7d5eda640c90df2471a` | Personal-path patterns occur in agent test fixtures and scenarios. They are synthetic paths, not identified personal home directories. | Keep only after a final wording review; replace realistic names with neutral fixture names if they are not needed by the test. |
| note | `deploy/helm/bee/{ci,values.yaml}` and `scripts/check-helm.sh` at `e56676030a20fe4ca0d5e61e1d8e407849b3366b` | Cloud-project and cluster-name patterns are deployment-test configuration, not live project identifiers. | Keep as synthetic examples; never substitute live identifiers in committed values. |
| note | all refs, gitleaks report outside the worktree | Gitleaks 8.28.0 scanned 586 commit snapshots / 66.49 MB with redaction. It found no unallowlisted credentials. The 44 initial detections were all deliberately invalid test, seed, config, or lockfile examples; none had a plausible live credential format. | No credential rotation is required. `.gitleaks.toml` restricts suppressions to those confirmed invalid examples. |
| note | historical large blobs, primarily `schemas/openapi/{bee,swarm}.v1.json` | 21 blobs are at least 1 MiB; the 20 largest are generated OpenAPI schemas (3.54–3.79 MB each). Git LFS tracks none. | History size is modest, but repeated generated schemas account for much of it. Consider history compaction only as part of the required `.terramind` rewrite; do not rewrite solely for size. |
| note | tracked web assets | 155 tracked raster/vector/font-named assets were inspected by path. Project assets are generated fixture/screenshots or the project icon; no vendored fonts are committed. Geist font packages are OFL-1.1 dependencies, not repository assets. | Preserve package notices in dependency distributions. |

No real secret was found. If a future run finds one, publication is blocked until the
credential is rotated and the affected history is rewritten or otherwise made safe; this
audit does not rewrite history.

## Clean-room provenance

Read `docs/research/*`, `docs/spec/*`, `testdata/`, `corpus/`, committed fixture/media
assets, and vendored-path candidates. Research documents identify their sources and are
analysis rather than copied source material. No vendored directory, copied code/test
fixture, or third-party prose longer than a short identifying phrase was found. `corpus/`
and `testdata/vectors/` contain project-owned build inputs and vectors. The committed
screenshots are internal working artefacts, not third-party assets, and are covered by the
publication blocker above.

## Licence audit

`scripts/audit/licenses.py` read locked Cargo metadata plus installed packages from
`apps/web` and `harness/agents`: 918 package records. It reported no unallowlisted licence.
The two `@img/sharp-libvips` packages are explicitly documented dynamic LGPL shared-library
uses, not static linkage. Rust and npm packages otherwise have Apache-2.0-compatible
licences or an Apache-compatible alternative.

Container bases add their own notices and obligations: the server image uses Rust and Debian
Bookworm during build/runtime; the web image uses Oven Bun for build and Node Alpine at
runtime. Their image licences and SBOMs must be retained in the release image process; this
repository audit does not redistribute those base layers.

## Re-run

```sh
scripts/dev-setup.sh
git fetch --unshallow origin || true
git fetch origin '+refs/heads/*:refs/remotes/origin/*' '+refs/tags/*:refs/tags/*' '+refs/pull/*/head:refs/remotes/pull/*'
scripts/audit/secrets.sh
scripts/audit/licenses.py
```

`secrets.sh` writes its redacted JSON report under the operating-system temporary directory
by default. Set `BEE_SECRETS_REPORT` to an absolute path outside the repository to retain
it. The Bee scanner is a library only (no history-scanning binary or CLI), so it cannot
be run directly over Git history; `cargo test -p bee-secrets` verifies its 38 active tests.

## Deviations

None. This audit reports the internal files and does not rewrite history or remove files
outside its owned paths.
