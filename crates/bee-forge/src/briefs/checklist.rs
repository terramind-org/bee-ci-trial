//! The brief checklist (SPEC §11.9): what a change request's brief and evidence still owe for
//! one revision. [`compute`] is a pure function of the revision's per-path diff classes
//! (SPEC §11.3), who authored the CR, the brief and the evidence; it reads no clock and keeps
//! no order but the inputs' sorted order. Advisory only: landing never consults it.
//!
//! | item | raised when | satisfied by |
//! |---|---|---|
//! | `brief` | the CR is agent-authored (its author is an agent or a service) and a path is `logic` or `sensitive` | an outcome and at least one highlight |
//! | `brief:risk` | a path is `sensitive` | at least one risk, or a rollback plan |
//! | `evidence:ui` | a UI file ([`UI_EXTENSIONS`]) is `logic` or `sensitive` | ready evidence of this revision that is a screenshot, before and after, or a video |

use bee_proto::v1::{
    BriefChecklist, BriefChecklistItem, ChangeBrief, Evidence, EvidenceKind, EvidenceState,
    PrincipalKind,
};

/// Extensions of UI files (lower case, without the dot).
pub const UI_EXTENSIONS: &[&str] = &[
    "tsx", "jsx", "vue", "svelte", "astro", "html", "css", "scss", "less",
];
/// Most paths one item lists.
pub const MAX_ITEM_PATHS: usize = 20;
/// Evidence kinds that show a UI change.
pub const UI_KINDS: &[EvidenceKind] = &[
    EvidenceKind::Screenshot,
    EvidenceKind::BeforeAfter,
    EvidenceKind::Video,
];

/// One changed path and its diff class (wire name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathIn<'a> {
    /// Repository path.
    pub path: &'a str,
    /// Diff class, e.g. `logic`.
    pub class: &'a str,
}

/// What the checklist is computed from.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    /// The revision the checklist is for.
    pub revision: u32,
    /// Kind of the CR's author.
    pub author_kind: PrincipalKind,
    /// The revision's changed paths with their classes.
    pub paths: &'a [PathIn<'a>],
    /// The CR's brief, if written.
    pub brief: Option<&'a ChangeBrief>,
    /// The CR's evidence (any revision, any state; only ready items of `revision` count).
    pub evidence: &'a [Evidence],
}

/// Whether `path` is a UI file.
pub fn is_ui(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, ext)| UI_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn risky(class: &str) -> bool {
    matches!(class, "logic" | "sensitive")
}

/// Paths in path order, unique, at most [`MAX_ITEM_PATHS`].
fn listed<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<&str> = paths.collect();
    v.sort_unstable();
    v.dedup();
    v.into_iter()
        .take(MAX_ITEM_PATHS)
        .map(str::to_string)
        .collect()
}

