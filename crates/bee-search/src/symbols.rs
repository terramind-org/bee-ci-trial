//! A lightweight tags extractor: symbol definitions found by per-language line regexes.
//!
//! This is search-based navigation, not a compiler: it reads one line at a time and knows
//! nothing about scopes beyond indentation. Documented limits (see docs/spec/search.md):
//!
//! - A definition must start on one line (`fn name`, `def name`, `class Name` ...); names
//!   split across lines, or produced by macros and code generation, are missed.
//! - Containers (impl, class, module ...) are tracked by indentation, so a method is
//!   attributed to the nearest less-indented container above it. Badly indented code gets
//!   the wrong container but never a wrong name.
//! - Definitions inside comments and strings that look like code are reported.
//! - C and C++ functions are recognized only at column 0 with the opening `(` on the line.

use std::sync::LazyLock;

use regex::bytes::Regex;

use crate::lang::Language;

/// Kind of a symbol (mirrors `bee.v1.SymbolKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(missing_docs)]
pub enum SymbolKind {
    Module,
    Namespace,
    Class,
    Method,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    Struct,
    Trait,
    TypeAlias,
    Macro,
}

impl SymbolKind {
    /// Lower-case name used in `kind:` filters and docs.
    pub fn name(self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Namespace => "namespace",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Field => "field",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Function => "function",
            SymbolKind::Variable => "variable",
            SymbolKind::Constant => "constant",
            SymbolKind::Struct => "struct",
            SymbolKind::Trait => "trait",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Macro => "macro",
        }
    }

    /// Parses [`SymbolKind::name`].
    pub fn parse(text: &str) -> Option<SymbolKind> {
        ALL_KINDS
            .iter()
            .copied()
            .find(|k| k.name().eq_ignore_ascii_case(text))
    }

    fn is_container(self) -> bool {
        matches!(
            self,
            SymbolKind::Module
                | SymbolKind::Namespace
                | SymbolKind::Class
                | SymbolKind::Interface
                | SymbolKind::Struct
                | SymbolKind::Trait
                | SymbolKind::Enum
        )
    }
}

const ALL_KINDS: &[SymbolKind] = &[
    SymbolKind::Module,
    SymbolKind::Namespace,
    SymbolKind::Class,
    SymbolKind::Method,
    SymbolKind::Field,
    SymbolKind::Constructor,
    SymbolKind::Enum,
    SymbolKind::Interface,
    SymbolKind::Function,
    SymbolKind::Variable,
    SymbolKind::Constant,
    SymbolKind::Struct,
    SymbolKind::Trait,
    SymbolKind::TypeAlias,
    SymbolKind::Macro,
];

/// One symbol definition.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Symbol {
    /// The name.
    pub name: String,
    /// The kind.
    pub kind: SymbolKind,
    /// 1-based line.
    pub line: u32,
    /// 0-based byte column of the name.
    pub column: u32,
    /// Byte column one past the name.
    pub end_column: u32,
    /// Enclosing container's name, if any.
    pub container: Option<String>,
    /// The definition line, trimmed (one-line signature).
    pub signature: String,
}

impl Symbol {
    /// `container::name` (or `container.name` for dotted languages), or the bare name.
    pub fn qualified_name(&self, language: Language) -> String {
        match &self.container {
            None => self.name.clone(),
            Some(c) => {
                let sep = match language {
                    Language::Rust | Language::Cpp | Language::Php => "::",
                    _ => ".",
                };
                format!("{c}{sep}{}", self.name)
            }
        }
    }
}

/// A rule: lines matching `re` define a symbol named by capture group 1.
struct Rule {
    re: Regex,
    kind: SymbolKind,
    /// Kind to report when inside a container (functions become methods).
    in_container: Option<SymbolKind>,
    /// Pushes a container scope without reporting a symbol (e.g. Rust `impl`).
    scope_only: bool,
}

fn rule(re: &str, kind: SymbolKind) -> Rule {
    Rule {
        re: Regex::new(re).unwrap_or_else(|e| panic!("bad built-in symbol regex {re}: {e}")),
        kind,
        in_container: None,
        scope_only: false,
    }
}

