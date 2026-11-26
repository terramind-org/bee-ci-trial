//! Repo file analysis behind `InsightsService`: language statistics, license detection,
//! community health files, citation files (F416-012, F416-013) and `FUNDING.yml`.
//!
//! Everything here is a pure function of a repo's file list and contents at one revision, so
//! results are deterministic and cacheable by tree id.
//!
//! - **Languages** count bytes of programming and markup files by extension; vendored,
//!   generated and documentation paths are skipped unless asked for.
//! - **License** reads root `LICENSE`/`COPYING`-style files and matches their text against
//!   fingerprints of common licenses; with no license file, `license` in `Cargo.toml` or
//!   `package.json` is used. Several license files make a multi-licensed repo.
//! - **Community files** are found at the root, then `.github`, then `docs`.
//! - **Citation** parses `CITATION.cff` and renders APA and BibTeX.
//! - **Funding** parses `FUNDING.yml` into platform links.

use std::collections::BTreeMap;

use bee_proto::v1::{
    CitationInfo, CommunityFile, CommunityFileKind, CommunityProfile, LanguageStat, LanguageType,
    LicenseInfo, ObjectId, SponsorFundingLink,
};

use crate::error::{ForgeError, Result};

use super::lang::language_of;
use super::ports::FileEntry;
use super::yaml::{self, Yaml};

/// Counts bytes per language over `files` (programming and markup; prose too with
/// `documentation`). Returns the stats largest first and the counted total.
pub fn language_stats(
    files: &[FileEntry],
    vendored: bool,
    generated: bool,
    documentation: bool,
) -> (Vec<LanguageStat>, u64) {
    let mut by_lang: BTreeMap<&'static str, (u64, u32, LanguageType, &'static str)> =
        BTreeMap::new();
    for f in files {
        let lower = f.path.to_lowercase();
        let is_vendor = [
            "vendor/",
            "node_modules/",
            "third_party/",
            "third-party/",
            "external/",
        ]
        .iter()
        .any(|d| lower.starts_with(d) || lower.contains(&format!("/{d}")));
        let is_generated = lower.ends_with(".min.js")
            || lower.ends_with(".min.css")
            || lower.ends_with(".pb.go")
            || lower.contains("generated/")
            || lower.ends_with(".lock");
        let is_doc =
            lower.starts_with("docs/") || lower.starts_with("doc/") || lower.contains("/docs/");
        if (is_vendor && !vendored) || (is_generated && !generated) || (is_doc && !documentation) {
            continue;
        }
        let Some((name, ty, color)) = language_of(&f.path) else {
            continue;
        };
        let counted = matches!(ty, LanguageType::Programming | LanguageType::Markup)
            || (documentation && ty == LanguageType::Prose);
        if !counted {
            continue;
        }
        let e = by_lang.entry(name).or_insert((0, 0, ty, color));
        e.0 += f.size;
        e.1 += 1;
    }
    let total: u64 = by_lang.values().map(|v| v.0).sum();
    let mut out: Vec<LanguageStat> = by_lang
        .into_iter()
        .map(|(name, (bytes, count, ty, color))| LanguageStat {
            name: name.to_string(),
            byte_count: bytes,
            file_count: count,
            percentage: if total == 0 {
                0.0
            } else {
                (bytes as f64) * 100.0 / (total as f64)
            },
            color: color.to_string(),
            r#type: ty as i32,
        })
        .collect();
    out.sort_by(|a, b| b.byte_count.cmp(&a.byte_count).then(a.name.cmp(&b.name)));
    (out, total)
}

struct Fingerprint {
    spdx: &'static str,
    name: &'static str,
    all: &'static [&'static str],
    permissions: &'static [&'static str],
    conditions: &'static [&'static str],
    limitations: &'static [&'static str],
}

const COMMERCIAL: &[&str] = &[
    "commercial-use",
    "modifications",
    "distribution",
    "private-use",
];

