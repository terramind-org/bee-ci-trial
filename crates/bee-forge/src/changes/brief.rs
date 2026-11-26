//! The brief of a change request (SPEC §11.9): pure validation, and the writes that store it
//! on the CR record ([`Changes::set_brief`], and `OpenChange.brief` through
//! [`super::NewChange::brief`]).
//!
//! The brief lives on the stored `ChangeRequest` (field `brief`), so it is written with the
//! CR's own optimistic version and read wherever the CR is. The server sets
//! `written_for_revision`, `author` and `updated_at`; whatever the caller sends in them is
//! ignored.

use bee_proto::v1::{ChangeBrief, ChangeRequest, Principal, Repo};

use super::model::PathClass;
use super::store::{Changes, current_rev};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;

/// Most characters of the outcome and of every text of a brief.
pub const MAX_TEXT_CHARS: usize = 280;
/// Most characters of one option of an ask.
pub const MAX_OPTION_CHARS: usize = 80;
/// Most highlights, asks and risks (each).
pub const MAX_ITEMS: usize = 20;
/// Options of one ask: at least, at most.
pub const ASK_OPTIONS: (usize, usize) = (2, 6);
/// Most paths of one highlight.
pub const MAX_PATHS: usize = 20;

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn text(field: &str, s: &str, max: usize) -> Result<()> {
    let n = chars(s);
    if n > max {
        return Err(ForgeError::invalid(
            "brief_too_long",
            format!("{field} has {n} characters, at most {max} are allowed"),
        ));
    }
    Ok(())
}

fn required(field: &str, s: &str) -> Result<()> {
    if s.trim().is_empty() {
        return Err(ForgeError::invalid(
            "bad_brief",
            format!("{field} is empty"),
        ));
    }
    Ok(())
}

fn count(field: &str, n: usize, max: usize) -> Result<()> {
    if n > max {
        return Err(ForgeError::invalid(
            "bad_brief",
            format!("{n} {field}, at most {max} are allowed"),
        ));
    }
    Ok(())
}

/// Whether `url` is an absolute `https` URL with a host and no white space.
pub fn https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty()
        && !host.starts_with(':')
        && !host.contains('@')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Checks a brief against SPEC §11.9 and returns it without the server-set fields: an
/// outcome and every text at most [`MAX_TEXT_CHARS`] characters and options at most
/// [`MAX_OPTION_CHARS`] (`brief_too_long`); at most [`MAX_ITEMS`] highlights, asks and risks,
/// [`ASK_OPTIONS`] options per ask, [`MAX_PATHS`] paths per highlight, and no empty highlight,
/// question, option or risk (`bad_brief`); `try_it_url` empty or `https` (`bad_url`).
pub fn validate(brief: &ChangeBrief) -> Result<ChangeBrief> {
    text("outcome", &brief.outcome, MAX_TEXT_CHARS)?;
    text("rollback", &brief.rollback, MAX_TEXT_CHARS)?;
    count("highlights", brief.highlights.len(), MAX_ITEMS)?;
    count("asks", brief.asks.len(), MAX_ITEMS)?;
    count("risks", brief.risks.len(), MAX_ITEMS)?;
    for (i, h) in brief.highlights.iter().enumerate() {
        let f = format!("highlights[{i}].text");
        required(&f, &h.text)?;
        text(&f, &h.text, MAX_TEXT_CHARS)?;
        count(
            &format!("paths in highlights[{i}]"),
            h.paths.len(),
            MAX_PATHS,
        )?;
        if h.paths.iter().any(|p| p.is_empty()) {
            return Err(ForgeError::invalid(
                "bad_brief",
                format!("highlights[{i}] names an empty path"),
            ));
        }
    }
    for (i, a) in brief.asks.iter().enumerate() {
        let f = format!("asks[{i}].question");
        required(&f, &a.question)?;
        text(&f, &a.question, MAX_TEXT_CHARS)?;
        let n = a.options.len();
        if n < ASK_OPTIONS.0 || n > ASK_OPTIONS.1 {
            return Err(ForgeError::invalid(
                "bad_brief",
                format!(
                    "asks[{i}] has {n} options, {} to {} are allowed",
                    ASK_OPTIONS.0, ASK_OPTIONS.1
                ),
            ));
        }
        for (j, o) in a.options.iter().enumerate() {
            let f = format!("asks[{i}].options[{j}]");
            required(&f, o)?;
            text(&f, o, MAX_OPTION_CHARS)?;
        }
    }
    for (i, r) in brief.risks.iter().enumerate() {
        let f = format!("risks[{i}].text");
        required(&f, &r.text)?;
        text(&f, &r.text, MAX_TEXT_CHARS)?;
        text(
            &format!("risks[{i}].mitigation"),
            &r.mitigation,
            MAX_TEXT_CHARS,
        )?;
    }
    if !brief.try_it_url.is_empty() && !https_url(&brief.try_it_url) {
        return Err(ForgeError::invalid(
            "bad_url",
            format!("try_it_url {:?} is not an https URL", brief.try_it_url),
        ));
    }
    Ok(ChangeBrief {
        written_for_revision: 0,
        author: None,
        updated_at: None,
        ..brief.clone()
    })
}