fn method(re: &str) -> Rule {
    let mut r = rule(re, SymbolKind::Function);
    r.in_container = Some(SymbolKind::Method);
    r
}

fn scope(re: &str, kind: SymbolKind) -> Rule {
    let mut r = rule(re, kind);
    r.scope_only = true;
    r
}

const ID: &str = r"([A-Za-z_][A-Za-z0-9_]*)";

static RUST: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let vis = r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?";
    vec![
        method(&format!(
            r#"{vis}(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+{ID}"#
        )),
        rule(&format!(r"{vis}struct\s+{ID}"), SymbolKind::Struct),
        rule(&format!(r"{vis}enum\s+{ID}"), SymbolKind::Enum),
        rule(&format!(r"{vis}union\s+{ID}"), SymbolKind::Struct),
        rule(
            &format!(r"{vis}(?:unsafe\s+)?trait\s+{ID}"),
            SymbolKind::Trait,
        ),
        rule(&format!(r"{vis}type\s+{ID}"), SymbolKind::TypeAlias),
        rule(&format!(r"{vis}mod\s+{ID}"), SymbolKind::Module),
        rule(&format!(r"{vis}const\s+{ID}\s*:"), SymbolKind::Constant),
        rule(
            &format!(r"{vis}static\s+(?:mut\s+)?{ID}\s*:"),
            SymbolKind::Variable,
        ),
        rule(&format!(r"^\s*macro_rules!\s*{ID}"), SymbolKind::Macro),
        scope(
            &format!(
                r"^\s*(?:unsafe\s+)?impl(?:\s*<[^>]*>)?\s+(?:[^\s{{]+\s+for\s+)?(?:[A-Za-z_][A-Za-z0-9_]*::)*{ID}"
            ),
            SymbolKind::Struct,
        ),
    ]
});

static GO: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let mut m = rule(&format!(r"^func\s+\([^)]*\)\s*{ID}"), SymbolKind::Method);
    m.in_container = None;
    vec![
        m,
        rule(&format!(r"^func\s+{ID}"), SymbolKind::Function),
        rule(&format!(r"^type\s+{ID}\s+struct\b"), SymbolKind::Struct),
        rule(
            &format!(r"^type\s+{ID}\s+interface\b"),
            SymbolKind::Interface,
        ),
        rule(&format!(r"^type\s+{ID}"), SymbolKind::TypeAlias),
        rule(&format!(r"^const\s+{ID}"), SymbolKind::Constant),
        rule(&format!(r"^var\s+{ID}"), SymbolKind::Variable),
    ]
});

static PYTHON: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        method(&format!(r"^\s*(?:async\s+)?def\s+{ID}")),
        rule(&format!(r"^\s*class\s+{ID}"), SymbolKind::Class),
        rule(
            r"^([A-Z][A-Z0-9_]*)\s*(?::[^=]*)?=[^=]",
            SymbolKind::Constant,
        ),
    ]
});

