//! Behaviour of the semantic diff classifier (SPEC §11.3).

use bee_classify::{
    Basis, CLASSIFIER_VERSION, Class, ClassRules, Content, GlobSet, Language, PathChange,
    classify_change, classify_path, normalize,
};
use bee_core::{Mode, RepoPath};
use proptest::prelude::*;

fn rp(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

/// A modification of `path` from `old` to `new` (both text).
fn modify(path: &str, old: &str, new: &str) -> PathChange {
    PathChange {
        path: rp(path),
        old: Some(Content::file(old)),
        new: Some(Content::file(new)),
        renamed_from: None,
    }
}

fn add(path: &str, new: &str) -> PathChange {
    PathChange {
        path: rp(path),
        old: None,
        new: Some(Content::file(new)),
        renamed_from: None,
    }
}

fn delete(path: &str, old: &str) -> PathChange {
    PathChange {
        path: rp(path),
        old: Some(Content::file(old)),
        new: None,
        renamed_from: None,
    }
}

fn rename(from: &str, to: &str, old: &str, new: &str) -> PathChange {
    PathChange {
        path: rp(to),
        old: Some(Content::file(old)),
        new: Some(Content::file(new)),
        renamed_from: Some(rp(from)),
    }
}

fn policy_rules() -> ClassRules {
    ClassRules::with_policy(
        ["**/*.snap", "gen", "Cargo.lock"],
        [
            ".github/workflows",
            ".beehive/policy.toml",
            "src/auth",
            "**/payments",
        ],
    )
    .unwrap()
}

fn class_of(change: PathChange) -> Class {
    classify_path(change, &policy_rules()).class
}

fn same(lang: Language, a: &str, b: &str) {
    assert_eq!(
        normalize(lang, a),
        normalize(lang, b),
        "{lang:?}: expected equal\n--- a\n{a}\n--- b\n{b}"
    );
}

fn differs(lang: Language, a: &str, b: &str) {
    assert_ne!(
        normalize(lang, a),
        normalize(lang, b),
        "{lang:?}: expected different\n--- a\n{a}\n--- b\n{b}"
    );
}

// ---- classes ----

#[test]
fn version_is_pinned() {
    assert_eq!(CLASSIFIER_VERSION, "classify-v1");
}

#[test]
fn classes_are_ordered_by_risk_and_round_trip() {
    let names: Vec<&str> = Class::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(
        names,
        [
            "generated",
            "docs",
            "comment_only",
            "test_only",
            "rename_only",
            "dependency",
            "logic",
            "sensitive"
        ]
    );
    assert!(Class::ALL.windows(2).all(|w| w[0] < w[1]));
    for c in Class::ALL {
        assert_eq!(Class::parse(c.as_str()), Some(c));
    }
    assert_eq!(Class::parse("nope"), None);
}

#[test]
fn generated_comes_from_rules() {
    let r = classify_path(modify("snapshots/a.snap", "x", "y"), &policy_rules());
    assert_eq!((r.class, r.basis), (Class::Generated, Basis::GeneratedPath));
    assert_eq!(
        class_of(modify("gen/api/client.rs", "fn a(){}", "fn b(){}")),
        Class::Generated
    );
    assert_eq!(class_of(modify("Cargo.lock", "a", "b")), Class::Generated);
    // Without policy the same path is ordinary logic.
    let plain = classify_path(
        modify("gen/api/client.rs", "fn a(){}", "fn b(){}"),
        &ClassRules::default(),
    );
    assert_eq!(plain.class, Class::Logic);
}

#[test]
fn docs_paths_are_docs() {
    for p in [
        "README.md",
        "docs/guide/intro.html",
        "crates/x/docs/a.rs",
        "notes.txt",
        "LICENSE",
        "LICENSE-APACHE",
        "guide.rst",
        "CHANGELOG",
    ] {
        assert_eq!(
            class_of(modify(p, "old text", "new text")),
            Class::Docs,
            "{p}"
        );
    }
    assert_eq!(class_of(add("docs/new.md", "# hi")), Class::Docs);
    assert_eq!(class_of(delete("docs/old.md", "# bye")), Class::Docs);
}

#[test]
fn binary_docs_asset_is_docs_but_binary_elsewhere_is_logic() {
    let bin = |p: &str| PathChange {
        path: rp(p),
        old: Some(Content::file(vec![0u8, 1, 2])),
        new: Some(Content::file(vec![0u8, 9, 2])),
        renamed_from: None,
    };
    assert_eq!(class_of(bin("docs/logo.png")), Class::Docs);
    assert_eq!(class_of(bin("assets/logo.png")), Class::Logic);
    assert_eq!(class_of(bin("tests/blob.bin")), Class::Logic);
    assert_eq!(class_of(bin("gen/blob.bin")), Class::Generated);
    assert_eq!(class_of(bin("src/auth/key.bin")), Class::Sensitive);
}

#[test]
fn invalid_utf8_counts_as_binary() {
    let c = PathChange {
        path: rp("src/a.rs"),
        old: Some(Content::file(vec![0xff, 0xfe, b'a'])),
        new: Some(Content::file(vec![0xff, 0xfe, b'a', b' '])),
        renamed_from: None,
    };
    assert_eq!(class_of(c), Class::Logic);
}

#[test]
fn comment_only_changes() {
    let r = classify_path(
        modify(
            "src/lib.rs",
            "fn a() {}\n",
            "// explain\nfn a() {} // trailing\n",
        ),
        &policy_rules(),
    );
    assert_eq!(
        (r.class, r.basis),
        (Class::CommentOnly, Basis::CommentsAndWhitespace)
    );
    assert_eq!(
        class_of(modify("src/lib.rs", "fn a() {}", "fn a()   {}\n\n\n")),
        Class::CommentOnly
    );
    assert_eq!(
        class_of(modify("a.py", "x = 1\n", "# why\nx = 1  # because\n")),
        Class::CommentOnly
    );
    assert_eq!(
        class_of(add("src/note.rs", "// only a comment\n")),
        Class::CommentOnly
    );
    assert_eq!(
        class_of(delete("src/note.rs", "/* gone */\n")),
        Class::CommentOnly
    );
}

#[test]
fn code_change_next_to_comment_is_logic() {
    assert_eq!(
        class_of(modify("src/lib.rs", "fn a() {}", "// note\nfn b() {}")),
        Class::Logic
    );
    assert_eq!(
        class_of(modify(
            "src/lib.rs",
            "let s = \"a // b\";",
            "let s = \"a // c\";"
        )),
        Class::Logic
    );
}

#[test]
fn unknown_language_whitespace_change_is_logic() {
    assert_eq!(
        class_of(modify("src/a.zig", "const a = 1;", "const a  =  1;")),
        Class::Logic
    );
    assert_eq!(
        class_of(modify("Makefile", "a:\n\tb", "a:\n\t b")),
        Class::Logic
    );
}

#[test]
fn test_only_paths() {
    for p in [
        "tests/it.rs",
        "crates/x/tests/it.rs",
        "pkg/a_test.go",
        "web/src/a.test.ts",
        "web/src/a.spec.tsx",
        "py/test_a.py",
        "py/a_test.py",
        "lib/a_spec.rb",
        "src/test/java/FooTest.java",
        "__tests__/a.js",
    ] {
        assert_eq!(
            class_of(modify(p, "fn a() { 1 }", "fn a() { 2 }")),
            Class::TestOnly,
            "{p}"
        );
    }
    // A comment-only edit of a test file is the lower class.
    assert_eq!(
        class_of(modify("tests/it.rs", "fn a() {}", "// c\nfn a() {}")),
        Class::CommentOnly
    );
}

#[test]
fn rename_only_is_content_identical_move() {
    let r = classify_path(
        rename("src/a.rs", "src/b.rs", "fn a() {}", "fn a() {}"),
        &policy_rules(),
    );
    assert_eq!(
        (r.class, r.basis),
        (Class::RenameOnly, Basis::IdenticalMove)
    );
    // Unknown language and binary content still count: bytes are identical.
    assert_eq!(
        class_of(rename("a.zig", "b.zig", "x", "x")),
        Class::RenameOnly
    );
    let bin = PathChange {
        path: rp("img/b.png"),
        old: Some(Content::file(vec![0u8, 1])),
        new: Some(Content::file(vec![0u8, 1])),
        renamed_from: Some(rp("img/a.png")),
    };
    assert_eq!(class_of(bin), Class::RenameOnly);
    // Move plus edit is logic; move plus comment edit stays rename-only (a move never drops below its own class).
    assert_eq!(
        class_of(rename("src/a.rs", "src/b.rs", "fn a() {}", "fn b() {}")),
        Class::Logic
    );
    assert_eq!(
        class_of(rename(
            "src/a.rs",
            "src/b.rs",
            "fn a() {}",
            "fn a() {} // moved"
        )),
        Class::RenameOnly
    );
    // A mode change is not a pure move.
    let mut exec = rename("a.zig", "b.zig", "x", "x");
    exec.new.as_mut().unwrap().mode = Mode::Executable;
    assert_eq!(class_of(exec), Class::Logic);
    // Moving into docs or tests still reports the move, not the destination.
    assert_eq!(
        class_of(rename("src/a.rs", "docs/a.rs", "x", "x")),
        Class::RenameOnly
    );
}

#[test]
fn rename_across_languages_is_not_comment_only() {
    assert_eq!(
        class_of(rename("a.py", "a.rb", "x = 1", "x = 1 # c")),
        Class::Logic
    );
}

#[test]
fn dependency_manifests_and_lockfiles() {
    for p in [
        "Cargo.toml",
        "crates/x/Cargo.toml",
        "package.json",
        "web/package-lock.json",
        "web/yarn.lock",
        "web/pnpm-lock.yaml",
        "go.mod",
        "go.sum",
        "requirements.txt",
        "requirements-dev.txt",
        "pyproject.toml",
        "poetry.lock",
        "Gemfile",
        "Gemfile.lock",
        "pom.xml",
        "build.gradle",
        "App/App.csproj",
    ] {
        assert_eq!(class_of(modify(p, "a 1", "a 2")), Class::Dependency, "{p}");
    }
    // A comment-only manifest edit is the lower class.
    assert_eq!(
        class_of(modify(
            "crates/x/Cargo.toml",
            "[package]\nname = \"x\"\n",
            "# hi\n[package]\nname = \"x\" # n\n"
        )),
        Class::CommentOnly
    );
    // requirements.txt is a manifest, not documentation.
    assert_eq!(
        class_of(modify("docs/requirements.txt", "a", "b")),
        Class::Dependency
    );
}

#[test]
fn logic_is_the_fallback() {
    let r = classify_path(
        modify("src/lib.rs", "fn a() {}", "fn b() {}"),
        &policy_rules(),
    );
    assert_eq!((r.class, r.basis), (Class::Logic, Basis::Fallback));
    // Mode flips and symlink retargets are behaviour.
    let mut flip = modify("scripts/run.sh", "echo 1", "echo 1");
    flip.new.as_mut().unwrap().mode = Mode::Executable;
    assert_eq!(class_of(flip), Class::Logic);
    let link = |t: &str| Content {
        bytes: t.as_bytes().to_vec(),
        mode: Mode::Symlink,
    };
    let retarget = PathChange {
        path: rp("lnk"),
        old: Some(link("a")),
        new: Some(link("b")),
        renamed_from: None,
    };
    assert_eq!(class_of(retarget), Class::Logic);
}

#[test]
fn unchanged_path_is_the_lowest_content_class() {
    let r = classify_path(modify("src/a.zig", "x", "x"), &policy_rules());
    assert_eq!((r.class, r.basis), (Class::CommentOnly, Basis::Unchanged));
}

#[test]
fn sensitive_overrides_everything() {
    let r = classify_path(modify("src/auth/token.rs", "// a", "// b"), &policy_rules());
    assert_eq!((r.class, r.basis), (Class::Sensitive, Basis::SensitivePath));
    assert_eq!(
        class_of(modify(".github/workflows/ci.yml", "a", "b")),
        Class::Sensitive
    );
    assert_eq!(
        class_of(modify("svc/payments/charge.go", "a", "b")),
        Class::Sensitive
    );
    assert_eq!(
        class_of(modify("src/auth/README.md", "a", "b")),
        Class::Sensitive
    );
    // Moving a file out of a sensitive directory is still sensitive.
    assert_eq!(
        class_of(rename("src/auth/a.rs", "src/b.rs", "x", "x")),
        Class::Sensitive
    );
    // Sensitive wins over generated.
    let both = ClassRules::with_policy(["src/auth"], ["src/auth"]).unwrap();
    assert_eq!(
        classify_path(modify("src/auth/a", "1", "2"), &both).class,
        Class::Sensitive
    );
}

#[test]
fn as_cr_02_docs_only_is_docs() {
    let change = [
        modify("README.md", "# a", "# b"),
        modify("docs/design/overview.md", "x", "y"),
        add("docs/img/diagram.png", "\u{0}png"),
        delete("docs/old.rst", "gone"),
    ];
    let got = classify_change(&change, &policy_rules());
    assert!(got.per_path.iter().all(|c| c.class == Class::Docs));
    assert_eq!(got.highest, Some(Class::Docs));
    // The land rule "agents may land docs | test_only | comment_only" admits it.
    assert!(got.highest.unwrap() <= Class::TestOnly);
}

#[test]
fn as_cr_02_sensitive_path_is_sensitive() {
    let change = [
        modify("README.md", "# a", "# b"),
        modify(".beehive/policy.toml", "# note\n", "# other note\n"),
    ];
    let got = classify_change(&change, &policy_rules());
    assert_eq!(got.per_path[0].class, Class::Docs);
    assert_eq!(got.per_path[1].class, Class::Sensitive);
    assert_eq!(got.highest, Some(Class::Sensitive));
    // A docs file inside a sensitive directory does not escape the rule.
    let hidden = classify_change(&[modify("src/auth/NOTES.md", "a", "b")], &policy_rules());
    assert_eq!(hidden.highest, Some(Class::Sensitive));
}

#[test]
fn empty_change_has_no_highest() {
    let got = classify_change(&[], &ClassRules::default());
    assert!(got.per_path.is_empty());
    assert_eq!(got.highest, None);
}

#[test]
fn per_path_keeps_input_order() {
    let change = [
        modify("src/b.rs", "1", "2"),
        modify("README.md", "a", "b"),
        modify("tests/t.rs", "1", "2"),
    ];
    let got = classify_change(&change, &ClassRules::default());
    let paths: Vec<&[u8]> = got.per_path.iter().map(|c| c.path.as_bytes()).collect();
    assert_eq!(paths, [&b"src/b.rs"[..], b"README.md", b"tests/t.rs"]);
    assert_eq!(got.highest, Some(Class::Logic));
}

#[test]
fn glob_rules() {
    let g = GlobSet::new(["src/auth", "**/*.snap", "a/?/c", "x/**/y"]).unwrap();
    assert!(g.matches("src/auth/k.rs"));
    assert!(!g.matches("src/authz/k.rs"));
    assert!(g.matches("deep/er/t.snap"));
    assert!(g.matches("a/b/c/d"));
    assert!(!g.matches("a/bb/c"));
    assert!(g.matches("x/y"));
    assert!(g.matches("x/1/2/y/z"));
    assert!(!g.matches(""));
    for bad in ["", "/", "a//b", "a/**b", "a[0]", "{a,b}", "!a"] {
        let err = GlobSet::new([bad]).unwrap_err();
        assert_eq!(err.reason, bee_core::Reason::InvalidArgument, "{bad}");
    }
}

#[test]
fn custom_globs_replace_defaults() {
    let rules = ClassRules {
        docs: GlobSet::new(["handbook"]).unwrap(),
        ..ClassRules::default()
    };
    assert_eq!(
        classify_path(modify("README.md", "a", "b"), &rules).class,
        Class::Logic
    );
    assert_eq!(
        classify_path(modify("handbook/a.bin", "a", "b"), &rules).class,
        Class::Docs
    );
}

// ---- per-language comment handling ----

#[test]
fn rust_comments() {
    use Language::Rust as L;
    same(L, "let a = 1; // x\n", "let a = 1;\n");
    same(L, "/// doc\nfn f() {}", "fn f() {}");
    same(
        L,
        "/* a /* nested */ still comment */ fn f() {}",
        "fn f() {}",
    );
    differs(L, "/* a */ code */ fn f() {}", "fn f() {}");
    differs(L, "let s = \"a // b\";", "let s = \"a // c\";");
    differs(L, "let s = \"a /* b */\";", "let s = \"a /* c */\";");
    differs(L, "let s = \"a\\\"//b\";", "let s = \"a\\\"//c\";");
    differs(L, "let s = r#\"x \"// y\"#;", "let s = r#\"x \"// z\"#;");
    differs(L, "let s = r\"/* a */\";", "let s = r\"/* b */\";");
    same(L, "let s = r#\"a\"#; // c\n", "let s = r#\"a\"#;\n");
    same(L, "fn f<'a>(x: &'a str) {} // c", "fn f<'a>(x: &'a str) {}");
    same(
        L,
        "let c = '\"'; // note\nlet d = 1;",
        "let c = '\"';\nlet d = 1;",
    );
    same(L, "let c = '\\''; // note", "let c = '\\'';");
    differs(L, "let c = '\"'; let d = 1;", "let c = '\"'; let d = 2;");
    same(L, "let b = br#\"a\"#; // c", "let b = br#\"a\"#;");
}

#[test]
fn c_family_comments() {
    use Language::C as L;
    same(L, "int x; /* c */ int y;", "int x; int y;");
    same(L, "int x; // c\n", "int x;\n");
    differs(L, "char *s = \"/* x */\";", "char *s = \"/* y */\";");
    differs(L, "char *s = \"// x\";", "char *s = \"// y\";");
    differs(L, "auto s = R\"d(// x)d\";", "auto s = R\"d(// y)d\";");
    differs(L, "auto s = R\"(/* x\n*/)\";", "auto s = R\"(/* y\n*/)\";");
    same(L, "char c = '\"'; // hi", "char c = '\"';");
    same(L, "int x = 1'000; // c", "int x = 1'000;");
    differs(L, "int a = b - -c;", "int a = b--c;");
}

#[test]
fn go_comments() {
    use Language::Go as L;
    same(L, "x := 1 // c\n/* b */\ny := 2", "x := 1\ny := 2");
    differs(L, "s := `// x`", "s := `// y`");
    differs(L, "s := `a\n// x\n`", "s := `a\n// y\n`");
    differs(L, "s := \"// x\"", "s := \"// y\"");
    same(L, "r := '\"' // c", "r := '\"'");
}

#[test]
fn java_comments() {
    use Language::Java as L;
    same(L, "int a; /** doc */ int b; // c", "int a; int b;");
    differs(
        L,
        "String s = \"\"\"\n// x\n\"\"\";",
        "String s = \"\"\"\n// y\n\"\"\";",
    );
    differs(L, "String s = \"a // b\";", "String s = \"a // c\";");
    same(L, "char c = '\"'; // c", "char c = '\"';");
}

#[test]
fn javascript_comments() {
    use Language::JavaScript as L;
    same(
        L,
        "let a = 1; // c\n/* b */ let b = 2;",
        "let a = 1;\nlet b = 2;",
    );
    differs(L, "let s = 'a // b';", "let s = 'a // c';");
    differs(L, "let s = `a ${x} // b`;", "let s = `a ${x} // c`;");
    differs(
        L,
        "let s = `a ${ \"//\" + y } b`;",
        "let s = `a ${ \"//\" + z } b`;",
    );
    differs(
        L,
        "let s = `a ${`b ${c} // x`} d`;",
        "let s = `a ${`b ${c} // y`} d`;",
    );
    same(L, "let r = /a\\/\\//g; // c", "let r = /a\\/\\//g;");
    differs(L, "let r = /[/]\\//; y = 1", "let r = /[/]\\//; y = 2");
    same(L, "a = b / c; // d", "a = b / c;");
    differs(L, "a = b / c; d = 1", "a = b / c; d = 2");
    same(
        L,
        "if (x) return /a\\/\\//.test(y); // c",
        "if (x) return /a\\/\\//.test(y);",
    );
}

#[test]
fn python_comments() {
    use Language::Python as L;
    same(L, "x = 1  # c\n# d\ny = 2\n", "x = 1\ny = 2\n");
    differs(L, "s = '# x'", "s = '# y'");
    differs(L, "s = \"a\\\"# x\"", "s = \"a\\\"# y\"");
    differs(L, "s = '''\n# x\n'''", "s = '''\n# y\n'''");
    differs(
        L,
        "s = \"\"\"a \"quoted\" # x\"\"\"",
        "s = \"\"\"a \"quoted\" # y\"\"\"",
    );
    differs(L, "s = r'\\'# x'", "s = r'\\'# y'");
    differs(L, "#!/usr/bin/python\nx = 1", "#!/usr/bin/python3\nx = 1");
    differs(L, "if x:\n    y\n", "if x:\n  y\n");
    same(L, "if x:\n    y\n\n\n    z  \n", "if x:\n    y\n    z\n");
}

#[test]
fn ruby_comments() {
    use Language::Ruby as L;
    same(L, "x = 1 # c\n", "x = 1\n");
    same(L, "=begin\nold\n=end\nx = 1\n", "x = 1\n");
    differs(L, "s = \"a #{h[\"k\"]} # b\"", "s = \"a #{h[\"k\"]} # c\"");
    differs(L, "s = 'a # b'", "s = 'a # c'");
    differs(L, "s = <<~EOS\n  # x\nEOS\n", "s = <<~EOS\n  # y\nEOS\n");
    same(
        L,
        "s = <<~EOS # c\n  text\nEOS\nx = 1\n",
        "s = <<~EOS\n  text\nEOS\nx = 1\n",
    );
    differs(L, "x = /a#b/", "x = /a#c/");
    differs(L, "x = %w[a #b]", "x = %w[a #c]");
    differs(
        L,
        "x = 1\n__END__\n# data a\n",
        "x = 1\n__END__\n# data b\n",
    );
    same(L, "a << b # c", "a << b");
}

#[test]
fn shell_comments() {
    use Language::Shell as L;
    same(L, "echo hi # c\n# top\n", "echo hi\n");
    differs(L, "echo a#b", "echo a#c");
    differs(L, "echo $# x", "echo $# y");
    differs(L, "echo \"# x\"", "echo \"# y\"");
    differs(L, "echo '# x'", "echo '# y'");
    differs(L, "echo \\# x", "echo \\# y");
    differs(L, "echo \"$(echo \"#x\")\"", "echo \"$(echo \"#y\")\"");
    differs(L, "cat <<EOF\n# x\nEOF\n", "cat <<EOF\n# y\nEOF\n");
    differs(
        L,
        "cat <<-'EOF'\n\t# x\n\tEOF\necho 1",
        "cat <<-'EOF'\n\t# y\n\tEOF\necho 1",
    );
    same(
        L,
        "cat <<EOF\ntext\nEOF\necho 1 # c",
        "cat <<EOF\ntext\nEOF\necho 1",
    );
    differs(L, "#!/bin/sh\necho 1", "#!/bin/bash\necho 1");
    same(L, "a=$(( 1 << n )) # c", "a=$(( 1 << n ))");
}

#[test]
fn yaml_comments() {
    use Language::Yaml as L;
    same(L, "a: b # c\n# d\nx: 1\n", "a: b\nx: 1\n");
    differs(L, "a: b#c\n", "a: b#d\n");
    differs(L, "a: \"x # y\"\n", "a: \"x # z\"\n");
    differs(L, "a: 'x # y'\n", "a: 'x # z'\n");
    differs(L, "a: |\n  # x\nb: 1\n", "a: |\n  # y\nb: 1\n");
    same(L, "a: |\n  text\nb: 1 # c\n", "a: |\n  text\nb: 1\n");
    same(L, "a: don't # c\n", "a: don't\n");
    differs(L, "a:\n  b: 1\n", "a:\n    b: 1\n");
    differs(L, "- >-\n  # x\n- y\n", "- >-\n  # y\n- y\n");
}

#[test]
fn toml_comments() {
    use Language::Toml as L;
    same(L, "a = 1 # c\n# d\nb = 2\n", "a = 1\nb = 2\n");
    differs(L, "a = \"x # y\"", "a = \"x # z\"");
    differs(L, "a = 'x # y'", "a = 'x # z'");
    differs(L, "a = '''\n# x\n'''", "a = '''\n# y\n'''");
    differs(
        L,
        "a = \"\"\"\n# x \\\" # q\n\"\"\"",
        "a = \"\"\"\n# y \\\" # q\n\"\"\"",
    );
}

#[test]
fn css_comments() {
    use Language::Css as L;
    same(L, "a { color: red; /* c */ }", "a { color: red; }");
    differs(
        L,
        "a { content: \"/* x */\"; }",
        "a { content: \"/* y */\"; }",
    );
    // `//` is not a CSS comment.
    differs(
        L,
        "a { background: url(//x.com/a.png); }",
        "a { background: url(//y.com/a.png); }",
    );
}

#[test]
fn sql_comments() {
    use Language::Sql as L;
    same(L, "select 1; -- c\n", "select 1;\n");
    same(L, "select /* c */ 1;", "select 1;");
    differs(L, "select '-- x';", "select '-- y';");
    differs(L, "select \"a--b\" from t;", "select \"a--c\" from t;");
    differs(L, "select $$ -- x $$;", "select $$ -- y $$;");
    differs(L, "select $q$ /* x */ $q$;", "select $q$ /* y */ $q$;");
    same(L, "select $1; -- c", "select $1;");
}

#[test]
fn whitespace_rules() {
    same(Language::Rust, "a  +  b", "a + b");
    same(Language::Rust, "a = b", "a=b");
    differs(Language::Rust, "let x", "letx");
    same(Language::Rust, "f(a,\n\n\n  b)", "f(a,\n b)");
    differs(Language::Rust, "a\nb", "a b");
    same(Language::Rust, "a\r\nb\r\n", "a\nb\n");
    same(Language::Rust, "\u{feff}a", "a");
    // An unterminated string swallows the rest, so later edits are not comment-only.
    differs(Language::Rust, "let s = \"abc // x", "let s = \"abc // y");
}

#[test]
fn language_detection() {
    let cases = [
        ("a/b.rs", Some(Language::Rust)),
        ("a/B.RS", Some(Language::Rust)),
        ("x.hpp", Some(Language::C)),
        ("x.go", Some(Language::Go)),
        ("X.java", Some(Language::Java)),
        ("x.tsx", Some(Language::JavaScript)),
        ("x.py", Some(Language::Python)),
        ("Gemfile", Some(Language::Ruby)),
        ("x.sh", Some(Language::Shell)),
        ("x.yml", Some(Language::Yaml)),
        ("Cargo.toml", Some(Language::Toml)),
        ("x.css", Some(Language::Css)),
        ("x.sql", Some(Language::Sql)),
        ("x.md", None),
        ("Makefile", None),
        ("x.zig", None),
    ];
    for (p, want) in cases {
        assert_eq!(Language::detect(p), want, "{p}");
    }
}

// ---- properties ----

/// Snippets per language. Spaces occur only in code or comments (never inside literals), and a
/// leading `!` marks a unit that must not be followed by blank lines or trailing whitespace.
fn corpus(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &[
            "fn main() {",
            "let x = a + b; // note",
            "let s = \"a//b\";",
            "/* block */ let y = x * 2;",
            "let c = '\"'; let r = r#\"x//y\"#;",
            "}",
        ],
        Language::C => &[
            "int main(void) {",
            "char *s = \"/*x*/\"; /* c */",
            "char c = '\"'; // c",
            "auto r = R\"d(//x)d\";",
            "}",
        ],
        Language::Go => &["func f() {", "s := `//x` // c", "r := '\"'", "}"],
        Language::Java => &[
            "class A {",
            "String s = \"a//b\"; // c",
            "String t = \"\"\"\n//x\n\"\"\";",
            "}",
        ],
        Language::JavaScript => &[
            "let a = b / c; // d",
            "const s = `x${y}//z`;",
            "const r = /a\\/\\//g; /* c */",
            "f(a, b);",
        ],
        Language::Python => &[
            "def f(x):",
            "    return x  # c",
            "s = '#x'",
            "t = '''\n#y\n'''",
            "if a  and b:",
            "    pass",
        ],
        Language::Ruby => &[
            "x = 1 # c",
            "s = \"a#{y}#b\"",
            "r = /a#b/",
            "!t = <<~EOS\n  #x\nEOS",
            "=begin\nold\n=end",
        ],
        Language::Shell => &[
            "echo hi # c",
            "echo a#b $# \"#x\" '#y'",
            "!cat <<EOF\n#x\nEOF",
            "if true; then",
            "fi",
        ],
        Language::Yaml => &[
            "a: b # c",
            "d: \"x#y\"",
            "e: f#g",
            "list:",
            "  - one",
            "!h: |\n  #x\n  text",
        ],
        Language::Toml => &[
            "a = 1 # c",
            "b = \"x#y\"",
            "c = '''\n#x\n'''",
            "[t]",
            "d = [1, 2]",
        ],
        Language::Css => &["a { color: red; /* c */ }", "b { content: \"/*x*/\"; }"],
        Language::Sql => &[
            "select 1; -- c",
            "select '--x' from t;",
            "select $$--y$$;",
            "select /* c */ a, b from t;",
        ],
    }
}

