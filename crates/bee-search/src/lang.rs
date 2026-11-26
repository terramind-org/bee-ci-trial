//! Language detection from a path (extension or well-known file name) and, failing that, a
//! shebang line.
//!
//! Detection is a pure function of the path and the first line of content. Each language has
//! a stable numeric code used by the on-disk index; codes are never reused.

/// A programming or markup language the search index recognizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(missing_docs)]
pub enum Language {
    Rust,
    Go,
    Python,
    JavaScript,
    TypeScript,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Shell,
    Scala,
    Perl,
    Lua,
    Haskell,
    Elixir,
    Erlang,
    ObjectiveC,
    Sql,
    Html,
    Css,
    Markdown,
    Json,
    Yaml,
    Toml,
    Xml,
    Protobuf,
    Dockerfile,
    Makefile,
    Text,
}

/// Every language, in code order.
pub const ALL_LANGUAGES: &[Language] = &[
    Language::Rust,
    Language::Go,
    Language::Python,
    Language::JavaScript,
    Language::TypeScript,
    Language::Java,
    Language::C,
    Language::Cpp,
    Language::CSharp,
    Language::Ruby,
    Language::Php,
    Language::Kotlin,
    Language::Swift,
    Language::Shell,
    Language::Scala,
    Language::Perl,
    Language::Lua,
    Language::Haskell,
    Language::Elixir,
    Language::Erlang,
    Language::ObjectiveC,
    Language::Sql,
    Language::Html,
    Language::Css,
    Language::Markdown,
    Language::Json,
    Language::Yaml,
    Language::Toml,
    Language::Xml,
    Language::Protobuf,
    Language::Dockerfile,
    Language::Makefile,
    Language::Text,
];

impl Language {
    /// The canonical lower-case name used in `lang:` and facets.
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Java => "java",
            Language::C => "c",
            Language::Cpp => "c++",
            Language::CSharp => "c#",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Kotlin => "kotlin",
            Language::Swift => "swift",
            Language::Shell => "shell",
            Language::Scala => "scala",
            Language::Perl => "perl",
            Language::Lua => "lua",
            Language::Haskell => "haskell",
            Language::Elixir => "elixir",
            Language::Erlang => "erlang",
            Language::ObjectiveC => "objective-c",
            Language::Sql => "sql",
            Language::Html => "html",
            Language::Css => "css",
            Language::Markdown => "markdown",
            Language::Json => "json",
            Language::Yaml => "yaml",
            Language::Toml => "toml",
            Language::Xml => "xml",
            Language::Protobuf => "protobuf",
            Language::Dockerfile => "dockerfile",
            Language::Makefile => "makefile",
            Language::Text => "text",
        }
    }

    /// The stable numeric code (1-based; 0 means "unknown" on disk).
    pub fn code(self) -> u16 {
        ALL_LANGUAGES
            .iter()
            .position(|l| *l == self)
            .map_or(0, |i| i as u16 + 1)
    }

    /// The language with a numeric code.
    pub fn from_code(code: u16) -> Option<Language> {
        ALL_LANGUAGES
            .get(usize::from(code).checked_sub(1)?)
            .copied()
    }

    /// Parses a `lang:` value: a canonical name or a common alias, case-insensitively.
    pub fn parse(text: &str) -> Option<Language> {
        let t = text.to_ascii_lowercase();
        let alias = match t.as_str() {
            "rs" => Some(Language::Rust),
            "golang" => Some(Language::Go),
            "py" | "python3" => Some(Language::Python),
            "js" | "node" | "jsx" => Some(Language::JavaScript),
            "ts" | "tsx" => Some(Language::TypeScript),
            "cpp" | "cxx" | "cplusplus" => Some(Language::Cpp),
            "csharp" | "cs" => Some(Language::CSharp),
            "rb" => Some(Language::Ruby),
            "kt" => Some(Language::Kotlin),
            "sh" | "bash" | "zsh" => Some(Language::Shell),
            "objc" | "objectivec" => Some(Language::ObjectiveC),
            "md" => Some(Language::Markdown),
            "yml" => Some(Language::Yaml),
            "proto" => Some(Language::Protobuf),
            "docker" => Some(Language::Dockerfile),
            "make" => Some(Language::Makefile),
            "txt" | "plaintext" => Some(Language::Text),
            _ => None,
        };
        alias.or_else(|| ALL_LANGUAGES.iter().copied().find(|l| l.name() == t))
    }
}

