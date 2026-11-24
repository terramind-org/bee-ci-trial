//! Language-aware comment and whitespace normalization.
//!
//! [`normalize`] turns source text into a canonical form in which comments are gone and
//! whitespace is reduced to what can change meaning. Two texts with the same canonical form
//! differ only in comments and whitespace. String and character literals (including raw,
//! triple-quoted, template and heredoc forms) are copied verbatim, so a comment marker inside a
//! literal is never mistaken for a comment and a change inside a literal is never ignored.

/// A language whose comment syntax the classifier understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    /// Rust: `//`, nested `/* */`, raw strings, char literals versus lifetimes.
    Rust,
    /// C and C++: `//`, `/* */`, C++ raw strings.
    C,
    /// Go: `//`, `/* */`, raw backtick strings.
    Go,
    /// Java: `//`, `/* */`, text blocks.
    Java,
    /// JavaScript and TypeScript: `//`, `/* */`, template literals, regex literals.
    JavaScript,
    /// Python: `#`, triple-quoted strings. Indentation is kept.
    Python,
    /// Ruby: `#`, `=begin`/`=end`, heredocs, percent literals, regex literals.
    Ruby,
    /// POSIX-style shell: `#` at a word start, heredocs, `$(...)` nesting.
    Shell,
    /// YAML: `#` after whitespace, block scalars. Indentation is kept.
    Yaml,
    /// TOML: `#`, triple-quoted strings.
    Toml,
    /// CSS: `/* */` only.
    Css,
    /// SQL: `--`, `/* */`, dollar quoting.
    Sql,
}

impl Language {
    /// Picks the language from a repository path's file name or extension, case-insensitively.
    /// Returns `None` for anything else, including Markdown (a docs path concern, not a
    /// comment-syntax one).
    pub fn detect(path: &str) -> Option<Language> {
        let name = path.rsplit('/').next().unwrap_or(path);
        match name {
            "Gemfile" | "Rakefile" => return Some(Language::Ruby),
            _ => {}
        }
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::C,
            "go" => Language::Go,
            "java" => Language::Java,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Language::JavaScript,
            "py" | "pyi" => Language::Python,
            "rb" | "rake" | "gemspec" => Language::Ruby,
            "sh" | "bash" | "zsh" => Language::Shell,
            "yaml" | "yml" => Language::Yaml,
            "toml" => Language::Toml,
            "css" => Language::Css,
            "sql" => Language::Sql,
            _ => return None,
        })
    }

    fn keeps_indent(self) -> bool {
        matches!(self, Language::Python | Language::Yaml)
    }
}

/// The canonical form of `text`: comments removed, blank lines dropped, trailing whitespace
/// dropped, and whitespace inside a line reduced to a single space only where its removal could
/// fuse two identifier characters or two operator characters. Leading indentation is dropped
/// except for Python and YAML, where it is kept exactly.
pub fn normalize(lang: Language, text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let chars = match chars.first() {
        Some('\u{feff}') => &chars[1..],
        _ => &chars[..],
    };
    Scan {
        lang,
        s: chars,
        i: 0,
        out: Out::new(lang.keeps_indent()),
        heredocs: Vec::new(),
    }
    .run()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_op(c: char) -> bool {
    "+-*/%&|^<>=!~?:.@\\".contains(c)
}

struct Out {
    s: String,
    line_content: bool,
    indent: String,
    pending_space: bool,
    keep_indent: bool,
}

impl Out {
    fn new(keep_indent: bool) -> Out {
        Out {
            s: String::new(),
            line_content: false,
            indent: String::new(),
            pending_space: false,
            keep_indent,
        }
    }

    fn ws(&mut self, c: char) {
        if c == '\r' {
            if self.line_content {
                self.pending_space = true;
            }
        } else if self.line_content {
            self.pending_space = true;
        } else {
            self.indent.push(c);
        }
    }

    fn newline(&mut self) {
        if self.line_content {
            self.s.push('\n');
            self.line_content = false;
        }
        self.indent.clear();
        self.pending_space = false;
    }

    fn separator(&mut self) {
        if self.line_content {
            self.pending_space = true;
        }
    }

    fn start_token(&mut self, first: char) {
        if !self.line_content {
            if self.keep_indent {
                self.s.push_str(&self.indent);
            }
            self.indent.clear();
            self.line_content = true;
        } else if self.pending_space
            && let Some(last) = self.s.chars().next_back()
            && ((is_ident(last) && is_ident(first)) || (is_op(last) && is_op(first)))
        {
            self.s.push(' ');
        }
        self.pending_space = false;
    }

    fn code(&mut self, c: char) {
        self.start_token(c);
        self.s.push(c);
    }

    fn verbatim(&mut self, text: &str) {
        let Some(first) = text.chars().next() else {
            return;
        };
        self.start_token(first);
        self.s.push_str(text);
        if text.ends_with('\n') {
            self.line_content = false;
            self.indent.clear();
        }
    }

    /// The last character written, `None` at the start of output.
    fn last(&self) -> Option<char> {
        self.s.chars().next_back()
    }

    /// The identifier-like word that ends the output, if any.
    fn tail_word(&self) -> &str {
        let start = self
            .s
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_ident(*c))
            .last()
            .map_or(self.s.len(), |(i, _)| i);
        &self.s[start..]
    }
}