const FINGERPRINTS: &[Fingerprint] = &[
    Fingerprint {
        spdx: "MIT",
        name: "MIT License",
        all: &["permission is hereby granted, free of charge"],
        permissions: COMMERCIAL,
        conditions: &["include-copyright"],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "Apache-2.0",
        name: "Apache License 2.0",
        all: &["apache license", "version 2.0"],
        permissions: &[
            "commercial-use",
            "modifications",
            "distribution",
            "patent-use",
            "private-use",
        ],
        conditions: &["include-copyright", "document-changes"],
        limitations: &["trademark-use", "liability", "warranty"],
    },
    Fingerprint {
        spdx: "AGPL-3.0",
        name: "GNU Affero General Public License v3.0",
        all: &["gnu affero general public license", "version 3"],
        permissions: COMMERCIAL,
        conditions: &[
            "include-copyright",
            "document-changes",
            "disclose-source",
            "network-use-disclose",
            "same-license",
        ],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "LGPL-3.0",
        name: "GNU Lesser General Public License v3.0",
        all: &["gnu lesser general public license", "version 3"],
        permissions: COMMERCIAL,
        conditions: &[
            "include-copyright",
            "document-changes",
            "disclose-source",
            "same-license",
        ],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "GPL-3.0",
        name: "GNU General Public License v3.0",
        all: &["gnu general public license", "version 3"],
        permissions: COMMERCIAL,
        conditions: &[
            "include-copyright",
            "document-changes",
            "disclose-source",
            "same-license",
        ],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "GPL-2.0",
        name: "GNU General Public License v2.0",
        all: &["gnu general public license", "version 2"],
        permissions: COMMERCIAL,
        conditions: &[
            "include-copyright",
            "document-changes",
            "disclose-source",
            "same-license",
        ],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "MPL-2.0",
        name: "Mozilla Public License 2.0",
        all: &["mozilla public license", "2.0"],
        permissions: &[
            "commercial-use",
            "modifications",
            "distribution",
            "patent-use",
            "private-use",
        ],
        conditions: &["include-copyright", "disclose-source", "same-license"],
        limitations: &["trademark-use", "liability", "warranty"],
    },
    Fingerprint {
        spdx: "BSD-3-Clause",
        name: "BSD 3-Clause License",
        all: &[
            "redistribution and use in source and binary forms",
            "neither the name",
        ],
        permissions: COMMERCIAL,
        conditions: &["include-copyright"],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "BSD-2-Clause",
        name: "BSD 2-Clause License",
        all: &["redistribution and use in source and binary forms"],
        permissions: COMMERCIAL,
        conditions: &["include-copyright"],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "ISC",
        name: "ISC License",
        all: &["permission to use, copy, modify, and/or distribute"],
        permissions: COMMERCIAL,
        conditions: &["include-copyright"],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "Unlicense",
        name: "The Unlicense",
        all: &["this is free and unencumbered software"],
        permissions: COMMERCIAL,
        conditions: &[],
        limitations: &["liability", "warranty"],
    },
    Fingerprint {
        spdx: "CC0-1.0",
        name: "Creative Commons Zero v1.0 Universal",
        all: &["cc0 1.0 universal"],
        permissions: COMMERCIAL,
        conditions: &[],
        limitations: &["liability", "trademark-use", "patent-use", "warranty"],
    },
    Fingerprint {
        spdx: "EPL-2.0",
        name: "Eclipse Public License 2.0",
        all: &["eclipse public license", "v 2.0"],
        permissions: &[
            "commercial-use",
            "modifications",
            "distribution",
            "patent-use",
            "private-use",
        ],
        conditions: &["disclose-source", "include-copyright", "same-license"],
        limitations: &["liability", "warranty"],
    },
];

fn license_file_name(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    let up = path.to_uppercase();
    let stem = up.split('.').next().unwrap_or("");
    ["LICENSE", "LICENCE", "COPYING", "UNLICENSE", "COPYRIGHT"]
        .iter()
        .any(|n| {
            stem == *n || stem.starts_with(&format!("{n}-")) || stem.starts_with(&format!("{n}_"))
        })
}

