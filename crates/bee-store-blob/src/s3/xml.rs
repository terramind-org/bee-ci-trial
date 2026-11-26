//! Just enough XML for S3 responses: element text by name, and repeated elements.

/// The text between the first `<tag>` and its `</tag>`.
pub(crate) fn first<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// The inner text of every `<tag>…</tag>` element, in order.
pub(crate) fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

/// Replaces the five predefined entities.
pub(crate) fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_elements() {
        let xml = "<R><IsTruncated>true</IsTruncated><Contents><Key>a&amp;b</Key><Size>3</Size></Contents><Contents><Key>c</Key><Size>4</Size></Contents></R>";
        assert_eq!(first(xml, "IsTruncated"), Some("true"));
        let c = blocks(xml, "Contents");
        assert_eq!(c.len(), 2);
        assert_eq!(unescape(first(c[0], "Key").unwrap()), "a&b");
        assert_eq!(first(c[1], "Size"), Some("4"));
        assert_eq!(first(xml, "Missing"), None);
        assert_eq!(first(xml, "Ke"), None);
    }
}