struct Heredoc {
    word: String,
    strip_leading: bool,
}

struct Scan<'a> {
    lang: Language,
    s: &'a [char],
    i: usize,
    out: Out,
    heredocs: Vec<Heredoc>,
}

impl Scan<'_> {
    fn ch(&self, k: usize) -> Option<char> {
        self.s.get(k).copied()
    }

    fn at(&self, k: usize, pat: &str) -> bool {
        pat.chars()
            .enumerate()
            .all(|(d, p)| self.ch(k + d) == Some(p))
    }

    fn at_line_start(&self, k: usize) -> bool {
        k == 0 || self.s[k - 1] == '\n'
    }

    fn line_end(&self, from: usize) -> usize {
        (from..self.s.len())
            .find(|&k| self.s[k] == '\n')
            .unwrap_or(self.s.len())
    }

    fn text(&self, a: usize, b: usize) -> String {
        self.s[a..b].iter().collect()
    }

    fn run(mut self) -> String {
        let n = self.s.len();
        while self.i < n {
            let c = self.s[self.i];
            if c == '\n' {
                self.newline_event();
                continue;
            }
            if c.is_whitespace() {
                self.out.ws(c);
                self.i += 1;
                continue;
            }
            if self.lang == Language::Ruby
                && self.at_line_start(self.i)
                && self.at(self.i, "__END__")
                && self.line_end(self.i) == self.i + 7
            {
                let rest = self.text(self.i, n);
                self.out.verbatim(&rest);
                break;
            }
            if let Some((end, block)) = self.comment_at() {
                if block {
                    self.out.separator();
                }
                self.i = end;
                continue;
            }
            if let Some(end) = self.string_at(self.i).or_else(|| self.special_at()) {
                let text = self.text(self.i, end);
                self.out.verbatim(&text);
                self.i = end;
                continue;
            }
            if matches!(self.lang, Language::Ruby | Language::Shell)
                && let Some((end, hd)) = self.heredoc_marker()
            {
                for k in self.i..end {
                    let ch = self.s[k];
                    self.out.code(ch);
                }
                self.heredocs.push(hd);
                self.i = end;
                continue;
            }
            if self.lang == Language::Shell
                && c == '\\'
                && self.ch(self.i + 1).is_some_and(|d| d != '\n')
            {
                let d = self.s[self.i + 1];
                self.out.code(c);
                self.out.code(d);
                self.i += 2;
                continue;
            }
            self.out.code(c);
            self.i += 1;
        }
        self.out.newline();
        self.out.s
    }

    /// Handles a newline in code: heredoc bodies and YAML block scalars follow it verbatim.
    fn newline_event(&mut self) {
        let n = self.s.len();
        let block_scalar = self.lang == Language::Yaml && self.yaml_block_scalar_pending();
        let line_start = (0..self.i)
            .rev()
            .find(|&k| self.s[k] == '\n')
            .map_or(0, |k| k + 1);
        let parent_indent = self.s[line_start..self.i]
            .iter()
            .take_while(|c| **c == ' ')
            .count();
        self.out.newline();
        self.i += 1;
        for hd in std::mem::take(&mut self.heredocs) {
            while self.i < n {
                let end = self.line_end(self.i);
                let line = self.text(self.i, end);
                let probe = line.trim_end_matches('\r');
                let probe = if hd.strip_leading {
                    probe.trim_start_matches([' ', '\t'])
                } else {
                    probe
                };
                let done = probe == hd.word;
                let upto = (end + 1).min(n);
                let chunk = self.text(self.i, upto);
                self.out.verbatim(&chunk);
                if end == n && !chunk.ends_with('\n') {
                    self.out.newline();
                }
                self.i = upto;
                if done {
                    break;
                }
            }
        }
        if block_scalar {
            while self.i < n {
                let end = self.line_end(self.i);
                let line = &self.s[self.i..end];
                let blank = line.iter().all(|c| c.is_whitespace());
                let indent = line.iter().take_while(|c| **c == ' ').count();
                if !blank && indent <= parent_indent {
                    break;
                }
                let upto = (end + 1).min(n);
                let chunk = self.text(self.i, upto);
                self.out.verbatim(&chunk);
                self.i = upto;
            }
            self.out.newline();
        }
    }

    fn yaml_block_scalar_pending(&self) -> bool {
        if !self.out.line_content {
            return false;
        }
        let line_start = self.out.s.rfind('\n').map_or(0, |k| k + 1);
        let line = self.out.s[line_start..].trim_end();
        let core = line.trim_end_matches(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
        if !(core.ends_with('|') || core.ends_with('>')) {
            return false;
        }
        let before = &core[..core.len() - 1];
        before.is_empty() || before.ends_with(' ')
    }

    // ---- comments ----

    fn hash_ok(&self) -> bool {
        let i = self.i;
        if i == 0 && self.ch(1) == Some('!') {
            return false;
        }
        let prev = if i == 0 { None } else { Some(self.s[i - 1]) };
        match self.lang {
            Language::Python | Language::Toml => true,
            Language::Ruby => !matches!(prev, Some('$' | '?')),
            Language::Shell => {
                prev.is_none_or(|p| p.is_whitespace() || matches!(p, ';' | '&' | '|' | '('))
            }
            Language::Yaml => prev.is_none_or(char::is_whitespace),
            _ => false,
        }
    }

    fn comment_at(&self) -> Option<(usize, bool)> {
        let i = self.i;
        let n = self.s.len();
        match self.lang {
            Language::Rust | Language::C | Language::Go | Language::Java | Language::JavaScript => {
                if self.at(i, "//") {
                    return Some((self.line_end(i), false));
                }
                if self.at(i, "/*") {
                    return Some((self.block_end(i + 2, self.lang == Language::Rust), true));
                }
            }
            Language::Css => {
                if self.at(i, "/*") {
                    return Some((self.block_end(i + 2, false), true));
                }
            }
            Language::Sql => {
                if self.at(i, "--") {
                    return Some((self.line_end(i), false));
                }
                if self.at(i, "/*") {
                    return Some((self.block_end(i + 2, false), true));
                }
            }
            Language::Ruby if self.at_line_start(i) && self.at(i, "=begin") => {
                let mut ls = self.line_end(i) + 1;
                while ls < n {
                    if self.at(ls, "=end") {
                        return Some((self.line_end(ls), true));
                    }
                    ls = self.line_end(ls) + 1;
                }
                return Some((n, true));
            }
            _ => {}
        }
        if self.s[i] == '#'
            && matches!(
                self.lang,
                Language::Python
                    | Language::Ruby
                    | Language::Shell
                    | Language::Yaml
                    | Language::Toml
            )
            && self.hash_ok()
        {
            return Some((self.line_end(i), false));
        }
        None
    }

    fn block_end(&self, from: usize, nested: bool) -> usize {
        let n = self.s.len();
        let mut k = from;
        let mut depth = 1usize;
        while k < n {
            if self.at(k, "*/") {
                depth -= 1;
                k += 2;
                if depth == 0 {
                    return k;
                }
            } else if nested && self.at(k, "/*") {
                depth += 1;
                k += 2;
            } else {
                k += 1;
            }
        }
        n
    }

    // ---- strings ----

    fn quoted(&self, start: usize, q: char, esc: bool) -> usize {
        let n = self.s.len();
        let mut k = start + 1;
        while k < n {
            let c = self.s[k];
            if esc && c == '\\' {
                k += 2;
            } else if c == q {
                return k + 1;
            } else {
                k += 1;
            }
        }
        n
    }

    /// A quoted string whose body may contain nested code (`${..}`, `#{..}`, `$(..)`).
    fn quoted_nested(
        &self,
        start: usize,
        q: char,
        open: &str,
        open_ch: char,
        close_ch: char,
    ) -> usize {
        let n = self.s.len();
        let mut k = start + 1;
        while k < n {
            let c = self.s[k];
            if c == '\\' {
                k += 2;
            } else if c == q {
                return k + 1;
            } else if self.at(k, open) {
                k = self.nested(k + open.chars().count(), open_ch, close_ch);
            } else {
                k += 1;
            }
        }
        n
    }

    fn nested(&self, from: usize, open_ch: char, close_ch: char) -> usize {
        let n = self.s.len();
        let mut k = from;
        let mut depth = 1usize;
        while k < n {
            if let Some(e) = self.string_at(k) {
                k = e;
                continue;
            }
            let c = self.s[k];
            if c == open_ch {
                depth += 1;
            } else if c == close_ch {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            k += 1;
        }
        n
    }

    fn triple(&self, start: usize, q: char, esc: bool) -> usize {
        let n = self.s.len();
        let mut k = start + 3;
        while k < n {
            if esc && self.s[k] == '\\' {
                k += 2;
            } else if self.s[k] == q && self.at(k, &q.to_string().repeat(3)) {
                return k + 3;
            } else {
                k += 1;
            }
        }
        n
    }

    fn prev(&self, k: usize) -> Option<char> {
        if k == 0 { None } else { Some(self.s[k - 1]) }
    }

    /// A string or character literal starting at `i`, as the end index (exclusive).
    fn string_at(&self, i: usize) -> Option<usize> {
        let c = *self.s.get(i)?;
        let q3 = |q: char| self.at(i, &q.to_string().repeat(3));
        match self.lang {
            Language::Rust => match c {
                '"' => Some(self.quoted(i, '"', true)),
                'r' => self.rust_raw(i),
                '\'' => self.rust_char(i),
                _ => None,
            },
            Language::C => match c {
                '"' => Some(self.c_raw(i).unwrap_or_else(|| self.quoted(i, '"', true))),
                '\'' => {
                    let separator = self.prev(i).is_some_and(|p| p.is_ascii_hexdigit())
                        && self.ch(i + 1).is_some_and(|d| d.is_ascii_hexdigit())
                        && self.ch(i + 2) != Some('\'');
                    if separator {
                        None
                    } else {
                        Some(self.quoted(i, '\'', true))
                    }
                }
                _ => None,
            },
            Language::Go => match c {
                '"' | '\'' => Some(self.quoted(i, c, true)),
                '`' => Some(self.quoted(i, '`', false)),
                _ => None,
            },
            Language::Java => match c {
                '"' if q3('"') => Some(self.triple(i, '"', true)),
                '"' | '\'' => Some(self.quoted(i, c, true)),
                _ => None,
            },
            Language::JavaScript => match c {
                '"' | '\'' => Some(self.quoted(i, c, true)),
                '`' => Some(self.quoted_nested(i, '`', "${", '{', '}')),
                _ => None,
            },
            Language::Python => match c {
                '"' | '\'' if q3(c) => Some(self.triple(i, c, true)),
                '"' | '\'' => Some(self.quoted(i, c, true)),
                _ => None,
            },
            Language::Ruby => match c {
                '"' | '`' => Some(self.quoted_nested(i, c, "#{", '{', '}')),
                '\'' => Some(self.quoted(i, '\'', true)),
                _ => None,
            },
            Language::Shell => match c {
                '\'' => {
                    let ansi_c = self.prev(i) == Some('$');
                    Some(self.quoted(i, '\'', ansi_c))
                }
                '"' => Some(self.quoted_nested(i, '"', "$(", '(', ')')),
                '`' => Some(self.quoted(i, '`', true)),
                _ => None,
            },
            Language::Yaml => match c {
                '"' | '\'' if self.yaml_quote_starts(i) => Some(self.quoted(i, c, c == '"')),
                _ => None,
            },
            Language::Toml => match c {
                '"' | '\'' if q3(c) => Some(self.triple(i, c, c == '"')),
                '"' | '\'' => Some(self.quoted(i, c, c == '"')),
                _ => None,
            },
            Language::Css => match c {
                '"' | '\'' => Some(self.quoted(i, c, true)),
                _ => None,
            },
            Language::Sql => match c {
                '\'' | '"' | '`' => Some(self.quoted(i, c, false)),
                '$' => self.sql_dollar(i),
                _ => None,
            },
        }
    }

    fn rust_raw(&self, i: usize) -> Option<usize> {
        let prefix_ok = match self.prev(i) {
            None => true,
            Some(p) if !is_ident(p) => true,
            Some('b' | 'c') => i < 2 || !is_ident(self.s[i - 2]),
            Some(_) => false,
        };
        if !prefix_ok {
            return None;
        }
        let mut k = i + 1;
        while self.ch(k) == Some('#') {
            k += 1;
        }
        if self.ch(k) != Some('"') {
            return None;
        }
        let hashes = k - i - 1;
        let closing = format!("\"{}", "#".repeat(hashes));
        let n = self.s.len();
        let mut j = k + 1;
        while j < n {
            if self.at(j, &closing) {
                return Some(j + closing.chars().count());
            }
            j += 1;
        }
        Some(n)
    }

    fn rust_char(&self, i: usize) -> Option<usize> {
        let n = self.s.len();
        if self.ch(i + 1) == Some('\\') {
            let limit = (i + 12).min(n);
            return (i + 3..limit).find(|&k| self.s[k] == '\'').map(|k| k + 1);
        }
        if self.ch(i + 2) == Some('\'') && self.ch(i + 1) != Some('\'') {
            return Some(i + 3);
        }
        None
    }

    fn c_raw(&self, i: usize) -> Option<usize> {
        if self.prev(i) != Some('R') {
            return None;
        }
        if i >= 2 {
            let before = self.s[i - 2];
            if is_ident(before) && !matches!(before, 'L' | 'u' | 'U' | '8') {
                return None;
            }
        }
        let mut k = i + 1;
        while k < self.s.len() && k - i <= 17 && !matches!(self.s[k], '(' | ')' | '"' | '\\' | ' ')
        {
            k += 1;
        }
        if self.ch(k) != Some('(') {
            return None;
        }
        let delim = self.text(i + 1, k);
        let closing = format!("){delim}\"");
        let n = self.s.len();
        let mut j = k + 1;
        while j < n {
            if self.at(j, &closing) {
                return Some(j + closing.chars().count());
            }
            j += 1;
        }
        Some(n)
    }

    fn yaml_quote_starts(&self, i: usize) -> bool {
        let mut k = i;
        while k > 0 {
            let p = self.s[k - 1];
            if p == ' ' || p == '\t' {
                k -= 1;
                continue;
            }
            return matches!(p, '\n' | ':' | '-' | ',' | '[' | '{' | '?');
        }
        true
    }

    fn sql_dollar(&self, i: usize) -> Option<usize> {
        if self.prev(i).is_some_and(is_ident) {
            return None;
        }
        let mut k = i + 1;
        while self
            .ch(k)
            .is_some_and(|c| c.is_alphabetic() || c == '_' || (k > i + 1 && c.is_ascii_digit()))
        {
            k += 1;
        }
        if self.ch(k) != Some('$') {
            return None;
        }
        let tag = self.text(i, k + 1);
        let n = self.s.len();
        let mut j = k + 1;
        while j < n {
            if self.at(j, &tag) {
                return Some(j + tag.chars().count());
            }
            j += 1;
        }
        Some(n)
    }

    // ---- regex and percent literals, heredocs ----

    fn value_may_start(&self) -> bool {
        match self.out.last() {
            None | Some('\n') => true,
            Some(c) if is_ident(c) => {
                let kws: &[&str] = match self.lang {
                    Language::Ruby => &[
                        "if", "unless", "while", "until", "and", "or", "not", "return", "when",
                        "case", "then", "elsif", "puts", "p",
                    ],
                    _ => &[
                        "return",
                        "typeof",
                        "instanceof",
                        "in",
                        "of",
                        "new",
                        "delete",
                        "void",
                        "throw",
                        "case",
                        "do",
                        "else",
                        "yield",
                        "await",
                    ],
                };
                kws.contains(&self.out.tail_word())
            }
            Some(')' | ']' | '}') => false,
            Some(_) => true,
        }
    }

    /// Regex literals (JavaScript, Ruby) and Ruby percent literals.
    fn special_at(&self) -> Option<usize> {
        let i = self.i;
        let c = self.s[i];
        if !matches!(self.lang, Language::JavaScript | Language::Ruby) || !self.value_may_start() {
            return None;
        }
        if c == '/' {
            let n = self.s.len();
            let mut k = i + 1;
            let mut in_class = false;
            while k < n {
                match self.s[k] {
                    '\n' => return None,
                    '\\' => k += 1,
                    '[' => in_class = true,
                    ']' => in_class = false,
                    '/' if !in_class => return Some(k + 1),
                    _ => {}
                }
                k += 1;
            }
            return None;
        }
        if c == '%' && self.lang == Language::Ruby {
            let mut k = i + 1;
            if self.ch(k).is_some_and(|l| "qQwWiIrsx".contains(l)) {
                k += 1;
            }
            let open = self.ch(k)?;
            if open.is_alphanumeric() || open.is_whitespace() {
                return None;
            }
            if k == i + 1 && !"([{<|!/^".contains(open) {
                return None;
            }
            let close = match open {
                '(' => ')',
                '[' => ']',
                '{' => '}',
                '<' => '>',
                o => o,
            };
            let n = self.s.len();
            let mut j = k + 1;
            let mut depth = 1usize;
            while j < n {
                let d = self.s[j];
                if d == '\\' {
                    j += 2;
                    continue;
                }
                if d == close && open != close {
                    depth -= 1;
                } else if d == open && open != close {
                    depth += 1;
                } else if d == close {
                    depth = 0;
                }
                j += 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            return Some(n);
        }
        None
    }

    fn heredoc_marker(&self) -> Option<(usize, Heredoc)> {
        let i = self.i;
        if !self.at(i, "<<") || self.ch(i + 2) == Some('<') {
            return None;
        }
        let mut k = i + 2;
        let mut strip_leading = false;
        let mut flagged = false;
        if let Some(f) = self.ch(k)
            && (f == '-' || (f == '~' && self.lang == Language::Ruby))
        {
            strip_leading = true;
            flagged = true;
            k += 1;
        }
        let mut spaced = false;
        if self.lang == Language::Shell {
            while matches!(self.ch(k), Some(' ' | '\t')) {
                spaced = true;
                k += 1;
            }
        }
        let quote = self.ch(k).filter(|q| matches!(q, '\'' | '"' | '`'));
        if quote.is_some() {
            k += 1;
        }
        let first = self.ch(k)?;
        if !(first.is_alphabetic() || first == '_') {
            return None;
        }
        let start = k;
        while self.ch(k).is_some_and(|c| c.is_alphanumeric() || c == '_') {
            k += 1;
        }
        let word = self.text(start, k);
        if let Some(q) = quote {
            if self.ch(k) != Some(q) {
                return None;
            }
            k += 1;
        } else {
            let upper = word
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
            match self.lang {
                Language::Ruby if !flagged && !upper => return None,
                Language::Shell if spaced && !upper => return None,
                _ => {}
            }
        }
        Some((
            k,
            Heredoc {
                word,
                strip_leading,
            },
        ))
    }
}