const SPACES: [&str; 4] = [" ", "  ", "\t", "   "];
const TAILS: [&str; 3] = ["", " ", "\t  "];

/// Re-renders the units with different whitespace: interior spaces widened, trailing whitespace
/// and blank lines added between units. Leading indentation is left alone.
fn reflow(units: &[&str], choices: &[u8]) -> String {
    let mut k = 0usize;
    let mut next = || {
        let c = choices[k % choices.len()] as usize;
        k += 1;
        c
    };
    let mut out = String::new();
    for unit in units {
        let (unit, tail_ok) = match unit.strip_prefix('!') {
            Some(rest) => (rest, false),
            None => (*unit, true),
        };
        let mut prev: Option<char> = None;
        for ch in unit.chars() {
            let interior = prev.is_some_and(|p| p != ' ' && p != '\t' && p != '\n');
            if ch == ' ' && interior {
                out.push_str(SPACES[next() % SPACES.len()]);
            } else {
                out.push(ch);
            }
            prev = Some(ch);
        }
        if tail_ok {
            out.push_str(TAILS[next() % TAILS.len()]);
        }
        out.push('\n');
        if tail_ok {
            for _ in 0..next() % 3 {
                out.push('\n');
            }
        }
    }
    out
}

const EXT: [(Language, &str); 12] = [
    (Language::Rust, "rs"),
    (Language::C, "c"),
    (Language::Go, "go"),
    (Language::Java, "java"),
    (Language::JavaScript, "ts"),
    (Language::Python, "py"),
    (Language::Ruby, "rb"),
    (Language::Shell, "sh"),
    (Language::Yaml, "yaml"),
    (Language::Toml, "toml"),
    (Language::Css, "css"),
    (Language::Sql, "sql"),
];