/// The checklist of `inputs`.
pub fn compute(inputs: &Inputs<'_>) -> BriefChecklist {
    let mut items = Vec::new();
    let agent = matches!(
        inputs.author_kind,
        PrincipalKind::Agent | PrincipalKind::Service
    );
    let brief = inputs.brief;
    let logic: Vec<&str> = inputs
        .paths
        .iter()
        .filter(|p| risky(p.class))
        .map(|p| p.path)
        .collect();
    if agent && !logic.is_empty() {
        let satisfied =
            brief.is_some_and(|b| !b.outcome.trim().is_empty() && !b.highlights.is_empty());
        items.push(BriefChecklistItem {
            id: "brief".into(),
            detail: "an outcome and at least one highlight in the brief".into(),
            kinds: Vec::new(),
            paths: listed(logic.iter().copied()),
            satisfied,
            evidence_ids: Vec::new(),
        });
    }
    let sensitive: Vec<&str> = inputs
        .paths
        .iter()
        .filter(|p| p.class == "sensitive")
        .map(|p| p.path)
        .collect();
    if !sensitive.is_empty() {
        let satisfied = brief.is_some_and(|b| !b.risks.is_empty() || !b.rollback.trim().is_empty());
        items.push(BriefChecklistItem {
            id: "brief:risk".into(),
            detail: "a risk or a rollback plan for the sensitive change".into(),
            kinds: Vec::new(),
            paths: listed(sensitive.into_iter()),
            satisfied,
            evidence_ids: Vec::new(),
        });
    }
    let ui: Vec<&str> = inputs
        .paths
        .iter()
        .filter(|p| risky(p.class) && is_ui(p.path))
        .map(|p| p.path)
        .collect();
    if !ui.is_empty() {
        let mut ids: Vec<String> = inputs
            .evidence
            .iter()
            .filter(|e| {
                e.revision == inputs.revision
                    && e.state == EvidenceState::Ready as i32
                    && e.spec
                        .as_ref()
                        .and_then(|s| EvidenceKind::try_from(s.kind).ok())
                        .is_some_and(|k| UI_KINDS.contains(&k))
            })
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        items.push(BriefChecklistItem {
            id: "evidence:ui".into(),
            detail: "a screenshot, before and after, or video of the UI change".into(),
            kinds: UI_KINDS.iter().map(|k| *k as i32).collect(),
            paths: listed(ui.into_iter()),
            satisfied: !ids.is_empty(),
            evidence_ids: ids,
        });
    }
    BriefChecklist {
        revision: inputs.revision,
        complete: items.iter().all(|i| i.satisfied),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::v1::{BriefHighlight, BriefRisk, EvidenceSpec};

    fn ev(id: &str, revision: u32, kind: EvidenceKind, state: EvidenceState) -> Evidence {
        Evidence {
            id: id.into(),
            revision,
            state: state as i32,
            spec: Some(EvidenceSpec {
                kind: kind as i32,
                title: id.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn item<'a>(c: &'a BriefChecklist, id: &str) -> Option<&'a BriefChecklistItem> {
        c.items.iter().find(|i| i.id == id)
    }

    #[test]
    fn checklist_asks_ui_changes_for_visual_evidence() {
        let paths = [
            PathIn {
                path: "web/checkout.tsx",
                class: "logic",
            },
            PathIn {
                path: "web/app.css",
                class: "logic",
            },
            PathIn {
                path: "docs/page.html",
                class: "docs",
            },
            PathIn {
                path: "src/lib.rs",
                class: "logic",
            },
        ];
        let none = compute(&Inputs {
            revision: 2,
            author_kind: PrincipalKind::Human,
            paths: &paths,
            brief: None,
            evidence: &[],
        });
        let ui = item(&none, "evidence:ui").expect("a UI change owes visual evidence");
        assert!(!ui.satisfied && !none.complete);
        // Only UI files at logic or above, in path order; docs-class HTML does not count.
        assert_eq!(ui.paths, vec!["web/app.css", "web/checkout.tsx"]);
        assert_eq!(
            ui.detail,
            "a screenshot, before and after, or video of the UI change"
        );
        assert_eq!(
            ui.kinds,
            vec![
                EvidenceKind::Screenshot as i32,
                EvidenceKind::BeforeAfter as i32,
                EvidenceKind::Video as i32
            ]
        );
        // A human's CR owes no brief.
        assert!(item(&none, "brief").is_none());
        // A benchmark, a pending screenshot and one of an older revision do not satisfy it.
        let evidence = [
            ev("b", 2, EvidenceKind::Benchmark, EvidenceState::Ready),
            ev("p", 2, EvidenceKind::Screenshot, EvidenceState::Pending),
            ev("o", 1, EvidenceKind::Screenshot, EvidenceState::Ready),
        ];
        let still = compute(&Inputs {
            revision: 2,
            author_kind: PrincipalKind::Human,
            paths: &paths,
            brief: None,
            evidence: &evidence,
        });
        assert!(!item(&still, "evidence:ui").unwrap().satisfied);
        let evidence = [
            ev("v", 2, EvidenceKind::Video, EvidenceState::Ready),
            ev("s", 2, EvidenceKind::BeforeAfter, EvidenceState::Ready),
        ];
        let done = compute(&Inputs {
            revision: 2,
            author_kind: PrincipalKind::Human,
            paths: &paths,
            brief: None,
            evidence: &evidence,
        });
        let ui = item(&done, "evidence:ui").unwrap();
        assert!(ui.satisfied && done.complete);
        assert_eq!(ui.evidence_ids, vec!["s", "v"]);
        // A docs-only UI touch owes nothing.
        let docs = [PathIn {
            path: "site/index.html",
            class: "docs",
        }];
        let c = compute(&Inputs {
            revision: 1,
            author_kind: PrincipalKind::Agent,
            paths: &docs,
            brief: None,
            evidence: &[],
        });
        assert!(c.items.is_empty() && c.complete);
    }

    #[test]
    fn agents_owe_a_brief_and_sensitive_changes_a_risk() {
        let paths = [
            PathIn {
                path: "src/billing.rs",
                class: "sensitive",
            },
            PathIn {
                path: "src/api.rs",
                class: "logic",
            },
            PathIn {
                path: "README.md",
                class: "docs",
            },
        ];
        for kind in [PrincipalKind::Agent, PrincipalKind::Service] {
            let c = compute(&Inputs {
                revision: 1,
                author_kind: kind,
                paths: &paths,
                brief: None,
                evidence: &[],
            });
            let ids: Vec<&str> = c.items.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["brief", "brief:risk"]);
            assert_eq!(c.items[0].paths, vec!["src/api.rs", "src/billing.rs"]);
            assert_eq!(c.items[1].paths, vec!["src/billing.rs"]);
        }
        let mut brief = ChangeBrief {
            outcome: "Invoices round to cents".into(),
            ..Default::default()
        };
        let c = compute(&Inputs {
            revision: 1,
            author_kind: PrincipalKind::Agent,
            paths: &paths,
            brief: Some(&brief),
            evidence: &[],
        });
        assert!(!item(&c, "brief").unwrap().satisfied, "a highlight is owed");
        brief.highlights.push(BriefHighlight {
            text: "Rounding moves to one place".into(),
            paths: vec![],
        });
        brief.rollback = "Revert; no data changes".into();
        let c = compute(&Inputs {
            revision: 1,
            author_kind: PrincipalKind::Agent,
            paths: &paths,
            brief: Some(&brief),
            evidence: &[],
        });
        assert!(c.complete);
        brief.rollback.clear();
        brief.risks.push(BriefRisk {
            text: "Old invoices".into(),
            mitigation: String::new(),
        });
        let c = compute(&Inputs {
            revision: 1,
            author_kind: PrincipalKind::Agent,
            paths: &paths,
            brief: Some(&brief),
            evidence: &[],
        });
        assert!(item(&c, "brief:risk").unwrap().satisfied);
    }

    #[test]
    fn item_paths_are_bounded_and_ui_detection_reads_the_extension() {
        let names: Vec<String> = (0..30).map(|i| format!("ui/c{i:02}.TSX")).collect();
        let paths: Vec<PathIn> = names
            .iter()
            .map(|p| PathIn {
                path: p,
                class: "logic",
            })
            .collect();
        let c = compute(&Inputs {
            revision: 1,
            author_kind: PrincipalKind::Human,
            paths: &paths,
            brief: None,
            evidence: &[],
        });
        let ui = item(&c, "evidence:ui").unwrap();
        assert_eq!(ui.paths.len(), MAX_ITEM_PATHS);
        assert_eq!(ui.paths[0], "ui/c00.TSX");
        assert!(is_ui("a/b/Button.svelte") && is_ui("x.LESS"));
        assert!(!is_ui("styles.css.map") && !is_ui("css") && !is_ui("src/tsx/main.rs"));
    }
}