static JS: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let id = r"([A-Za-z_$][A-Za-z0-9_$]*)";
    vec![
        rule(
            &format!(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*{id}"),
            SymbolKind::Function,
        ),
        rule(
            &format!(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+{id}"),
            SymbolKind::Class,
        ),
        rule(
            &format!(
                r"^\s*(?:export\s+)?(?:const|let|var)\s+{id}\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][A-Za-z0-9_$]*\s*=>)"
            ),
            SymbolKind::Function,
        ),
        rule(
            &format!(r"^\s*(?:export\s+)?interface\s+{id}"),
            SymbolKind::Interface,
        ),
        rule(
            &format!(r"^\s*(?:export\s+)?type\s+{id}\s*(?:<[^>]*>)?\s*="),
            SymbolKind::TypeAlias,
        ),
        rule(
            &format!(r"^\s*(?:export\s+)?(?:const\s+)?enum\s+{id}"),
            SymbolKind::Enum,
        ),
        rule(
            &format!(
                r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*(?:\*\s*)?{id}\s*\([^)]*\)\s*(?::\s*[^{{]+)?\{{"
            ),
            SymbolKind::Method,
        ),
    ]
});

static JAVA_LIKE: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let mods = r"(?:(?:public|private|protected|internal|static|final|abstract|sealed|partial|open|data|override|virtual|async|synchronized|native|readonly|unsafe|extern|new)\s+)*";
    vec![
        rule(
            &format!(r"^\s*{mods}(?:class|record)\s+{ID}"),
            SymbolKind::Class,
        ),
        rule(
            &format!(r"^\s*{mods}(?:interface|@interface)\s+{ID}"),
            SymbolKind::Interface,
        ),
        rule(
            &format!(r"^\s*{mods}enum\s+(?:class\s+)?{ID}"),
            SymbolKind::Enum,
        ),
        rule(&format!(r"^\s*{mods}struct\s+{ID}"), SymbolKind::Struct),
        rule(
            r"^\s*namespace\s+([A-Za-z_][A-Za-z0-9_.]*)",
            SymbolKind::Namespace,
        ),
        method(&format!(
            r"^\s*{mods}fun\s+(?:<[^>]*>\s*)?(?:[A-Za-z_][A-Za-z0-9_<>?]*\.)?{ID}"
        )),
        rule(&format!(r"^\s*{mods}object\s+{ID}"), SymbolKind::Class),
        rule(
            &format!(
                r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|override|virtual|async|synchronized|native|unsafe|extern|new)\s+)+(?:<[^>]*>\s*)?[A-Za-z_][A-Za-z0-9_<>\[\]?,. ]*\s+{ID}\s*\("
            ),
            SymbolKind::Method,
        ),
    ]
});

static C_LIKE: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(&format!(r"^\s*#\s*define\s+{ID}"), SymbolKind::Macro),
        rule(
            &format!(r"^\s*(?:typedef\s+)?struct\s+{ID}\s*\{{"),
            SymbolKind::Struct,
        ),
        rule(
            &format!(r"^\s*(?:typedef\s+)?union\s+{ID}\s*\{{"),
            SymbolKind::Struct,
        ),
        rule(
            &format!(r"^\s*(?:typedef\s+)?enum\s+(?:class\s+)?{ID}"),
            SymbolKind::Enum,
        ),
        rule(
            &format!(r"^\s*(?:template\s*<[^>]*>\s*)?class\s+{ID}"),
            SymbolKind::Class,
        ),
        rule(&format!(r"^\s*namespace\s+{ID}"), SymbolKind::Namespace),
        rule(
            &format!(r"^\s*typedef\s+[^;]*\b{ID}\s*;"),
            SymbolKind::TypeAlias,
        ),
        rule(
            &format!(
                r"^(?:static\s+|inline\s+|extern\s+|const\s+|unsigned\s+|signed\s+|struct\s+)*[A-Za-z_][A-Za-z0-9_:<>]*[\s*&]+(?:[A-Za-z_][A-Za-z0-9_]*::)*{ID}\s*\([^;]*$"
            ),
            SymbolKind::Function,
        ),
    ]
});

static RUBY: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        method(r"^\s*def\s+(?:self\.)?([A-Za-z_][A-Za-z0-9_]*[?!=]?)"),
        rule(&format!(r"^\s*class\s+{ID}"), SymbolKind::Class),
        rule(&format!(r"^\s*module\s+{ID}"), SymbolKind::Module),
    ]
});

static PHP: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        method(&format!(
            r"^\s*(?:(?:public|private|protected|static|final|abstract)\s+)*function\s+&?{ID}"
        )),
        rule(
            &format!(r"^\s*(?:abstract\s+|final\s+)?class\s+{ID}"),
            SymbolKind::Class,
        ),
        rule(&format!(r"^\s*interface\s+{ID}"), SymbolKind::Interface),
        rule(&format!(r"^\s*trait\s+{ID}"), SymbolKind::Trait),
    ]
});

