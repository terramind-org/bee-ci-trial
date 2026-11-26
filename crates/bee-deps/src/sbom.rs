//! SBOM export: SPDX 2.3 JSON and CycloneDX 1.6 JSON from a [`DependencyGraph`].
//!
//! Output is a pure function of the graph and [`SbomOptions`]: components are sorted by purl,
//! identifiers are derived from content, and the creation time is supplied by the caller. A
//! package that appears in several lockfiles is one component.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::license::combine_declared;
use crate::model::{DependencyGraph, Package, Relationship, Scope};

/// Inputs to SBOM generation that are not part of the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbomOptions {
    /// Name of the described project (usually `owner/repo`).
    pub name: String,
    /// Creation time, RFC 3339 UTC with seconds (`2026-01-02T03:04:05Z`). Supplied by the caller
    /// so the output stays deterministic.
    pub created: String,
    /// Base URL for the SPDX `documentNamespace`; a content hash is appended.
    pub namespace_base: String,
    /// The tool name recorded as the creator.
    pub tool_name: String,
    /// The tool version recorded as the creator.
    pub tool_version: String,
}

impl SbomOptions {
    /// Options for `name` created at `created`, with the default namespace and tool.
    pub fn new(name: &str, created: &str) -> SbomOptions {
        SbomOptions {
            name: name.to_owned(),
            created: created.to_owned(),
            namespace_base: "https://beehive.invalid/sbom".to_owned(),
            tool_name: "bee-deps".to_owned(),
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// One component after merging duplicates across files.
struct Component<'a> {
    purl: &'a str,
    pkg: &'a Package,
    scope: Scope,
    direct: bool,
    direct_runtime: bool,
    licenses: BTreeSet<String>,
    depends_on: BTreeSet<&'a str>,
    hashes: BTreeSet<(&'static str, String)>,
}

fn components(g: &DependencyGraph) -> Vec<Component<'_>> {
    let mut map: BTreeMap<&str, Component<'_>> = BTreeMap::new();
    for p in g.packages() {
        let c = map.entry(p.purl.as_str()).or_insert_with(|| Component {
            purl: &p.purl,
            pkg: p,
            scope: Scope::Development,
            direct: false,
            direct_runtime: false,
            licenses: BTreeSet::new(),
            depends_on: BTreeSet::new(),
            hashes: BTreeSet::new(),
        });
        if p.scope == Scope::Runtime {
            c.scope = Scope::Runtime;
        }
        if p.relationship == Relationship::Direct {
            c.direct = true;
            if p.scope == Scope::Runtime {
                c.direct_runtime = true;
            }
        }
        c.licenses.extend(p.licenses.iter().cloned());
        c.depends_on
            .extend(p.dependencies.iter().map(String::as_str));
        if let Some((alg, hex)) = p.integrity.as_deref().and_then(integrity_to_hash) {
            c.hashes.insert((alg, hex));
        }
    }
    map.into_values().collect()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buf >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Converts a lockfile integrity string to `(algorithm, lowercase hex)`.
///
/// Understood forms: npm/SRI `sha512-<base64>` (also sha256, sha1), and `sha256:<hex>`,
/// `sha512:<hex>`, `sha1:<hex>`. Algorithm names are the SPDX spellings (`SHA256`).
pub(crate) fn integrity_to_hash(s: &str) -> Option<(&'static str, String)> {
    let (alg, rest, is_hex) = if let Some((a, r)) = s.split_once(':') {
        (a, r, true)
    } else {
        let (a, r) = s.split_once('-')?;
        (a, r, false)
    };
    let (name, len) = match alg.to_ascii_lowercase().as_str() {
        "sha1" => ("SHA1", 20),
        "sha256" => ("SHA256", 32),
        "sha512" => ("SHA512", 64),
        _ => return None,
    };
    let bytes = if is_hex {
        if rest.len() != len * 2 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return Some((name, rest.to_ascii_lowercase()));
    } else {
        base64_decode(rest)?
    };
    (bytes.len() == len).then(|| (name, hex(&bytes)))
}

fn content_hash(comps: &[Component<'_>], name: &str) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    for c in comps {
        h.update(b"\0");
        h.update(c.purl.as_bytes());
        for d in &c.depends_on {
            h.update(b"\x01");
            h.update(d.as_bytes());
        }
    }
    hex(&h.finalize())
}

fn spdx_id_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn license_expression(c: &Component<'_>) -> Option<String> {
    let list: Vec<String> = c.licenses.iter().cloned().collect();
    combine_declared(&list).map(|e| e.to_string())
}

/// Generates an SPDX 2.3 JSON document.
///
/// The root package `SPDXRef-Root` depends on every direct runtime dependency; direct
/// development dependencies are `DEV_DEPENDENCY_OF` the root; transitive edges are `DEPENDS_ON`.
pub fn spdx_json(g: &DependencyGraph, opts: &SbomOptions) -> Value {
    let comps = components(g);
    let ids: BTreeMap<&str, String> = comps
        .iter()
        .enumerate()
        .map(|(i, c)| {
            (
                c.purl,
                format!(
                    "SPDXRef-Package-{}-{}",
                    i + 1,
                    spdx_id_part(&format!("{}-{}", c.pkg.name, c.pkg.version))
                ),
            )
        })
        .collect();
    let mut packages = vec![json!({
        "SPDXID": "SPDXRef-Root",
        "name": opts.name,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": false,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "NOASSERTION",
        "copyrightText": "NOASSERTION",
        "primaryPackagePurpose": "APPLICATION",
    })];
    let mut relationships = vec![json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": "SPDXRef-Root",
    })];
    for c in &comps {
        let id = &ids[c.purl];
        let declared = license_expression(c).unwrap_or_else(|| "NOASSERTION".to_owned());
        let mut pkg = Map::new();
        pkg.insert("SPDXID".into(), json!(id));
        pkg.insert("name".into(), json!(c.pkg.name));
        if !c.pkg.version.is_empty() {
            pkg.insert("versionInfo".into(), json!(c.pkg.version));
        }
        pkg.insert("downloadLocation".into(), json!("NOASSERTION"));
        pkg.insert("filesAnalyzed".into(), json!(false));
        pkg.insert("licenseConcluded".into(), json!("NOASSERTION"));
        pkg.insert("licenseDeclared".into(), json!(declared));
        pkg.insert("copyrightText".into(), json!("NOASSERTION"));
        pkg.insert("primaryPackagePurpose".into(), json!("LIBRARY"));
        pkg.insert(
            "externalRefs".into(),
            json!([{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": c.purl,
            }]),
        );
        if !c.hashes.is_empty() {
            let sums: Vec<Value> = c
                .hashes
                .iter()
                .map(|(a, v)| json!({"algorithm": a, "checksumValue": v}))
                .collect();
            pkg.insert("checksums".into(), Value::Array(sums));
        }
        packages.push(Value::Object(pkg));
        if c.direct {
            if c.direct_runtime {
                relationships.push(json!({"spdxElementId": "SPDXRef-Root", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": id}));
            } else {
                relationships.push(json!({"spdxElementId": id, "relationshipType": "DEV_DEPENDENCY_OF", "relatedSpdxElement": "SPDXRef-Root"}));
            }
        }
        for d in &c.depends_on {
            if let Some(did) = ids.get(d) {
                relationships.push(json!({"spdxElementId": id, "relationshipType": "DEPENDS_ON", "relatedSpdxElement": did}));
            }
        }
    }
    let hash = content_hash(&comps, &opts.name);
    json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": opts.name,
        "documentNamespace": format!("{}/{}-{}", opts.namespace_base.trim_end_matches('/'), spdx_id_part(&opts.name), &hash[..32]),
        "creationInfo": {
            "created": opts.created,
            "creators": [format!("Tool: {}-{}", opts.tool_name, opts.tool_version)],
        },
        "documentDescribes": ["SPDXRef-Root"],
        "packages": packages,
        "relationships": relationships,
    })
}

/// Generates a CycloneDX 1.6 JSON document.
///
/// The root is `metadata.component` with bom-ref `root`; components use their purl as bom-ref.
pub fn cyclonedx_json(g: &DependencyGraph, opts: &SbomOptions) -> Value {
    let comps = components(g);
    let hash = content_hash(&comps, &opts.name);
    // A name-based UUID: version 5 and the RFC 4122 variant over the content hash.
    let mut b: [u8; 16] = [0; 16];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let u = hex(&b);
    let serial = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &u[0..8],
        &u[8..12],
        &u[12..16],
        &u[16..20],
        &u[20..32]
    );

    let mut components_json = Vec::new();
    let mut deps_json = Vec::new();
    let known: BTreeSet<&str> = comps.iter().map(|c| c.purl).collect();
    let mut root_deps: Vec<&str> = Vec::new();
    for c in &comps {
        let mut o = Map::new();
        o.insert("type".into(), json!("library"));
        o.insert("bom-ref".into(), json!(c.purl));
        o.insert("name".into(), json!(c.pkg.name));
        if !c.pkg.version.is_empty() {
            o.insert("version".into(), json!(c.pkg.version));
        }
        o.insert("purl".into(), json!(c.purl));
        o.insert(
            "scope".into(),
            json!(if c.scope == Scope::Runtime {
                "required"
            } else {
                "optional"
            }),
        );
        if let Some(expr) = license_expression(c) {
            o.insert("licenses".into(), json!([{"expression": expr}]));
        } else if let Some(name) = c.licenses.iter().next() {
            o.insert("licenses".into(), json!([{"license": {"name": name}}]));
        }
        if !c.hashes.is_empty() {
            let hs: Vec<Value> = c
                .hashes
                .iter()
                .map(|(a, v)| {
                    let alg = match *a {
                        "SHA1" => "SHA-1",
                        "SHA256" => "SHA-256",
                        _ => "SHA-512",
                    };
                    json!({"alg": alg, "content": v})
                })
                .collect();
            o.insert("hashes".into(), Value::Array(hs));
        }
        o.insert(
            "properties".into(),
            json!([{"name": "bee:scope", "value": if c.scope == Scope::Runtime { "runtime" } else { "development" }}]),
        );
        components_json.push(Value::Object(o));
        let on: Vec<&str> = c
            .depends_on
            .iter()
            .copied()
            .filter(|d| known.contains(d))
            .collect();
        deps_json.push(json!({"ref": c.purl, "dependsOn": on}));
        if c.direct {
            root_deps.push(c.purl);
        }
    }
    deps_json.insert(0, json!({"ref": "root", "dependsOn": root_deps}));
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": serial,
        "version": 1,
        "metadata": {
            "timestamp": opts.created,
            "tools": {"components": [{"type": "application", "name": opts.tool_name, "version": opts.tool_version}]},
            "component": {"type": "application", "bom-ref": "root", "name": opts.name},
        },
        "components": components_json,
        "dependencies": deps_json,
    })
}

/// Renders an SBOM document as pretty-printed JSON with a trailing newline.
pub fn to_json_string(doc: &Value) -> String {
    let mut s = serde_json::to_string_pretty(doc).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_conversion() {
        let (alg, h) = integrity_to_hash("sha1-o1AIsg9BOD7sH7kU9M1d95omQoQ=").unwrap();
        assert_eq!(alg, "SHA1");
        assert_eq!(h.len(), 40);
        let hex64 = "ab".repeat(32);
        assert_eq!(
            integrity_to_hash(&format!("sha256:{hex64}")),
            Some(("SHA256", hex64))
        );
        assert_eq!(integrity_to_hash("h1:abc="), None);
        assert_eq!(integrity_to_hash("sha256:zz"), None);
        assert_eq!(integrity_to_hash("sha512-aaaa"), None);
    }

    #[test]
    fn base64_matches_known_vector() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert!(base64_decode("T*Fu").is_none());
    }
}
