//! Path prefixes and path globs.
//!
//! Repository paths (SPEC §4.5) have no leading or trailing `/`. Policy files may write a path
//! with or without a leading `/`; the root is written `/`. Internally a prefix is stored without
//! slashes at either end, so the root is the empty string.

/// Normalizes a policy path prefix: strips leading and trailing `/`, rejects empty components,
/// `.` and `..`. Returns the prefix without slashes (the root is `""`).
pub fn normalize_prefix(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        if raw.is_empty() {
            return Err("path is empty; write `/` for the repository root".to_string());
        }
        return Ok(String::new());
    }
    for component in trimmed.split('/') {
        if component.is_empty() {
            return Err(format!("path `{raw}` has an empty component"));
        }
        if component == "." || component == ".." {
            return Err(format!("path `{raw}` contains `{component}`"));
        }
    }
    Ok(trimmed.to_string())
}

/// Renders a normalized prefix for humans and for rule labels: the root is `/`, anything else
/// gets a leading `/`.
pub fn display_prefix(prefix: &str) -> String {
    format!("/{prefix}")
}

/// Strips any leading `/` from a request path so `docs/a` and `/docs/a` are the same path.
pub fn normalize_request_path(path: &str) -> &str {
    path.trim_start_matches('/').trim_end_matches('/')
}

/// Component-wise prefix test: `docs` covers `docs` and `docs/a` but not `docs2`. The root
/// prefix `""` covers every path.
pub fn prefix_covers(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Number of components in a normalized prefix; the root has zero. Used to pick the longest
/// matching prefix.
pub fn prefix_depth(prefix: &str) -> usize {
    if prefix.is_empty() {
        0
    } else {
        prefix.split('/').count()
    }
}

/// A path glob over `/`-separated components.
///
/// - `**` as a whole component matches zero or more components.
/// - `*` matches any run of characters inside one component, `?` exactly one character.
/// - Everything else is literal. `[`, `]`, `{`, `}` and `!` are rejected so nobody mistakes
///   this for full gitignore syntax.
/// - A glob is anchored at the repository root; write `**/x` to match at any depth.
/// - A glob matches a path when it matches the path itself or any ancestor directory of it, so
///   `/schema` covers `schema/a.sql`. With `dir_only` it matches only proper ancestors.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Glob {
    source: String,
    components: Vec<String>,
    dir_only: bool,
}

impl Glob {
    /// Parses a glob as written in a policy file (leading `/` optional).
    pub fn parse(raw: &str) -> Result<Glob, String> {
        Self::parse_with(raw, false)
    }

    /// Parses a glob that only matches directories (proper ancestors of a path).
    pub fn parse_dir_only(raw: &str) -> Result<Glob, String> {
        Self::parse_with(raw, true)
    }

    fn parse_with(raw: &str, dir_only: bool) -> Result<Glob, String> {
        if let Some(bad) = raw
            .chars()
            .find(|c| matches!(c, '[' | ']' | '{' | '}' | '!'))
        {
            return Err(format!("glob `{raw}` uses unsupported character `{bad}`"));
        }
        let trimmed = raw.trim_matches('/');
        if trimmed.is_empty() {
            return Err(format!("glob `{raw}` is empty"));
        }
        let mut components = Vec::new();
        for c in trimmed.split('/') {
            if c.is_empty() {
                return Err(format!("glob `{raw}` has an empty component"));
            }
            if c.contains("**") && c != "**" {
                return Err(format!("glob `{raw}`: `**` must be a whole component"));
            }
            // Collapse repeated `**` components; they mean the same thing.
            if c == "**" && components.last().is_some_and(|l: &String| l == "**") {
                continue;
            }
            components.push(c.to_string());
        }
        Ok(Glob {
            source: format!("/{}", components.join("/")),
            components,
            dir_only,
        })
    }

    /// The normalized text of the glob (leading `/`, collapsed `**`).
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether the glob only matches directories.
    pub fn is_dir_only(&self) -> bool {
        self.dir_only
    }

    /// Tests a repository path (leading `/` optional).
    pub fn matches(&self, path: &str) -> bool {
        let path = normalize_request_path(path);
        if path.is_empty() {
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        let max = if self.dir_only {
            parts.len() - 1
        } else {
            parts.len()
        };
        (1..=max).any(|n| match_components(&self.components, &parts[..n]))
    }
}

fn match_components(pattern: &[String], parts: &[&str]) -> bool {
    match pattern.split_first() {
        None => parts.is_empty(),
        Some((head, rest)) if head == "**" => {
            (0..=parts.len()).any(|skip| match_components(rest, &parts[skip..]))
        }
        Some((head, rest)) => match parts.split_first() {
            Some((part, tail)) => match_component(head, part) && match_components(rest, tail),
            None => false,
        },
    }
}

fn match_component(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative wildcard match with single-star backtracking.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_component_wise() {
        assert!(prefix_covers("secrets", "secrets"));
        assert!(prefix_covers("secrets", "secrets/a"));
        assert!(!prefix_covers("secrets", "secrets2"));
        assert!(!prefix_covers("secrets", "secrets2/a"));
        assert!(prefix_covers("", "anything/at/all"));
        assert_eq!(normalize_prefix("/").unwrap(), "");
        assert_eq!(normalize_prefix("/docs/").unwrap(), "docs");
        assert!(normalize_prefix("").is_err());
        assert!(normalize_prefix("/a//b").is_err());
        assert!(normalize_prefix("/a/../b").is_err());
    }

    #[test]
    fn globs() {
        let g = Glob::parse("**/*.pem").unwrap();
        assert!(g.matches("key.pem"));
        assert!(g.matches("a/b/key.pem"));
        assert!(!g.matches("a/b/key.pem.txt"));
        let g = Glob::parse("/schema").unwrap();
        assert!(g.matches("schema"));
        assert!(g.matches("schema/a.sql"));
        assert!(!g.matches("schemas/a.sql"));
        let g = Glob::parse("src/*/mod.rs").unwrap();
        assert!(g.matches("src/a/mod.rs"));
        assert!(!g.matches("src/a/b/mod.rs"));
        let g = Glob::parse("/gen/**").unwrap();
        assert!(g.matches("gen/x/y"));
        let g = Glob::parse("a?c").unwrap();
        assert!(g.matches("abc"));
        assert!(!g.matches("abbc"));
        let d = Glob::parse_dir_only("**/docs").unwrap();
        assert!(d.matches("x/docs/a.md"));
        assert!(!d.matches("x/docs"));
        assert!(Glob::parse("[ab]").is_err());
        assert!(Glob::parse("a**").is_err());
        assert_eq!(Glob::parse("a/**/**/b").unwrap().as_str(), "/a/**/b");
    }
}