static SWIFT: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let mods = r"(?:(?:public|private|fileprivate|internal|open|static|final|override|mutating|class)\s+)*";
    vec![
        method(&format!(r"^\s*{mods}func\s+{ID}")),
        rule(&format!(r"^\s*{mods}class\s+{ID}"), SymbolKind::Class),
        rule(&format!(r"^\s*{mods}struct\s+{ID}"), SymbolKind::Struct),
        rule(&format!(r"^\s*{mods}enum\s+{ID}"), SymbolKind::Enum),
        rule(
            &format!(r"^\s*{mods}protocol\s+{ID}"),
            SymbolKind::Interface,
        ),
        scope(&format!(r"^\s*{mods}extension\s+{ID}"), SymbolKind::Class),
    ]
});

static SHELL: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(
            r"^\s*function\s+([A-Za-z_][A-Za-z0-9_:.-]*)",
            SymbolKind::Function,
        ),
        rule(
            r"^\s*([A-Za-z_][A-Za-z0-9_:.-]*)\s*\(\)\s*\{?",
            SymbolKind::Function,
        ),
    ]
});

/// Words that look like names in the method rules but are statements.
const KEYWORDS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "return", "match", "else", "do", "try", "new",
    "throw", "sizeof", "case", "when", "elif", "until", "foreach", "using", "lock", "fixed",
];

fn rules_for(language: Language) -> Option<&'static [Rule]> {
    Some(match language {
        Language::Rust => &RUST,
        Language::Go => &GO,
        Language::Python => &PYTHON,
        Language::JavaScript | Language::TypeScript => &JS,
        Language::Java | Language::CSharp | Language::Kotlin | Language::Scala => &JAVA_LIKE,
        Language::C | Language::Cpp | Language::ObjectiveC => &C_LIKE,
        Language::Ruby => &RUBY,
        Language::Php => &PHP,
        Language::Swift => &SWIFT,
        Language::Shell => &SHELL,
        _ => return None,
    })
}

/// True when the extractor knows the language.
pub fn supports(language: Language) -> bool {
    rules_for(language).is_some()
}

fn indent_of(line: &[u8]) -> usize {
    line.iter()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .map(|b| if *b == b'\t' { 4 } else { 1 })
        .sum()
}