/// Detects the language of a file from its path, then its shebang line.
pub fn detect(path: &[u8], content: &[u8]) -> Option<Language> {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    if let Some(l) = by_file_name(&name) {
        return Some(l);
    }
    if let Some((_, ext)) = name.rsplit_once('.')
        && let Some(l) = by_extension(ext)
    {
        return Some(l);
    }
    shebang(content)
}

fn by_file_name(name: &str) -> Option<Language> {
    Some(match name {
        "dockerfile" | "containerfile" => Language::Dockerfile,
        "makefile" | "gnumakefile" => Language::Makefile,
        "gemfile" | "rakefile" => Language::Ruby,
        "cargo.lock" => Language::Toml,
        _ if name.starts_with("dockerfile.") => Language::Dockerfile,
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<Language> {
    Some(match ext {
        "rs" => Language::Rust,
        "go" => Language::Go,
        "py" | "pyi" | "pyw" => Language::Python,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "ts" | "mts" | "cts" | "tsx" => Language::TypeScript,
        "java" => Language::Java,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "c++" => Language::Cpp,
        "cs" => Language::CSharp,
        "rb" | "rake" | "gemspec" => Language::Ruby,
        "php" => Language::Php,
        "kt" | "kts" => Language::Kotlin,
        "swift" => Language::Swift,
        "sh" | "bash" | "zsh" | "ksh" => Language::Shell,
        "scala" | "sc" => Language::Scala,
        "pl" | "pm" => Language::Perl,
        "lua" => Language::Lua,
        "hs" => Language::Haskell,
        "ex" | "exs" => Language::Elixir,
        "erl" | "hrl" => Language::Erlang,
        "m" | "mm" => Language::ObjectiveC,
        "sql" => Language::Sql,
        "html" | "htm" => Language::Html,
        "css" | "scss" | "less" => Language::Css,
        "md" | "markdown" => Language::Markdown,
        "json" => Language::Json,
        "yaml" | "yml" => Language::Yaml,
        "toml" => Language::Toml,
        "xml" | "xsd" | "svg" => Language::Xml,
        "proto" => Language::Protobuf,
        "mk" => Language::Makefile,
        "txt" => Language::Text,
        _ => return None,
    })
}

fn shebang(content: &[u8]) -> Option<Language> {
    let line = content.split(|&b| b == b'\n').next()?;
    let rest = line.strip_prefix(b"#!")?;
    let text = String::from_utf8_lossy(rest);
    let mut words = text.split_whitespace();
    let mut prog = words.next()?.rsplit('/').next()?.to_string();
    if prog == "env" {
        prog = words.find(|w| !w.starts_with('-'))?.to_string();
    }
    let prog = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match prog {
        "python" => Language::Python,
        "node" | "nodejs" | "deno" => Language::JavaScript,
        "bash" | "sh" | "zsh" | "dash" | "ksh" => Language::Shell,
        "ruby" => Language::Ruby,
        "perl" => Language::Perl,
        "php" => Language::Php,
        "lua" => Language::Lua,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_names_and_shebangs() {
        assert_eq!(detect(b"src/lib.rs", b""), Some(Language::Rust));
        assert_eq!(detect(b"a/B.JAVA", b""), Some(Language::Java));
        assert_eq!(detect(b"Dockerfile", b""), Some(Language::Dockerfile));
        assert_eq!(detect(b"x/Makefile", b""), Some(Language::Makefile));
        assert_eq!(
            detect(b"bin/tool", b"#!/usr/bin/env python3\nprint(1)"),
            Some(Language::Python)
        );
        assert_eq!(detect(b"bin/run", b"#!/bin/bash\n"), Some(Language::Shell));
        assert_eq!(
            detect(b"bin/x", b"#!/usr/bin/env -S node\n"),
            Some(Language::JavaScript)
        );
        assert_eq!(detect(b"README", b"hello"), None);
    }

    #[test]
    fn codes_round_trip_and_aliases() {
        for l in ALL_LANGUAGES {
            assert_eq!(Language::from_code(l.code()), Some(*l));
            assert_eq!(Language::parse(l.name()), Some(*l));
        }
        assert_eq!(Language::from_code(0), None);
        assert_eq!(Language::parse("RS"), Some(Language::Rust));
        assert_eq!(Language::parse("golang"), Some(Language::Go));
        assert_eq!(Language::parse("cobol"), None);
    }
}
