//! File-extension language table for gists and language statistics.

use bee_proto::v1::LanguageType;

/// `(extension, language, type, color)`.
const TABLE: &[(&str, &str, LanguageType, &str)] = &[
    ("rs", "Rust", LanguageType::Programming, "dea584"),
    ("py", "Python", LanguageType::Programming, "3572a5"),
    ("js", "JavaScript", LanguageType::Programming, "f1e05a"),
    ("mjs", "JavaScript", LanguageType::Programming, "f1e05a"),
    ("jsx", "JavaScript", LanguageType::Programming, "f1e05a"),
    ("ts", "TypeScript", LanguageType::Programming, "3178c6"),
    ("tsx", "TypeScript", LanguageType::Programming, "3178c6"),
    ("go", "Go", LanguageType::Programming, "00add8"),
    ("java", "Java", LanguageType::Programming, "b07219"),
    ("kt", "Kotlin", LanguageType::Programming, "a97bff"),
    ("swift", "Swift", LanguageType::Programming, "f05138"),
    ("c", "C", LanguageType::Programming, "555555"),
    ("h", "C", LanguageType::Programming, "555555"),
    ("cc", "C++", LanguageType::Programming, "f34b7d"),
    ("cpp", "C++", LanguageType::Programming, "f34b7d"),
    ("hpp", "C++", LanguageType::Programming, "f34b7d"),
    ("cs", "C#", LanguageType::Programming, "178600"),
    ("rb", "Ruby", LanguageType::Programming, "701516"),
    ("php", "PHP", LanguageType::Programming, "4f5d95"),
    ("sh", "Shell", LanguageType::Programming, "89e051"),
    ("bash", "Shell", LanguageType::Programming, "89e051"),
    ("lua", "Lua", LanguageType::Programming, "000080"),
    ("zig", "Zig", LanguageType::Programming, "ec915c"),
    ("sql", "SQL", LanguageType::Data, "e38c00"),
    ("html", "HTML", LanguageType::Markup, "e34c26"),
    ("htm", "HTML", LanguageType::Markup, "e34c26"),
    ("css", "CSS", LanguageType::Markup, "563d7c"),
    ("scss", "SCSS", LanguageType::Markup, "c6538c"),
    ("md", "Markdown", LanguageType::Prose, "083fa1"),
    ("markdown", "Markdown", LanguageType::Prose, "083fa1"),
    ("rst", "reStructuredText", LanguageType::Prose, "141414"),
    ("txt", "Text", LanguageType::Prose, "cccccc"),
    ("json", "JSON", LanguageType::Data, "292929"),
    ("yaml", "YAML", LanguageType::Data, "cb171e"),
    ("yml", "YAML", LanguageType::Data, "cb171e"),
    ("toml", "TOML", LanguageType::Data, "9c4221"),
    ("xml", "XML", LanguageType::Data, "0060ac"),
    ("proto", "Protocol Buffer", LanguageType::Data, "cccccc"),
];

/// The language of a file name: `(name, type, color)`.
pub fn language_of(path: &str) -> Option<(&'static str, LanguageType, &'static str)> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|(e, ..)| *e == ext)
        .map(|(_, n, t, c)| (*n, *t, *c))
}

/// A content type guess for a file name.
pub fn content_type_of(path: &str) -> &'static str {
    match language_of(path).map(|l| l.0) {
        Some("JSON") => "application/json",
        Some("HTML") => "text/html",
        Some("CSS") => "text/css",
        Some("Markdown") => "text/markdown",
        Some("XML") => "application/xml",
        Some(_) => "text/plain",
        None => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_extension() {
        assert_eq!(language_of("src/main.rs").map(|l| l.0), Some("Rust"));
        assert_eq!(language_of("README.MD").map(|l| l.0), Some("Markdown"));
        assert!(language_of("Makefile").is_none());
        assert_eq!(content_type_of("a.json"), "application/json");
    }
}
