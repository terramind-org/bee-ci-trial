//! Pure checks of evidence (SPEC §11.9): the spec of each kind, the assets each kind allows by
//! role, the media kinds each role takes, alt text and the size limits. Every function here
//! is a pure function of its arguments.

use bee_proto::v1::{EvidenceAssetRole as Role, EvidenceKind, EvidenceSpec};

use crate::changes::brief::https_url;
use crate::error::{ForgeError, Result};
use crate::media::{
    EVIDENCE_AUDIO, EVIDENCE_DOCUMENT, EVIDENCE_IMAGE, EVIDENCE_VIDEO, Kind, Limit,
};

/// Title bounds, in characters.
pub const TITLE_CHARS: (usize, usize) = (1, 120);
/// Most characters of a caption.
pub const MAX_CAPTION_CHARS: usize = 1000;
/// Alt text bounds of an image, in characters.
pub const ALT_CHARS: (usize, usize) = (1, 1000);
/// Benchmark series of a benchmark: at least, at most.
pub const SERIES: (usize, usize) = (1, 20);
/// Most characters of a series name.
pub const MAX_SERIES_NAME_CHARS: usize = 120;
/// Most characters of a series unit.
pub const MAX_UNIT_CHARS: usize = 20;
/// Most failures of a test report.
pub const MAX_FAILURES: usize = 50;
/// Most characters of a test report's summary and of a failure's name.
pub const MAX_REPORT_TEXT_CHARS: usize = 280;
/// Most characters of a failure's message.
pub const MAX_FAILURE_MESSAGE_CHARS: usize = 2000;
/// Most paths an item names.
pub const MAX_PATHS: usize = 20;

/// What a role's file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// An image (png, jpeg, gif, webp); needs alt text.
    Image,
    /// A video (mp4, webm).
    Video,
    /// Audio (mpeg, ogg, wav).
    Audio,
    /// A report: PDF, plain text, Markdown or CSV.
    Document,
    /// WebVTT captions.
    Captions,
}

impl Class {
    /// The media kinds the class accepts.
    pub fn kinds(self) -> &'static [Kind] {
        match self {
            Class::Image => EVIDENCE_IMAGE.kinds,
            Class::Video => EVIDENCE_VIDEO.kinds,
            Class::Audio => EVIDENCE_AUDIO.kinds,
            Class::Document => &[Kind::Pdf, Kind::Text, Kind::Markdown, Kind::Csv],
            Class::Captions => &[Kind::Vtt],
        }
    }

    /// The size limit of the class.
    pub fn limit(self) -> Limit {
        match self {
            Class::Image => EVIDENCE_IMAGE,
            Class::Video => EVIDENCE_VIDEO,
            Class::Audio => EVIDENCE_AUDIO,
            Class::Document | Class::Captions => EVIDENCE_DOCUMENT,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Class::Image => "an image",
            Class::Video => "a video",
            Class::Audio => "an audio file",
            Class::Document => "a PDF, plain text, Markdown or CSV file",
            Class::Captions => "WebVTT captions",
        }
    }
}

/// One role a kind allows: how many assets may play it and what they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The role.
    pub role: Role,
    /// Fewest assets in the role.
    pub min: usize,
    /// Most assets in the role.
    pub max: usize,
    /// What they are.
    pub class: Class,
}

const fn slot(role: Role, min: usize, max: usize, class: Class) -> Slot {
    Slot {
        role,
        min,
        max,
        class,
    }
}

const SCREENSHOT: &[Slot] = &[slot(Role::Main, 1, 1, Class::Image)];
const BEFORE_AFTER: &[Slot] = &[
    slot(Role::Before, 1, 1, Class::Image),
    slot(Role::After, 1, 1, Class::Image),
];
const VIDEO: &[Slot] = &[
    slot(Role::Main, 1, 1, Class::Video),
    slot(Role::Poster, 0, 1, Class::Image),
    slot(Role::Captions, 0, 1, Class::Captions),
];
const IMAGE: &[Slot] = &[slot(Role::Main, 1, 12, Class::Image)];
const AUDIO: &[Slot] = &[
    slot(Role::Main, 1, 1, Class::Audio),
    slot(Role::Captions, 0, 1, Class::Captions),
];
const DOCUMENT: &[Slot] = &[slot(Role::Main, 1, 1, Class::Document)];
const TEST_REPORT: &[Slot] = &[slot(Role::Main, 0, 1, Class::Document)];