/// The license files at the root of a file list, sorted by path.
pub fn license_files(files: &[FileEntry]) -> Vec<&FileEntry> {
    let mut v: Vec<&FileEntry> = files
        .iter()
        .filter(|f| license_file_name(&f.path))
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// A matched license: SPDX id, name, permissions, conditions, limitations.
pub type LicenseMatch = (
    &'static str,
    &'static str,
    Vec<String>,
    Vec<String>,
    Vec<String>,
);

/// Matches license text against the fingerprint table.
pub fn detect_license_text(text: &str) -> Option<LicenseMatch> {
    let t = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    FINGERPRINTS
        .iter()
        .find(|f| f.all.iter().all(|p| t.contains(p)))
        .map(|f| {
            (
                f.spdx,
                f.name,
                f.permissions.iter().map(|s| s.to_string()).collect(),
                f.conditions.iter().map(|s| s.to_string()).collect(),
                f.limitations.iter().map(|s| s.to_string()).collect(),
            )
        })
}

/// The license declared by a package manifest (`Cargo.toml` or `package.json`).
pub fn manifest_license(path: &str, text: &str) -> Option<String> {
    match path {
        "Cargo.toml" => text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("license") && !l.starts_with("license-file") && l.contains('='))
            .and_then(|l| {
                l.split_once('=')
                    .map(|(_, v)| v.trim().trim_matches('"').to_string())
            })
            .filter(|v| !v.is_empty()),
        "package.json" => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| v.get("license").and_then(|l| l.as_str()).map(String::from)),
        _ => None,
    }
}

/// Builds license info from the license files' contents (`(file, bytes)`), a manifest license
/// as fallback, at changeset `at`. The first file is the primary license.
pub fn license_info(
    files: &[(&FileEntry, String)],
    manifest: Option<(&str, String)>,
    at: &ObjectId,
) -> LicenseInfo {
    let mut infos: Vec<LicenseInfo> = files
        .iter()
        .map(|(f, text)| match detect_license_text(text) {
            Some((spdx, name, p, c, l)) => LicenseInfo {
                spdx_id: spdx.into(),
                name: name.into(),
                confidence: 0.9,
                path: f.path.clone().into_bytes(),
                blob: Some(f.blob.clone()),
                matcher: "fingerprint".into(),
                permissions: p,
                conditions: c,
                limitations: l,
                changeset: Some(at.clone()),
                ..Default::default()
            },
            None => LicenseInfo {
                spdx_id: "NOASSERTION".into(),
                name: "Other".into(),
                confidence: 0.0,
                path: f.path.clone().into_bytes(),
                blob: Some(f.blob.clone()),
                matcher: "none".into(),
                changeset: Some(at.clone()),
                ..Default::default()
            },
        })
        .collect();
    if infos.is_empty() {
        return match manifest {
            Some((path, spdx)) => LicenseInfo {
                spdx_id: spdx.clone(),
                name: spdx,
                confidence: 1.0,
                path: path.as_bytes().to_vec(),
                matcher: "package_manifest".into(),
                changeset: Some(at.clone()),
                ..Default::default()
            },
            None => LicenseInfo {
                spdx_id: "NOASSERTION".into(),
                name: "No license found".into(),
                matcher: "none".into(),
                changeset: Some(at.clone()),
                ..Default::default()
            },
        };
    }
    let mut primary = infos.remove(0);
    primary.additional = infos;
    primary
}

fn ci_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn kind_of(dir: &str, name: &str) -> Option<CommunityFileKind> {
    let up = name.to_uppercase();
    let stem = up.split('.').next().unwrap_or("");
    let in_issue_dir = dir.eq_ignore_ascii_case(".github/ISSUE_TEMPLATE");
    let in_cr_dir = dir.eq_ignore_ascii_case(".github/PULL_REQUEST_TEMPLATE")
        || dir.eq_ignore_ascii_case(".github/CHANGE_REQUEST_TEMPLATE");
    if in_issue_dir {
        return Some(CommunityFileKind::IssueTemplate);
    }
    if in_cr_dir {
        return Some(CommunityFileKind::ChangeRequestTemplate);
    }
    if !(dir.is_empty() || ci_eq(dir, ".github") || ci_eq(dir, "docs")) {
        return None;
    }
    Some(match stem {
        "README" => CommunityFileKind::Readme,
        "LICENSE" | "LICENCE" | "COPYING" | "UNLICENSE" => CommunityFileKind::License,
        "CONTRIBUTING" => CommunityFileKind::Contributing,
        "SECURITY" => CommunityFileKind::Security,
        "CODE_OF_CONDUCT" => CommunityFileKind::CodeOfConduct,
        "FUNDING" if up == "FUNDING.YML" || up == "FUNDING.YAML" => CommunityFileKind::Funding,
        "SUPPORT" => CommunityFileKind::Support,
        "CITATION" if up == "CITATION.CFF" => CommunityFileKind::Citation,
        "ISSUE_TEMPLATE" => CommunityFileKind::IssueTemplate,
        "PULL_REQUEST_TEMPLATE" | "CHANGE_REQUEST_TEMPLATE" => {
            CommunityFileKind::ChangeRequestTemplate
        }
        "GOVERNANCE" => CommunityFileKind::Governance,
        "CODEOWNERS" => CommunityFileKind::Codeowners,
        _ => return None,
    })
}