/// The brief as stored: `brief` (already validated) with the server-set fields.
pub(crate) fn stamped(
    brief: ChangeBrief,
    revision: u32,
    author: Principal,
    now_ms: u64,
) -> ChangeBrief {
    ChangeBrief {
        written_for_revision: revision,
        author: Some(author),
        updated_at: Some(ts(now_ms)),
        ..brief
    }
}

impl Changes {
    /// Replaces the brief of a CR (SPEC §11.9) and emits `cr.brief_set`; `expected_version`
    /// 0 means any. Callers authorize first (the CR's author, or repo Write).
    pub async fn set_brief(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        brief: &ChangeBrief,
        expected_version: u64,
    ) -> Result<ChangeRequest> {
        let brief = validate(brief)?;
        let author = self.actor(cx).await;
        let now = self.db.now_ms();
        self.mutate(cx, repo, id, expected_version, move |rec, ev| {
            let n = current_rev(rec.cr());
            rec.cr_mut().brief = Some(Box::new(stamped(brief, n, author, now)));
            ev.push(("cr.brief_set", String::new()));
            Ok(Some("cr.set_brief".into()))
        })
        .await
    }

    /// A CR as `cx` is served it, with revision `n` (0 = the current one) and the paths that
    /// revision changes with their diff classes (SPEC §11.3), in path order. The current
    /// revision's classes are the stored ones; an older revision is classified again from its
    /// head and base under the repo's current policy. `NOT_FOUND` for a revision the CR does
    /// not have.
    pub async fn revision_paths(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        n: u32,
    ) -> Result<(ChangeRequest, u32, Vec<PathClass>)> {
        let (rec, version) = self.record(cx, repo, id).await?;
        let cr = Self::served(&rec, version, &cx.actor_id);
        let current = current_rev(&cr);
        let mut paths = if n == 0 || n == current {
            rec.paths.clone()
        } else {
            let rev = cr.revisions.iter().find(|r| r.n == n).ok_or_else(|| {
                ForgeError::not_found(format!("revision {n} of change request #{}", cr.number))
            })?;
            let (Some(head), Some(base)) = (rev.head.as_ref(), rev.base.as_ref()) else {
                return Err(ForgeError::internal(format!(
                    "revision {n} of change request #{} has no head or base",
                    cr.number
                )));
            };
            self.analyze(repo, head, base).await?.2
        };
        paths.sort_by(|a, b| a.path.cmp(&b.path));
        let n = if n == 0 { current } else { n };
        Ok((cr, n, paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_proto::v1::{BriefAsk, BriefHighlight, BriefRisk, ErrorReason};

    fn good() -> ChangeBrief {
        ChangeBrief {
            outcome: "Checkout shows the saved card first".into(),
            highlights: vec![BriefHighlight {
                text: "Saved cards load before the form".into(),
                paths: vec!["web/checkout.tsx".into()],
            }],
            asks: vec![BriefAsk {
                question: "Ship behind a flag?".into(),
                options: vec!["Yes".into(), "No".into()],
            }],
            risks: vec![BriefRisk {
                text: "Card list can be slow".into(),
                mitigation: "Cached for 5 minutes".into(),
            }],
            rollback: "Revert the change".into(),
            try_it_url: "https://preview.example.com/cr/12".into(),
            written_for_revision: 9,
            author: Some(Principal {
                id: "spoof".into(),
                ..Default::default()
            }),
            updated_at: Some(ts(1)),
        }
    }

    fn sub(b: ChangeBrief) -> String {
        let e = validate(&b).unwrap_err();
        assert_eq!(e.reason, ErrorReason::InvalidArgument);
        e.subcode
    }

    #[test]
    fn validate_strips_server_fields() {
        let v = validate(&good()).unwrap();
        assert_eq!(v.written_for_revision, 0);
        assert!(v.author.is_none() && v.updated_at.is_none());
        assert_eq!(v.outcome, good().outcome);
        assert!(validate(&ChangeBrief::default()).is_ok());
    }

    #[test]
    fn validate_enforces_the_limits() {
        let mut b = good();
        b.outcome = "x".repeat(281);
        assert_eq!(sub(b), "brief_too_long");
        let mut b = good();
        b.outcome = "é".repeat(280);
        assert!(validate(&b).is_ok(), "characters, not bytes");
        let mut b = good();
        b.asks[0].options = vec!["only".into()];
        assert_eq!(sub(b), "bad_brief");
        let mut b = good();
        b.asks[0].options = (0..7).map(|i| i.to_string()).collect();
        assert_eq!(sub(b), "bad_brief");
        let mut b = good();
        b.asks[0].options[0] = "o".repeat(81);
        assert_eq!(sub(b), "brief_too_long");
        let mut b = good();
        b.highlights = vec![b.highlights[0].clone(); 21];
        assert_eq!(sub(b), "bad_brief");
        let mut b = good();
        b.risks[0].text = " ".into();
        assert_eq!(sub(b), "bad_brief");
        let mut b = good();
        b.highlights[0].paths = vec!["a".into(); 21];
        assert_eq!(sub(b), "bad_brief");
        for bad in [
            "http://preview.example.com",
            "https://",
            "https://a b",
            "javascript:alert(1)",
            "https://user@evil.example",
        ] {
            let mut b = good();
            b.try_it_url = bad.into();
            assert_eq!(sub(b), "bad_url", "{bad}");
        }
    }
}