fn pool_change(i: usize, kind: u8) -> PathChange {
    let paths = [
        "src/lib.rs",
        "README.md",
        "docs/a.md",
        "tests/t.rs",
        "Cargo.toml",
        "src/auth/k.rs",
        "gen/x.rs",
        "src/z.zig",
    ];
    let path = paths[i % paths.len()];
    match kind % 4 {
        0 => modify(path, "fn a() {}", "// c\nfn a() {}"),
        1 => modify(path, "a", "b"),
        2 => add(path, "x"),
        _ => rename("old/name.rs", path, "same", "same"),
    }
}

proptest! {
    #[test]
    fn whitespace_only_changes_stay_at_or_below_comment_only(
        which in 0usize..12,
        choices in proptest::collection::vec(any::<u8>(), 1..64),
        pick in proptest::collection::vec(any::<bool>(), 0..8),
    ) {
        let (lang, ext) = EXT[which];
        let all = corpus(lang);
        let units: Vec<&str> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| pick.get(*i).copied().unwrap_or(true))
            .map(|(_, u)| *u)
            .collect();
        let plain: String = units.iter().map(|u| format!("{}\n", u.trim_start_matches('!'))).collect();
        let spaced = reflow(&units, &choices);
        let path = format!("src/x.{ext}");
        let rules = ClassRules::default();
        let there = classify_path(modify(&path, &plain, &spaced), &rules);
        let back = classify_path(modify(&path, &spaced, &plain), &rules);
        prop_assert!(there.class <= Class::CommentOnly, "{:?}\n{}\n---\n{}", there, plain, spaced);
        prop_assert!(back.class <= Class::CommentOnly, "{:?}", back);
    }

    #[test]
    fn added_comments_stay_at_or_below_comment_only(
        which in 0usize..12,
        choices in proptest::collection::vec(any::<u8>(), 1..16),
    ) {
        let (lang, ext) = EXT[which];
        let units = corpus(lang);
        let plain: String = units.iter().map(|u| format!("{}\n", u.trim_start_matches('!'))).collect();
        let marker = match lang {
            Language::Rust | Language::C | Language::Go | Language::Java | Language::JavaScript => "// added",
            Language::Sql => "-- added",
            Language::Css => "/* added */",
            _ => "# added",
        };
        let mut with = format!("{marker}\n{plain}");
        with.push_str(marker);
        with.push('\n');
        let _ = choices;
        let got = classify_path(modify(&format!("src/x.{ext}"), &plain, &with), &ClassRules::default());
        prop_assert!(got.class <= Class::CommentOnly, "{:?}", got);
    }

    #[test]
    fn highest_is_the_max_of_per_path(
        picks in proptest::collection::vec((0usize..8, any::<u8>()), 0..12),
    ) {
        let rules = policy_rules();
        let changes: Vec<PathChange> = picks.iter().map(|(i, k)| pool_change(*i, *k)).collect();
        let got = classify_change(&changes, &rules);
        prop_assert_eq!(got.per_path.len(), changes.len());
        prop_assert_eq!(got.highest, got.per_path.iter().map(|c| c.class).max());
        for (c, one) in changes.iter().zip(&got.per_path) {
            prop_assert_eq!(&classify_path(c.clone(), &rules), one);
        }
    }

    #[test]
    fn classification_is_deterministic(
        picks in proptest::collection::vec((0usize..8, any::<u8>()), 0..12),
    ) {
        let rules = policy_rules();
        let changes: Vec<PathChange> = picks.iter().map(|(i, k)| pool_change(*i, *k)).collect();
        let mut reversed = changes.clone();
        reversed.reverse();
        prop_assert_eq!(classify_change(&changes, &rules), classify_change(&changes, &rules));
        // Order of inputs changes only the order of the output, never a class.
        let fwd = classify_change(&changes, &rules);
        let mut rev = classify_change(&reversed, &rules);
        rev.per_path.reverse();
        prop_assert_eq!(fwd, rev);
    }

    #[test]
    fn normalize_never_panics_on_arbitrary_text(which in 0usize..12, text in "\\PC{0,200}") {
        let (lang, _) = EXT[which];
        let a = normalize(lang, &text);
        prop_assert_eq!(a, normalize(lang, &text));
    }
}