/// Extracts symbol definitions, in file order. Deterministic.
pub fn extract(content: &[u8], language: Language) -> Vec<Symbol> {
    let Some(rules) = rules_for(language) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // (indent, name) of open containers.
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (idx, raw) in content.split(|&b| b == b'\n').enumerate() {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let indent = indent_of(line);
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        for r in rules {
            let Some(caps) = r.re.captures(line) else {
                continue;
            };
            let Some(m) = caps.get(1) else { continue };
            let Ok(name) = std::str::from_utf8(m.as_bytes()) else {
                continue;
            };
            if KEYWORDS.contains(&name) {
                continue;
            }
            let container = stack.last().map(|(_, n)| n.clone());
            if !r.scope_only {
                let kind = match (r.in_container, &container) {
                    (Some(k), Some(_)) => k,
                    _ => r.kind,
                };
                out.push(Symbol {
                    name: name.to_string(),
                    kind,
                    line: idx as u32 + 1,
                    column: m.start() as u32,
                    end_column: m.end() as u32,
                    container,
                    signature: String::from_utf8_lossy(line).trim().to_string(),
                });
            }
            if r.scope_only || r.kind.is_container() {
                stack.push((indent, name.to_string()));
            }
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str, l: Language) -> Vec<(String, SymbolKind, Option<String>)> {
        extract(src.as_bytes(), l)
            .into_iter()
            .map(|s| (s.name, s.kind, s.container))
            .collect()
    }

    #[test]
    fn rust() {
        let src = "pub struct Invoice {\n    total: u64,\n}\n\nimpl Invoice {\n    pub fn total(&self) -> u64 {\n        self.total\n    }\n}\n\npub(crate) async fn charge_card(x: u32) {}\nmacro_rules! m { () => {} }\nconst LIMIT: u32 = 3;\ntrait Pay {}\n";
        let got = names(src, Language::Rust);
        assert_eq!(
            got,
            vec![
                ("Invoice".into(), SymbolKind::Struct, None),
                ("total".into(), SymbolKind::Method, Some("Invoice".into())),
                ("charge_card".into(), SymbolKind::Function, None),
                ("m".into(), SymbolKind::Macro, None),
                ("LIMIT".into(), SymbolKind::Constant, None),
                ("Pay".into(), SymbolKind::Trait, None),
            ]
        );
        let s = &extract(src.as_bytes(), Language::Rust)[1];
        assert_eq!((s.line, s.column, s.end_column), (6, 11, 16));
        assert_eq!(s.qualified_name(Language::Rust), "Invoice::total");
        assert_eq!(s.signature, "pub fn total(&self) -> u64 {");
    }

    #[test]
    fn python_go_js() {
        let py = "class Billing:\n    def charge(self):\n        pass\n\ndef helper():\n    return 1\nMAX_RETRIES = 3\n";
        assert_eq!(
            names(py, Language::Python),
            vec![
                ("Billing".into(), SymbolKind::Class, None),
                ("charge".into(), SymbolKind::Method, Some("Billing".into())),
                ("helper".into(), SymbolKind::Function, None),
                ("MAX_RETRIES".into(), SymbolKind::Constant, None),
            ]
        );
        let go = "package x\nfunc (s *Server) Serve() {}\nfunc main() {}\ntype Config struct {}\ntype Doer interface {}\n";
        assert_eq!(
            names(go, Language::Go),
            vec![
                ("Serve".into(), SymbolKind::Method, None),
                ("main".into(), SymbolKind::Function, None),
                ("Config".into(), SymbolKind::Struct, None),
                ("Doer".into(), SymbolKind::Interface, None),
            ]
        );
        let js = "export function chargeCard(a) {}\nconst refund = async (x) => x;\nclass Cart {\n  addItem(item) {\n    if (item) {\n    }\n  }\n}\n";
        assert_eq!(
            names(js, Language::JavaScript),
            vec![
                ("chargeCard".into(), SymbolKind::Function, None),
                ("refund".into(), SymbolKind::Function, None),
                ("Cart".into(), SymbolKind::Class, None),
                ("addItem".into(), SymbolKind::Method, Some("Cart".into())),
            ]
        );
    }

    #[test]
    fn java_c_ruby_shell() {
        let java =
            "public class Foo {\n    public static int bar(int x) {\n        return x;\n    }\n}\n";
        assert_eq!(
            names(java, Language::Java),
            vec![
                ("Foo".into(), SymbolKind::Class, None),
                ("bar".into(), SymbolKind::Method, Some("Foo".into())),
            ]
        );
        let c = "#define MAX 3\nstruct point {\n  int x;\n};\nstatic int add(int a, int b) {\n  return a + b;\n}\nint decl(void);\n";
        assert_eq!(
            names(c, Language::C),
            vec![
                ("MAX".into(), SymbolKind::Macro, None),
                ("point".into(), SymbolKind::Struct, None),
                ("add".into(), SymbolKind::Function, None),
            ]
        );
        let rb = "module Pay\n  class Card\n    def charge!\n    end\n  end\nend\n";
        assert_eq!(
            names(rb, Language::Ruby),
            vec![
                ("Pay".into(), SymbolKind::Module, None),
                ("Card".into(), SymbolKind::Class, Some("Pay".into())),
                ("charge!".into(), SymbolKind::Method, Some("Card".into())),
            ]
        );
        assert_eq!(
            names("deploy() {\n  echo\n}\n", Language::Shell),
            vec![("deploy".into(), SymbolKind::Function, None)]
        );
        assert!(extract(b"anything", Language::Markdown).is_empty());
        assert!(supports(Language::Rust));
        assert!(!supports(Language::Json));
        assert_eq!(SymbolKind::parse("Type_Alias"), Some(SymbolKind::TypeAlias));
    }
}
