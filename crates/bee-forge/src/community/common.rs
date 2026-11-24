//! Helpers shared by the community stores: text limits, paging, event and audit drafts,
//! `@mention` parsing and small encodings.

use bee_proto::v1::{Event, Principal, RepoRef, SubjectKind, SubjectRef, repo_ref};

use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::store::{Cursor, Row, Scan, TableDef, Tx};

/// Longest title, name or subject line.
pub const MAX_TITLE: usize = 256;
/// Longest Markdown body or file content accepted inline.
pub const MAX_BODY: usize = 1 << 20;
/// Longest short free text (descriptions, notes).
pub const MAX_SHORT: usize = 4096;

/// Fails with `TOO_LARGE` when `v` is longer than `max` bytes.
pub fn max_len(field: &str, v: &str, max: usize) -> Result<()> {
    if v.len() > max {
        Err(ForgeError::too_large(field, max as u64, v.len() as u64))
    } else {
        Ok(())
    }
}

/// Fails with `INVALID_ARGUMENT` / `missing_field` when `v` is blank.
pub fn required(field: &str, v: &str) -> Result<()> {
    if v.trim().is_empty() {
        Err(ForgeError::missing(field))
    } else {
        Ok(())
    }
}

/// A repo reference by id.
pub fn repo_ref_id(id: &str) -> Option<RepoRef> {
    (!id.is_empty()).then(|| RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

/// A subject reference.
pub fn subject(kind: SubjectKind, repo_id: &str, id: &str) -> SubjectRef {
    SubjectRef {
        kind: kind as i32,
        repo: repo_ref_id(repo_id),
        id: id.to_string(),
    }
}

/// The repo id inside a repo reference, if it is an id.
pub fn ref_repo_id(r: &Option<RepoRef>) -> String {
    match r.as_ref().and_then(|r| r.repo.as_ref()) {
        Some(repo_ref::Repo::Id(i)) => i.clone(),
        _ => String::new(),
    }
}

/// Records a JSON-bodied event in the transaction's outbox.
pub async fn emit(
    tx: &mut Tx,
    kind: &str,
    org_id: &str,
    repo_id: &str,
    subject: SubjectRef,
    payload: serde_json::Value,
) -> Result<()> {
    tx.emit_event(Event {
        kind: kind.into(),
        repo_id: repo_id.into(),
        org_id: org_id.into(),
        subject: Some(subject),
        payload_json: payload.to_string(),
        ..Default::default()
    })
    .await
}

/// An audit draft for a community action.
pub fn draft(
    action: &str,
    org_id: &str,
    repo_id: &str,
    kind: SubjectKind,
    id: &str,
    after: &serde_json::Value,
) -> AuditDraft {
    AuditDraft::new(action, org_id)
        .repo_id(repo_id)
        .target(kind, id)
        .after(&after.to_string())
}

/// Encodes the position after `row` as an opaque page token.
pub fn token_of(row: &Row) -> String {
    format!("{}|{}|{}", row.n1.unwrap_or(0), row.scope, row.id)
}

/// Decodes a page token; an empty token is the start.
pub fn cursor_of(token: &str) -> Result<Option<Cursor>> {
    if token.is_empty() {
        return Ok(None);
    }
    let bad = || ForgeError::invalid("bad_page_token", "the page token is not valid");
    let mut parts = token.splitn(3, '|');
    let n1 = parts
        .next()
        .and_then(|p| p.parse::<i64>().ok())
        .ok_or_else(bad)?;
    let scope = parts.next().ok_or_else(bad)?.to_string();
    let id = parts.next().ok_or_else(bad)?.to_string();
    Ok(Some(Cursor { n1, scope, id }))
}

/// Runs `scan` after the position in `token`, returning at most `limit` rows and the token of
/// the next page (empty on the last page).
pub async fn paged(
    tx: &mut Tx,
    t: &TableDef,
    scan: Scan,
    token: &str,
    limit: usize,
) -> Result<(Vec<Row>, String)> {
    let mut scan = scan.after(cursor_of(token)?);
    scan.limit = limit + 1;
    let mut rows = tx.scan(t, &scan).await?;
    let mut next = String::new();
    if rows.len() > limit {
        rows.truncate(limit);
        if let Some(last) = rows.last() {
            next = token_of(last);
        }
    }
    Ok((rows, next))
}

/// Pages an in-memory list that is already in display order: items after the offset encoded
/// in `token`.
pub fn page_vec<T>(items: Vec<T>, token: &str, limit: usize) -> Result<(Vec<T>, String)> {
    let start = if token.is_empty() {
        0
    } else {
        token
            .parse::<usize>()
            .map_err(|_| ForgeError::invalid("bad_page_token", "the page token is not valid"))?
    };
    let total = items.len();
    let page: Vec<T> = items.into_iter().skip(start).take(limit).collect();
    let end = start + page.len();
    let next = if end < total {
        end.to_string()
    } else {
        String::new()
    };
    Ok((page, next))
}

/// `@login` mentions in Markdown, outside code spans and fences, sorted and deduplicated.
pub fn mentions(text: &str) -> Vec<String> {
    let mut clean = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
            } else if !in_code {
                clean.push(c);
            }
        }
        clean.push('\n');
    }
    let bytes = clean.as_bytes();
    let mut out = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || !is_login_byte(bytes[i - 1])) {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (is_login_byte(bytes[end]) || bytes[end] == b'.') {
                end += 1;
            }
            let name = clean[start..end].trim_end_matches('.');
            if !name.is_empty() && name.len() <= 100 {
                out.insert(name.to_string());
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

fn is_login_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Lower-case URL slug: letters and digits, single dashes between.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.extend(c.to_lowercase());
        } else {
            dash = true;
        }
    }
    out
}

/// A principal reference with only its id, for records that store ids.
pub fn principal_ref(id: &str) -> Principal {
    Principal {
        id: id.to_string(),
        ..Default::default()
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 without padding (SSH fingerprints).
pub fn base64_nopad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
    out
}

/// A day number (days since 1970-01-01) of a millisecond time.
pub fn day_of(ms: u64) -> i64 {
    (ms / 86_400_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_skip_code_and_dedupe() {
        let m = mentions("hi @alice and @bob.\n`@code` and email a@b.c\n```\n@fenced\n```\n@alice");
        assert_eq!(m, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn slugs_and_base64() {
        assert_eq!(slugify("Q&A  Help!"), "q-a-help");
        assert_eq!(base64_nopad(b"ab"), "YWI");
        assert_eq!(base64_nopad(b"abc"), "YWJj");
    }

    #[test]
    fn page_tokens_round_trip() {
        let row = Row::new("s", "i").n1(7);
        let c = cursor_of(&token_of(&row)).unwrap().unwrap();
        assert_eq!((c.n1, c.scope.as_str(), c.id.as_str()), (7, "s", "i"));
        let (p, next) = page_vec(vec![1, 2, 3], "", 2).unwrap();
        assert_eq!((p, next.as_str()), (vec![1, 2], "2"));
        assert!(cursor_of("x").is_err());
    }
}
