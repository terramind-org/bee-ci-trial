//! Markdown to sanitized HTML for `body_html` fields (issues and their comments).
//!
//! The Markdown is parsed with `pulldown-cmark` (CommonMark plus tables, strikethrough and
//! task lists) and the HTML is emitted here from a fixed set of tags, so the output is safe by
//! construction:
//!
//! - raw HTML in the source (blocks and inline) is escaped and shown as text;
//! - link and image URLs are kept only for `http:`, `https:` and `mailto:` and for relative
//!   URLs (`/path`, `path`, `#anchor`, `?query`); any other scheme (`javascript:`, `data:`, ...)
//!   drops the link (its text stays) or the image (its alt text stays);
//! - every text and attribute value is escaped; no `style`, event handler or `id` attribute
//!   is ever written.
//!
//! The rendering is a pure function of the input.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Escapes text for an HTML text node or a double-quoted attribute value.
fn escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

/// Whether a link or image URL may be emitted.
pub fn safe_url(url: &str) -> bool {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if ["http://", "https://", "mailto:"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return true;
    }
    // Relative: no scheme before the first '/', '?' or '#'.
    let head = u.split(['/', '?', '#']).next().unwrap_or("");
    !head.contains(':') && !u.chars().any(|c| c.is_control())
}

fn heading(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Renders Markdown as sanitized HTML.
pub fn render_html(markdown: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut out = String::with_capacity(markdown.len() * 3 / 2);
    // Closing markup of each open tag; images collect their alt text into an attribute.
    let mut closers: Vec<&'static str> = Vec::new();
    let mut in_head = false;
    let mut image_depth = 0usize;
    for ev in Parser::new_ext(markdown, opts) {
        if image_depth > 0 {
            match ev {
                Event::Start(_) => {
                    image_depth += 1;
                    closers.push("");
                }
                Event::End(_) => {
                    image_depth -= 1;
                    out.push_str(closers.pop().unwrap_or(""));
                }
                Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                    escape(&mut out, &t)
                }
                _ => {}
            }
            continue;
        }
        match ev {
            Event::Start(tag) => {
                let close: &'static str = match tag {
                    Tag::Paragraph => {
                        out.push_str("<p>");
                        "</p>\n"
                    }
                    Tag::Heading { level, .. } => {
                        let n = heading(level);
                        out.push_str(&format!("<h{n}>"));
                        [
                            "", "</h1>\n", "</h2>\n", "</h3>\n", "</h4>\n", "</h5>\n", "</h6>\n",
                        ][n as usize]
                    }
                    Tag::BlockQuote(_) => {
                        out.push_str("<blockquote>\n");
                        "</blockquote>\n"
                    }
                    Tag::CodeBlock(kind) => {
                        out.push_str("<pre><code");
                        if let CodeBlockKind::Fenced(lang) = kind {
                            let lang = lang.split_whitespace().next().unwrap_or("");
                            if !lang.is_empty() {
                                out.push_str(" class=\"language-");
                                escape(&mut out, lang);
                                out.push('"');
                            }
                        }
                        out.push('>');
                        "</code></pre>\n"
                    }
                    Tag::List(Some(start)) => {
                        if start == 1 {
                            out.push_str("<ol>\n");
                        } else {
                            out.push_str(&format!("<ol start=\"{start}\">\n"));
                        }
                        "</ol>\n"
                    }
                    Tag::List(None) => {
                        out.push_str("<ul>\n");
                        "</ul>\n"
                    }
                    Tag::Item => {
                        out.push_str("<li>");
                        "</li>\n"
                    }
                    Tag::Table(_) => {
                        out.push_str("<table>\n");
                        "</tbody></table>\n"
                    }
                    Tag::TableHead => {
                        in_head = true;
                        out.push_str("<thead><tr>");
                        "</tr></thead><tbody>\n"
                    }
                    Tag::TableRow => {
                        out.push_str("<tr>");
                        "</tr>\n"
                    }
                    Tag::TableCell => {
                        if in_head {
                            out.push_str("<th>");
                            "</th>"
                        } else {
                            out.push_str("<td>");
                            "</td>"
                        }
                    }
                    Tag::Emphasis => {
                        out.push_str("<em>");
                        "</em>"
                    }
                    Tag::Strong => {
                        out.push_str("<strong>");
                        "</strong>"
                    }
                    Tag::Strikethrough => {
                        out.push_str("<del>");
                        "</del>"
                    }
                    Tag::Link {
                        dest_url, title, ..
                    } => {
                        if safe_url(&dest_url) {
                            out.push_str("<a href=\"");
                            escape(&mut out, dest_url.trim());
                            out.push('"');
                            if !title.is_empty() {
                                out.push_str(" title=\"");
                                escape(&mut out, &title);
                                out.push('"');
                            }
                            out.push_str(" rel=\"nofollow noopener\">");
                            "</a>"
                        } else {
                            ""
                        }
                    }
                    Tag::Image { dest_url, .. } => {
                        image_depth = 1;
                        if safe_url(&dest_url) {
                            out.push_str("<img src=\"");
                            escape(&mut out, dest_url.trim());
                            out.push_str("\" alt=\"");
                            "\">"
                        } else {
                            ""
                        }
                    }
                    // Raw HTML blocks: their contents arrive as `Html` events (escaped below).
                    _ => "",
                };
                closers.push(close);
            }
            Event::End(end) => {
                if matches!(end, TagEnd::TableHead) {
                    in_head = false;
                }
                out.push_str(closers.pop().unwrap_or(""));
            }
            Event::Text(t) => escape(&mut out, &t),
            Event::Code(t) => {
                out.push_str("<code>");
                escape(&mut out, &t);
                out.push_str("</code>");
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                out.push_str("<code>");
                escape(&mut out, &t);
                out.push_str("</code>");
            }
            Event::Html(t) | Event::InlineHtml(t) => escape(&mut out, &t),
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br>\n"),
            Event::Rule => out.push_str("<hr>\n"),
            Event::TaskListMarker(done) => out.push_str(if done {
                "<input type=\"checkbox\" checked disabled> "
            } else {
                "<input type=\"checkbox\" disabled> "
            }),
            Event::FootnoteReference(t) => escape(&mut out, &format!("[^{t}]")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_markdown() {
        assert_eq!(
            render_html("# Title\n\nSome **bold** and `code`."),
            "<h1>Title</h1>\n<p>Some <strong>bold</strong> and <code>code</code>.</p>\n"
        );
        let list = render_html("- [x] done\n- [ ] todo\n");
        assert!(
            list.contains("<input type=\"checkbox\" checked disabled> done"),
            "{list}"
        );
        let code = render_html("```rust\nfn a() {}\n```\n");
        assert_eq!(
            code,
            "<pre><code class=\"language-rust\">fn a() {}\n</code></pre>\n"
        );
        let table = render_html("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(
            table.contains("<th>a</th>") && table.contains("<td>2</td>"),
            "{table}"
        );
    }

    #[test]
    fn raw_html_and_unsafe_urls_never_survive() {
        let h = render_html("<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)>");
        assert!(!h.contains("<script") && !h.contains("<img"), "{h}");
        assert!(h.contains("&lt;script&gt;"), "{h}");
        let l = render_html("[click](javascript:alert(1)) [ok](https://example.com \"t\")");
        assert!(!l.contains("href=\"javascript"), "{l}");
        assert!(l.contains("click"), "{l}");
        assert!(
            l.contains("<a href=\"https://example.com\" title=\"t\""),
            "{l}"
        );
        let i = render_html("![alt *x*](data:image/png;base64,AA) ![pic](/a.png)");
        assert!(!i.contains("data:"), "{i}");
        assert!(i.contains("<img src=\"/a.png\" alt=\"pic\">"), "{i}");
        let q = render_html("[x](\"onmouseover=1)");
        assert!(!q.contains("\"onmouseover"), "{q}");
    }

    #[test]
    fn relative_and_mail_links_are_kept() {
        assert!(safe_url("/docs/a.md"));
        assert!(safe_url("../b"));
        assert!(safe_url("#section"));
        assert!(safe_url("mailto:a@b.c"));
        assert!(!safe_url("JavaScript:x"));
        assert!(!safe_url("vbscript:x"));
    }
}