/// The roles `kind` allows, as data. A role not listed is refused.
pub fn slots(kind: EvidenceKind) -> &'static [Slot] {
    match kind {
        EvidenceKind::Screenshot => SCREENSHOT,
        EvidenceKind::BeforeAfter => BEFORE_AFTER,
        EvidenceKind::Video => VIDEO,
        EvidenceKind::Image => IMAGE,
        EvidenceKind::Audio => AUDIO,
        EvidenceKind::Document => DOCUMENT,
        EvidenceKind::TestReport => TEST_REPORT,
        EvidenceKind::Benchmark | EvidenceKind::PreviewLink | EvidenceKind::Unspecified => &[],
    }
}

/// One asset as a request describes it, before or after its bytes are known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetIn {
    /// Its role.
    pub role: Role,
    /// The declared content type.
    pub content_type: String,
    /// Its size in bytes (declared, or of the bytes sent).
    pub size: u64,
    /// Alt text.
    pub alt: String,
}

fn bad(message: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_evidence", message)
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn bounded(field: &str, s: &str, max: usize) -> Result<()> {
    let n = chars(s);
    if n > max {
        return Err(bad(format!(
            "{field} has {n} characters, at most {max} are allowed"
        )));
    }
    Ok(())
}

/// The kind of a spec, refusing an unspecified or unknown one.
pub fn kind_of(spec: &EvidenceSpec) -> Result<EvidenceKind> {
    match EvidenceKind::try_from(spec.kind) {
        Ok(EvidenceKind::Unspecified) | Err(_) => Err(bad("spec.kind is required")),
        Ok(k) => Ok(k),
    }
}

/// Checks a spec: a kind; a title of [`TITLE_CHARS`]; a caption of at most
/// [`MAX_CAPTION_CHARS`]; benchmark series only (and 1 to 20) for `BENCHMARK`, a test report
/// only (and always) for `TEST_REPORT`, a URL only (and always, `https`, else `bad_url`) for
/// `PREVIEW_LINK`; at most [`MAX_PATHS`] non-empty paths. `bad_evidence` otherwise.
pub fn check_spec(spec: &EvidenceSpec) -> Result<EvidenceKind> {
    let kind = kind_of(spec)?;
    let t = chars(spec.title.trim());
    if t < TITLE_CHARS.0 || chars(&spec.title) > TITLE_CHARS.1 {
        return Err(bad(format!(
            "spec.title must have {} to {} characters",
            TITLE_CHARS.0, TITLE_CHARS.1
        )));
    }
    bounded("spec.caption", &spec.caption, MAX_CAPTION_CHARS)?;
    if spec.paths.len() > MAX_PATHS {
        return Err(bad(format!(
            "{} paths, at most {MAX_PATHS} are allowed",
            spec.paths.len()
        )));
    }
    if spec.paths.iter().any(|p| p.is_empty()) {
        return Err(bad("spec.paths names an empty path"));
    }
    let benchmark = kind == EvidenceKind::Benchmark;
    if benchmark {
        let n = spec.benchmark.len();
        if n < SERIES.0 || n > SERIES.1 {
            return Err(bad(format!(
                "a benchmark has {} to {} series, not {n}",
                SERIES.0, SERIES.1
            )));
        }
        for (i, s) in spec.benchmark.iter().enumerate() {
            if s.name.trim().is_empty() {
                return Err(bad(format!("spec.benchmark[{i}].name is empty")));
            }
            bounded(
                &format!("spec.benchmark[{i}].name"),
                &s.name,
                MAX_SERIES_NAME_CHARS,
            )?;
            bounded(
                &format!("spec.benchmark[{i}].unit"),
                &s.unit,
                MAX_UNIT_CHARS,
            )?;
            if !s.baseline.is_finite() || !s.value.is_finite() {
                return Err(bad(format!(
                    "spec.benchmark[{i}] needs finite baseline and value"
                )));
            }
        }
    } else if !spec.benchmark.is_empty() {
        return Err(bad("benchmark series belong to BENCHMARK evidence"));
    }
    match (&spec.test_report, kind == EvidenceKind::TestReport) {
        (None, true) => return Err(bad("TEST_REPORT evidence needs spec.test_report")),
        (Some(_), false) => return Err(bad("a test report belongs to TEST_REPORT evidence")),
        (Some(r), true) => {
            if r.failures.len() > MAX_FAILURES {
                return Err(bad(format!(
                    "{} failures, at most {MAX_FAILURES} are allowed",
                    r.failures.len()
                )));
            }
            bounded(
                "spec.test_report.summary",
                &r.summary,
                MAX_REPORT_TEXT_CHARS,
            )?;
            for (i, f) in r.failures.iter().enumerate() {
                bounded(
                    &format!("spec.test_report.failures[{i}].name"),
                    &f.name,
                    MAX_REPORT_TEXT_CHARS,
                )?;
                bounded(
                    &format!("spec.test_report.failures[{i}].message"),
                    &f.message,
                    MAX_FAILURE_MESSAGE_CHARS,
                )?;
            }
        }
        (None, false) => {}
    }
    if kind == EvidenceKind::PreviewLink {
        if !https_url(&spec.url) {
            return Err(ForgeError::invalid(
                "bad_url",
                format!("spec.url {:?} is not an https URL", spec.url),
            ));
        }
    } else if !spec.url.is_empty() {
        return Err(bad("spec.url belongs to PREVIEW_LINK evidence"));
    }
    Ok(kind)
}

/// The class a role of `kind` takes, or `bad_evidence` when the kind does not allow the role.
pub fn class_of(kind: EvidenceKind, role: Role) -> Result<Class> {
    slots(kind)
        .iter()
        .find(|s| s.role == role)
        .map(|s| s.class)
        .ok_or_else(|| {
            bad(format!(
                "{} evidence takes no {} asset",
                kind.as_str_name(),
                role.as_str_name()
            ))
        })
}

/// Checks a kind's assets: each role allowed and in its count, each declared content type one
/// its class accepts (`bad_media`: SVG, HTML and script never are), alt text on every image
/// (`missing_alt`), and each size within its class limit (`TOO_LARGE`). Returns the media kind
/// of each asset, in order.
pub fn check_assets(kind: EvidenceKind, assets: &[AssetIn]) -> Result<Vec<Kind>> {
    for s in slots(kind) {
        let n = assets.iter().filter(|a| a.role == s.role).count();
        if n < s.min || n > s.max {
            let want = if s.min == s.max {
                format!("{}", s.min)
            } else {
                format!("{} to {}", s.min, s.max)
            };
            return Err(bad(format!(
                "{} evidence takes {want} {} asset(s), not {n}",
                kind.as_str_name(),
                s.role.as_str_name()
            )));
        }
    }
    let mut kinds = Vec::with_capacity(assets.len());
    for (i, a) in assets.iter().enumerate() {
        let class = class_of(kind, a.role)?;
        let media = Kind::from_content_type(&a.content_type)
            .filter(|k| class.kinds().contains(k))
            .ok_or_else(|| {
                ForgeError::invalid(
                    "bad_media",
                    format!(
                        "assets[{i}] ({}) must be {}, not {:?}",
                        a.role.as_str_name(),
                        class.noun(),
                        a.content_type
                    ),
                )
            })?;
        if class == Class::Image {
            let n = chars(a.alt.trim());
            if n < ALT_CHARS.0 {
                return Err(ForgeError::invalid(
                    "missing_alt",
                    format!("assets[{i}] is an image and needs alt text"),
                ));
            }
        }
        if chars(&a.alt) > ALT_CHARS.1 {
            return Err(bad(format!(
                "assets[{i}].alt has more than {} characters",
                ALT_CHARS.1
            )));
        }
        let limit = class.limit();
        if a.size > limit.max_bytes {
            return Err(ForgeError::too_large(
                &format!("{}_bytes", limit.name),
                limit.max_bytes,
                a.size,
            ));
        }
        if a.size == 0 {
            return Err(ForgeError::invalid(
                "bad_media",
                format!("assets[{i}] is empty"),
            ));
        }
        kinds.push(media);
    }
    Ok(kinds)
}

/// The text kind a file name suggests (`.txt`/`.log` plain text, `.md` Markdown, `.csv` CSV,
/// `.vtt` WebVTT): how files copied from a workflow artifact, which carry no content type of
/// their own, are declared. Binary kinds are found by their signature instead.
pub fn text_kind_of_path(path: &str) -> Option<Kind> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" => Some(Kind::Text),
        "md" | "markdown" => Some(Kind::Markdown),
        "csv" => Some(Kind::Csv),
        "vtt" => Some(Kind::Vtt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::v1::{BenchmarkSeries, ErrorReason, TestReport};

    fn spec(kind: EvidenceKind) -> EvidenceSpec {
        EvidenceSpec {
            kind: kind as i32,
            title: "Checkout".into(),
            ..Default::default()
        }
    }

    fn asset(role: Role, ct: &str, alt: &str) -> AssetIn {
        AssetIn {
            role,
            content_type: ct.into(),
            size: 10,
            alt: alt.into(),
        }
    }

    fn sub(r: Result<impl std::fmt::Debug>) -> String {
        r.unwrap_err().subcode
    }

    #[test]
    fn specs_carry_what_their_kind_needs() {
        assert_eq!(
            check_spec(&spec(EvidenceKind::Screenshot)).unwrap(),
            EvidenceKind::Screenshot
        );
        assert_eq!(sub(check_spec(&EvidenceSpec::default())), "bad_evidence");
        let mut s = spec(EvidenceKind::Screenshot);
        s.title = "  ".into();
        assert_eq!(sub(check_spec(&s)), "bad_evidence");
        s.title = "t".repeat(121);
        assert_eq!(sub(check_spec(&s)), "bad_evidence");
        assert_eq!(
            sub(check_spec(&spec(EvidenceKind::Benchmark))),
            "bad_evidence"
        );
        let mut b = spec(EvidenceKind::Benchmark);
        b.benchmark = vec![BenchmarkSeries {
            name: "p95".into(),
            unit: "ms".into(),
            baseline: 120.0,
            value: 85.0,
            lower_is_better: true,
        }];
        assert!(check_spec(&b).is_ok());
        b.benchmark[0].value = f64::NAN;
        assert_eq!(sub(check_spec(&b)), "bad_evidence");
        let mut wrong = spec(EvidenceKind::Screenshot);
        wrong.benchmark = vec![BenchmarkSeries::default()];
        assert_eq!(sub(check_spec(&wrong)), "bad_evidence");
        assert_eq!(
            sub(check_spec(&spec(EvidenceKind::TestReport))),
            "bad_evidence"
        );
        let mut t = spec(EvidenceKind::TestReport);
        t.test_report = Some(TestReport {
            passed: 3,
            ..Default::default()
        });
        assert!(check_spec(&t).is_ok());
        let mut p = spec(EvidenceKind::PreviewLink);
        assert_eq!(sub(check_spec(&p)), "bad_url");
        p.url = "http://preview.example.com".into();
        assert_eq!(sub(check_spec(&p)), "bad_url");
        p.url = "https://preview.example.com/12".into();
        assert!(check_spec(&p).is_ok());
        let mut u = spec(EvidenceKind::Video);
        u.url = "https://x.example".into();
        assert_eq!(sub(check_spec(&u)), "bad_evidence");
    }

    #[test]
    fn kinds_allow_their_roles_only() {
        use EvidenceKind as K;
        let png = |r| asset(r, "image/png", "The page");
        assert!(check_assets(K::Screenshot, &[png(Role::Main)]).is_ok());
        assert_eq!(sub(check_assets(K::Screenshot, &[])), "bad_evidence");
        assert_eq!(
            sub(check_assets(
                K::Screenshot,
                &[png(Role::Main), png(Role::Main)]
            )),
            "bad_evidence"
        );
        assert_eq!(
            sub(check_assets(K::Screenshot, &[png(Role::Before)])),
            "bad_evidence"
        );
        assert!(check_assets(K::BeforeAfter, &[png(Role::Before), png(Role::After)]).is_ok());
        assert_eq!(
            sub(check_assets(K::BeforeAfter, &[png(Role::After)])),
            "bad_evidence"
        );
        let video = vec![
            asset(Role::Main, "video/webm", ""),
            png(Role::Poster),
            asset(Role::Captions, "text/vtt", ""),
        ];
        assert_eq!(
            check_assets(K::Video, &video).unwrap(),
            vec![Kind::Webm, Kind::Png, Kind::Vtt]
        );
        assert_eq!(
            sub(check_assets(
                K::Video,
                &[asset(Role::Main, "image/png", "x")]
            )),
            "bad_media"
        );
        assert!(check_assets(K::Image, &vec![png(Role::Main); 12]).is_ok());
        assert_eq!(
            sub(check_assets(K::Image, &vec![png(Role::Main); 13])),
            "bad_evidence"
        );
        assert!(check_assets(K::Audio, &[asset(Role::Main, "audio/mpeg", "")]).is_ok());
        assert!(check_assets(K::Document, &[asset(Role::Main, "application/pdf", "")]).is_ok());
        assert_eq!(
            sub(check_assets(
                K::Document,
                &[asset(Role::Main, "text/vtt", "")]
            )),
            "bad_media"
        );
        assert!(check_assets(K::TestReport, &[]).is_ok());
        assert!(check_assets(K::TestReport, &[asset(Role::Main, "text/markdown", "")]).is_ok());
        assert!(check_assets(K::Benchmark, &[]).is_ok());
        assert_eq!(
            sub(check_assets(K::Benchmark, &[png(Role::Main)])),
            "bad_evidence"
        );
        assert_eq!(
            sub(check_assets(K::PreviewLink, &[png(Role::Main)])),
            "bad_evidence"
        );
    }

    #[test]
    fn images_need_alt_svg_is_refused_and_sizes_are_bounded() {
        use EvidenceKind as K;
        assert_eq!(
            sub(check_assets(
                K::Screenshot,
                &[asset(Role::Main, "image/png", " ")]
            )),
            "missing_alt"
        );
        for ct in [
            "image/svg+xml",
            "text/html",
            "application/javascript",
            "text/javascript",
        ] {
            assert_eq!(
                sub(check_assets(K::Screenshot, &[asset(Role::Main, ct, "alt")])),
                "bad_media",
                "{ct}"
            );
            assert_eq!(
                sub(check_assets(K::Document, &[asset(Role::Main, ct, "")])),
                "bad_media",
                "{ct}"
            );
        }
        let mut big = asset(Role::Main, "image/png", "alt");
        big.size = EVIDENCE_IMAGE.max_bytes + 1;
        let e = check_assets(K::Screenshot, &[big]).unwrap_err();
        assert_eq!(e.reason, ErrorReason::TooLarge);
        let mut video = asset(Role::Main, "video/mp4", "");
        video.size = EVIDENCE_VIDEO.max_bytes;
        assert!(check_assets(K::Video, &[video]).is_ok());
        let mut audio = asset(Role::Main, "audio/wav", "");
        audio.size = EVIDENCE_AUDIO.max_bytes + 1;
        assert_eq!(
            check_assets(K::Audio, &[audio]).unwrap_err().reason,
            ErrorReason::TooLarge
        );
    }

    #[test]
    fn artifact_files_are_declared_by_extension() {
        assert_eq!(text_kind_of_path("out/report.MD"), Some(Kind::Markdown));
        assert_eq!(text_kind_of_path("bench.csv"), Some(Kind::Csv));
        assert_eq!(text_kind_of_path("demo.vtt"), Some(Kind::Vtt));
        assert_eq!(text_kind_of_path("test.log"), Some(Kind::Text));
        assert_eq!(text_kind_of_path("index.html"), None);
        assert_eq!(text_kind_of_path("shot.png"), None);
        assert_eq!(text_kind_of_path("README"), None);
    }
}