/// Finds the community files in a tree: root first, then `.github`, then `docs` (paths are
/// listed in that order). `code_of_conduct_text` names the detected conduct (if the caller
/// read it).
pub fn community_files(
    files: &[FileEntry],
    code_of_conduct_text: Option<&str>,
) -> Vec<CommunityFile> {
    const ALL: [CommunityFileKind; 12] = [
        CommunityFileKind::Readme,
        CommunityFileKind::License,
        CommunityFileKind::Contributing,
        CommunityFileKind::Security,
        CommunityFileKind::CodeOfConduct,
        CommunityFileKind::Funding,
        CommunityFileKind::Support,
        CommunityFileKind::Citation,
        CommunityFileKind::IssueTemplate,
        CommunityFileKind::ChangeRequestTemplate,
        CommunityFileKind::Governance,
        CommunityFileKind::Codeowners,
    ];
    let rank = |p: &str| -> u8 {
        if !p.contains('/') {
            0
        } else if p.to_lowercase().starts_with(".github/") {
            1
        } else {
            2
        }
    };
    let mut by_kind: BTreeMap<i32, Vec<&FileEntry>> = BTreeMap::new();
    for f in files {
        let (dir, name) = f.path.rsplit_once('/').unwrap_or(("", f.path.as_str()));
        if let Some(k) = kind_of(dir, name) {
            by_kind.entry(k as i32).or_default().push(f);
        }
    }
    ALL.iter()
        .map(|k| {
            let mut hits = by_kind.get(&(*k as i32)).cloned().unwrap_or_default();
            hits.sort_by(|a, b| rank(&a.path).cmp(&rank(&b.path)).then(a.path.cmp(&b.path)));
            let detail = if *k == CommunityFileKind::CodeOfConduct {
                code_of_conduct_text.and_then(|t| {
                    let l = t.to_lowercase();
                    l.contains("contributor covenant").then(|| {
                        let v = l
                            .split("version ")
                            .nth(1)
                            .map(|s| {
                                s.chars()
                                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        format!("contributor_covenant_{}", v.trim_end_matches('.'))
                    })
                })
            } else {
                None
            };
            CommunityFile {
                kind: *k as i32,
                present: !hits.is_empty(),
                paths: hits.iter().map(|f| f.path.clone().into_bytes()).collect(),
                inherited: false,
                detail: detail.unwrap_or_default(),
            }
        })
        .collect()
}

/// The community profile: health is the share of six items present (description, README,
/// code of conduct, contributing guide, license, issue or change request template).
pub fn community_profile(
    files: Vec<CommunityFile>,
    has_description: bool,
    has_discussions: bool,
    at: &ObjectId,
    now_ms: u64,
) -> CommunityProfile {
    let has = |k: CommunityFileKind| files.iter().any(|f| f.kind == k as i32 && f.present);
    let items = [
        has_description,
        has(CommunityFileKind::Readme),
        has(CommunityFileKind::CodeOfConduct),
        has(CommunityFileKind::Contributing),
        has(CommunityFileKind::License),
        has(CommunityFileKind::IssueTemplate) || has(CommunityFileKind::ChangeRequestTemplate),
    ];
    let present = items.iter().filter(|b| **b).count() as u32;
    CommunityProfile {
        repo: None,
        changeset: Some(at.clone()),
        health_percentage: (present * 100 + 3) / 6,
        files,
        has_description,
        has_discussions,
        content_reports_enabled: false,
        updated_at: Some(crate::ids::ts(now_ms)),
    }
}

fn to_json(y: &Yaml) -> serde_json::Value {
    match y {
        Yaml::Null => serde_json::Value::Null,
        Yaml::Bool(b) => serde_json::Value::Bool(*b),
        Yaml::Str(s) => serde_json::Value::String(s.clone()),
        Yaml::Seq(v) => serde_json::Value::Array(v.iter().map(to_json).collect()),
        Yaml::Map(m) => {
            serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
    }
}

/// Parses a `CITATION.cff` into title, authors, version, DOI, release date and URL, with APA
/// and BibTeX renderings.
pub fn citation(path: &str, blob: &ObjectId, text: &str) -> Result<CitationInfo> {
    let y = yaml::parse(text).map_err(|e| ForgeError::invalid("invalid_citation", e))?;
    let s = |k: &str| y.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let title = s("title");
    if title.is_empty() {
        return Err(ForgeError::invalid(
            "invalid_citation",
            "CITATION.cff needs a title",
        ));
    }
    struct Author {
        family: String,
        given: String,
        full: String,
    }
    let authors: Vec<Author> = y
        .get("authors")
        .map(|a| a.items())
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            let family = a
                .get("family-names")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let given = a
                .get("given-names")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or_default();
            if family.is_empty() && given.is_empty() {
                Author {
                    family: name.clone(),
                    given: String::new(),
                    full: name,
                }
            } else {
                Author {
                    full: format!("{given} {family}").trim().to_string(),
                    family,
                    given,
                }
            }
        })
        .collect();
    let date = s("date-released");
    let year = date.split('-').next().unwrap_or("").to_string();
    let url = [s("url"), s("repository-code")]
        .into_iter()
        .find(|u| !u.is_empty())
        .unwrap_or_default();
    let apa_authors: Vec<String> = authors
        .iter()
        .map(|a| {
            if a.given.is_empty() {
                a.family.clone()
            } else {
                let initials: String = a
                    .given
                    .split_whitespace()
                    .filter_map(|g| g.chars().next())
                    .map(|c| format!("{c}."))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{}, {initials}", a.family)
            }
        })
        .collect();
    let apa_names = match apa_authors.len() {
        0 => String::new(),
        1 => apa_authors[0].clone(),
        n => format!(
            "{}, & {}",
            apa_authors[..n - 1].join(", "),
            apa_authors[n - 1]
        ),
    };
    let version = s("version");
    let mut apa = String::new();
    if !apa_names.is_empty() {
        apa.push_str(&apa_names);
        apa.push(' ');
    }
    apa.push_str(&format!(
        "({}). {title}",
        if year.is_empty() {
            "n.d.".to_string()
        } else {
            year.clone()
        }
    ));
    if !version.is_empty() {
        apa.push_str(&format!(" (Version {version})"));
    }
    apa.push_str(" [Computer software].");
    let doi = s("doi");
    if !doi.is_empty() {
        apa.push_str(&format!(" https://doi.org/{doi}"));
    } else if !url.is_empty() {
        apa.push_str(&format!(" {url}"));
    }
    let key: String = format!(
        "{}{}{}",
        authors
            .first()
            .map(|a| a.family.to_lowercase())
            .unwrap_or_default(),
        year,
        title.split_whitespace().next().unwrap_or("").to_lowercase()
    )
    .chars()
    .filter(|c| c.is_ascii_alphanumeric())
    .collect();
    let mut bib = format!(
        "@software{{{},\n",
        if key.is_empty() {
            "cite".to_string()
        } else {
            key
        }
    );
    if !authors.is_empty() {
        let names: Vec<String> = authors
            .iter()
            .map(|a| {
                if a.given.is_empty() {
                    a.family.clone()
                } else {
                    format!("{}, {}", a.family, a.given)
                }
            })
            .collect();
        bib.push_str(&format!("  author = {{{}}},\n", names.join(" and ")));
    }
    bib.push_str(&format!("  title = {{{title}}},\n"));
    if !year.is_empty() {
        bib.push_str(&format!("  year = {{{year}}},\n"));
    }
    if !version.is_empty() {
        bib.push_str(&format!("  version = {{{version}}},\n"));
    }
    if !doi.is_empty() {
        bib.push_str(&format!("  doi = {{{doi}}},\n"));
    }
    if !url.is_empty() {
        bib.push_str(&format!("  url = {{{url}}},\n"));
    }
    bib.push('}');
    Ok(CitationInfo {
        path: path.as_bytes().to_vec(),
        blob: Some(blob.clone()),
        title: title.clone(),
        authors: authors.iter().map(|a| a.full.clone()).collect(),
        version,
        doi,
        date_released: date,
        url,
        cff_version: s("cff-version"),
        apa,
        bibtex: bib,
        json: to_json(&y).to_string(),
    })
}

const FUNDING_PLATFORMS: &[(&str, &str)] = &[
    ("github", "https://github.com/sponsors/{}"),
    ("beehive", "https://beehive.invalid/sponsors/{}"),
    ("open_collective", "https://opencollective.com/{}"),
    ("ko_fi", "https://ko-fi.com/{}"),
    ("liberapay", "https://liberapay.com/{}"),
    ("patreon", "https://patreon.com/{}"),
    ("buy_me_a_coffee", "https://www.buymeacoffee.com/{}"),
    ("polar", "https://polar.sh/{}"),
    ("thanks_dev", "https://thanks.dev/{}"),
    ("tidelift", "https://tidelift.com/funding/github/{}"),
    (
        "community_bridge",
        "https://funding.communitybridge.org/projects/{}",
    ),
    ("issuehunt", "https://issuehunt.io/r/{}"),
    (
        "lfx_crowdfunding",
        "https://crowdfunding.lfx.linuxfoundation.org/projects/{}",
    ),
    ("otechie", "https://otechie.com/{}"),
];

/// Most entries `FUNDING.yml` may list (GitHub: four custom URLs, one account per platform
/// except `github`).
pub const MAX_FUNDING_ENTRIES: usize = 4;

/// Parses a `FUNDING.yml` into links and warnings (unknown platforms, too many entries).
pub fn parse_funding(text: &str) -> Result<(Vec<SponsorFundingLink>, Vec<String>)> {
    parse_funding_with_base(text, "https://beehive.invalid")
}

/// Parses funding links using `base_url` for Beehive's built-in sponsor link.
pub fn parse_funding_with_base(
    text: &str,
    base_url: &str,
) -> Result<(Vec<SponsorFundingLink>, Vec<String>)> {
    let y = yaml::parse(text).map_err(|e| ForgeError::invalid("invalid_funding", e))?;
    let Yaml::Map(entries) = &y else {
        if matches!(y, Yaml::Null) {
            return Ok((Vec::new(), Vec::new()));
        }
        return Err(ForgeError::invalid(
            "invalid_funding",
            "FUNDING.yml is a mapping of platform to account",
        ));
    };
    let mut links = Vec::new();
    let mut warnings = Vec::new();
    for (platform, v) in entries {
        let accounts = v.strings();
        if accounts.is_empty() {
            continue;
        }
        if platform == "custom" {
            let mut urls = Vec::new();
            for (i, a) in accounts.iter().enumerate() {
                if i >= MAX_FUNDING_ENTRIES {
                    warnings.push(format!(
                        "custom lists more than {MAX_FUNDING_ENTRIES} URLs; the rest are ignored"
                    ));
                    break;
                }
                let u = if a.contains("://") {
                    a.clone()
                } else {
                    format!("https://{a}")
                };
                if u.starts_with("https://") || u.starts_with("http://") {
                    urls.push(u);
                } else {
                    warnings.push(format!("custom entry {a:?} is not a URL"));
                }
            }
            links.push(SponsorFundingLink {
                platform: "custom".into(),
                accounts,
                urls,
            });
            continue;
        }
        match FUNDING_PLATFORMS.iter().find(|(p, _)| p == platform) {
            Some((_, pattern)) => {
                let mut kept = accounts.clone();
                if platform != "github" && kept.len() > 1 {
                    warnings.push(format!(
                        "{platform} takes one account; only {:?} is used",
                        kept[0]
                    ));
                    kept.truncate(1);
                } else if kept.len() > MAX_FUNDING_ENTRIES {
                    warnings.push(format!("{platform} lists more than {MAX_FUNDING_ENTRIES} accounts; the rest are ignored"));
                    kept.truncate(MAX_FUNDING_ENTRIES);
                }
                let template = if platform == "beehive" {
                    format!("{}/sponsors/{{}}", base_url.trim_end_matches('/'))
                } else {
                    (*pattern).to_string()
                };
                let urls = kept.iter().map(|a| template.replace("{}", a)).collect();
                links.push(SponsorFundingLink {
                    platform: platform.clone(),
                    accounts: kept,
                    urls,
                });
            }
            None => warnings.push(format!("unknown funding platform {platform:?}")),
        }
    }
    Ok((links, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.into(),
            blob: ObjectId {
                raw: vec![size as u8],
            },
            size,
        }
    }

    #[test]
    fn language_percentages_skip_vendored_and_docs() {
        let files = [
            f("src/a.rs", 300),
            f("src/b.py", 100),
            f("vendor/x.js", 5000),
            f("docs/guide.md", 900),
            f("data.json", 50),
        ];
        let (stats, total) = language_stats(&files, false, false, false);
        assert_eq!(total, 400);
        assert_eq!(stats[0].name, "Rust");
        assert!((stats[0].percentage - 75.0).abs() < 1e-9);
        let (with_vendor, total) = language_stats(&files, true, false, false);
        assert_eq!(total, 5400);
        assert_eq!(with_vendor[0].name, "JavaScript");
    }

    #[test]
    fn licenses_by_fingerprint_and_manifest() {
        let mit = "MIT License\n\nPermission is hereby granted, free of charge, to any person";
        assert_eq!(detect_license_text(mit).unwrap().0, "MIT");
        let apache = "Apache License\n   Version 2.0, January 2004";
        assert_eq!(detect_license_text(apache).unwrap().0, "Apache-2.0");
        assert!(detect_license_text("do what you like").is_none());
        assert_eq!(
            manifest_license("Cargo.toml", "[package]\nlicense = \"MIT OR Apache-2.0\"\n")
                .as_deref(),
            Some("MIT OR Apache-2.0")
        );
        let file_a = f("LICENSE-APACHE", 1);
        let file_m = f("LICENSE-MIT", 2);
        assert_eq!(
            license_files(&[file_m.clone(), file_a.clone(), f("src/LICENSE", 3)]).len(),
            2
        );
        let info = license_info(
            &[(&file_a, apache.into()), (&file_m, mit.into())],
            None,
            &ObjectId::default(),
        );
        assert_eq!(info.spdx_id, "Apache-2.0");
        assert_eq!(info.additional[0].spdx_id, "MIT");
    }

    #[test]
    fn community_files_prefer_root_then_github_then_docs() {
        let files = [
            f("docs/CONTRIBUTING.md", 1),
            f(".github/CONTRIBUTING.md", 1),
            f("README.md", 1),
            f(".github/ISSUE_TEMPLATE/bug.yml", 1),
            f(".github/FUNDING.yml", 1),
        ];
        let found = community_files(&files, None);
        let contributing = found
            .iter()
            .find(|c| c.kind == CommunityFileKind::Contributing as i32)
            .unwrap();
        assert_eq!(
            contributing.paths,
            vec![
                b".github/CONTRIBUTING.md".to_vec(),
                b"docs/CONTRIBUTING.md".to_vec()
            ]
        );
        assert!(
            found
                .iter()
                .any(|c| c.kind == CommunityFileKind::Funding as i32 && c.present)
        );
        assert!(
            found
                .iter()
                .any(|c| c.kind == CommunityFileKind::IssueTemplate as i32 && c.present)
        );
        let profile = community_profile(found, true, false, &ObjectId::default(), 0);
        assert_eq!(profile.health_percentage, 67); // description, README, contributing, template = 4 of 6
    }

    #[test]
    fn citation_renders_apa_and_bibtex() {
        let cff = "cff-version: 1.2.0\ntitle: Bee\nversion: 1.0.0\ndate-released: 2026-10-01\ndoi: 10.1234/abc\nauthors:\n  - family-names: Doe\n    given-names: Jane Q\n  - family-names: Roe\n    given-names: Rick\n";
        let c = citation("CITATION.cff", &ObjectId::default(), cff).unwrap();
        assert_eq!(
            c.apa,
            "Doe, J. Q., & Roe, R. (2026). Bee (Version 1.0.0) [Computer software]. https://doi.org/10.1234/abc"
        );
        assert!(c.bibtex.starts_with("@software{doe2026bee,"));
        assert!(c.bibtex.contains("author = {Doe, Jane Q and Roe, Rick}"));
        assert_eq!(c.authors, vec!["Jane Q Doe", "Rick Roe"]);
        assert!(citation("CITATION.cff", &ObjectId::default(), "cff-version: 1.2.0\n").is_err());
    }

    #[test]
    fn funding_links_and_warnings() {
        let (links, warnings) = parse_funding("github: [octocat, hubot]\nko_fi: me\nunknown_site: x\ncustom: [example.com/donate]\nliberapay: [a, b]\n").unwrap();
        assert_eq!(links.len(), 4);
        assert_eq!(
            links[0].urls,
            vec![
                "https://github.com/sponsors/octocat",
                "https://github.com/sponsors/hubot"
            ]
        );
        assert_eq!(links[2].urls, vec!["https://example.com/donate"]);
        assert_eq!(warnings.len(), 2);
    }
}
