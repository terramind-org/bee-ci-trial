//! `bee import github` delegates without putting credentials in a process argument.

use std::os::unix::fs::PermissionsExt as _;

use serde_json::json;
use support::{Bee, validate};

use crate::support;

fn fake_importer(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bee-github");
    std::fs::write(&path, body).expect("fake importer");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dir
}

#[test]
fn github_import_is_listed_in_root_and_import_help() {
    let root = Bee::new(&["--help"]).run().0;
    assert_eq!(root.code, 0, "{root:?}");
    assert!(root.stdout.contains("import"), "{}", root.stdout);
    let import = Bee::new(&["import", "--help"]).run().0;
    assert_eq!(import.code, 0, "{import:?}");
    assert!(import.stdout.contains("github"), "{}", import.stdout);
}

#[test]
fn github_import_forwards_arguments_and_credentials_without_argv_token() {
    let fake = fake_importer(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGS_FILE\"\nprintf '%s' \"$BEE_TOKEN\" > \"$TOKEN_FILE\"\nprintf '{\"phase\":\"mirror\"}\\n'\nexit 17\n",
    );
    let args = fake.path().join("args");
    let token = fake.path().join("token");
    let run = Bee::new(&[
        "--endpoint",
        "http://127.0.0.1:7420",
        "--token",
        "not-in-argv",
        "import",
        "github",
        "acme/widget",
        "--dry-run",
        "--resume",
    ])
    .env(
        "BEE_GITHUB_BIN",
        fake.path().join("bee-github").to_str().unwrap(),
    )
    .env("ARGS_FILE", args.to_str().unwrap())
    .env("TOKEN_FILE", token.to_str().unwrap())
    .run()
    .0;
    assert_eq!(run.code, 17, "{run:?}");
    let seen = std::fs::read_to_string(args).expect("arguments");
    assert_eq!(seen, "import\nacme/widget\n--resume\n--dry-run\n");
    assert!(!seen.contains("not-in-argv"));
    assert_eq!(
        std::fs::read_to_string(token).expect("token"),
        "not-in-argv"
    );
}

#[test]
fn github_import_json_uses_the_published_envelope() {
    let fake = fake_importer("#!/bin/sh\nprintf '{\"phase\":\"mirror\"}\\n'\n");
    let run = Bee::new(&[
        "--json",
        "--endpoint",
        "http://127.0.0.1:7420",
        "import",
        "github",
        "acme/widget",
    ])
    .env(
        "BEE_GITHUB_BIN",
        fake.path().join("bee-github").to_str().unwrap(),
    )
    .run()
    .0;
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.json();
    validate("schemas/v1/cli/import.github.schema.json", &document);
    assert_eq!(document["data"], json!({"phase": "mirror"}));
}

#[test]
fn github_import_does_not_run_an_importer_from_path() {
    let fake = fake_importer(
        "#!/bin/sh\nprintf '%s' \"$BEE_TOKEN\" > \"$TOKEN_FILE\"\ntouch \"$MARKER_FILE\"\n",
    );
    let marker = fake.path().join("ran");
    let token = fake.path().join("token");
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", fake.path().display());
    let run = Bee::alone(&[
        "--endpoint",
        "http://127.0.0.1:7420",
        "--token",
        "not-for-path",
        "import",
        "github",
        "acme/widget",
    ])
    .env("PATH", &path)
    .env("MARKER_FILE", marker.to_str().unwrap())
    .env("TOKEN_FILE", token.to_str().unwrap())
    .run()
    .0;
    assert_eq!(run.code, 1, "{run:?}");
    assert!(
        run.stderr.contains("bee-github` is not installed"),
        "{run:?}"
    );
    assert!(!marker.exists(), "PATH importer ran");
    assert!(!token.exists(), "PATH importer received the token");
}

#[test]
fn github_import_refuses_a_relative_importer_path() {
    let run = Bee::new(&[
        "--endpoint",
        "http://127.0.0.1:7420",
        "import",
        "github",
        "acme/widget",
    ])
    .env("BEE_GITHUB_BIN", "bee-github")
    .run()
    .0;
    assert_eq!(run.code, 1, "{run:?}");
    assert!(run.stderr.contains("absolute path"), "{run:?}");
}

#[test]
fn github_import_explains_how_to_install_the_importer() {
    let run = Bee::alone(&[
        "--endpoint",
        "http://127.0.0.1:7420",
        "import",
        "github",
        "acme/widget",
    ])
    .env("PATH", "")
    .run()
    .0;
    assert_eq!(run.code, 1, "{run:?}");
    assert!(
        run.stderr.contains("bee-github` is not installed"),
        "{run:?}"
    );
}
