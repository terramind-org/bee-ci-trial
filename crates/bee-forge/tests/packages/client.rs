//! Opt-in tests with the real clients (`npm`, `cargo`, `oras`/`docker`). They need a server that
//! mounts [`bee_forge::packages::PackageRegistry`] and run only with `BEE_TEST_CLIENTS=1`;
//! `BEE_TEST_REGISTRY` names the server (for example `http://127.0.0.1:8080`) and
//! `BEE_TEST_TOKEN` a token that may publish under the org named by `BEE_TEST_OWNER`.
//! Without them the tests print why they skipped and pass.

use std::process::Command;

fn enabled() -> Option<(String, String, String)> {
    if std::env::var("BEE_TEST_CLIENTS").ok().as_deref() != Some("1") {
        eprintln!("BEE_TEST_CLIENTS is not 1; skipping real-client tests");
        return None;
    }
    let reg = std::env::var("BEE_TEST_REGISTRY").ok()?;
    let tok = std::env::var("BEE_TEST_TOKEN").ok()?;
    let owner = std::env::var("BEE_TEST_OWNER").unwrap_or_else(|_| "acme".into());
    Some((reg, tok, owner))
}

fn on_path(bin: &str) -> bool {
    Command::new(bin).arg("--version").output().is_ok()
}

#[test]
fn npm_publish_and_install_with_the_real_client() {
    let Some((reg, tok, owner)) = enabled() else {
        return;
    };
    if !on_path("npm") {
        eprintln!("npm is not on PATH; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        format!("{{\"name\":\"@{owner}/clienttest\",\"version\":\"{}.0.0\",\"publishConfig\":{{\"access\":\"restricted\"}}}}", std::process::id()),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".npmrc"),
        format!(
            "@{owner}:registry={reg}/npm/{owner}/\n//{}/npm/{owner}/:_authToken={tok}\n",
            reg.split("://").nth(1).unwrap_or(&reg)
        ),
    )
    .unwrap();
    let out = Command::new("npm")
        .arg("publish")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new("npm")
        .args(["view", &format!("@{owner}/clienttest"), "version"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cargo_index_is_readable_by_the_real_client() {
    let Some((reg, _tok, owner)) = enabled() else {
        return;
    };
    if !on_path("cargo") {
        eprintln!("cargo is not on PATH; skipping");
        return;
    }
    let out = Command::new("cargo")
        .args([
            "search",
            "--index",
            &format!("sparse+{reg}/cargo/{owner}/index/"),
            "clienttest",
        ])
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn oras_pushes_and_pulls_an_artifact() {
    let Some((reg, tok, owner)) = enabled() else {
        return;
    };
    if !on_path("oras") {
        eprintln!("oras is not on PATH; skipping");
        return;
    }
    let host = reg.split("://").nth(1).unwrap_or(&reg).to_string();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello").unwrap();
    let target = format!("{host}/{owner}/clienttest:v{}", std::process::id());
    let login = Command::new("oras")
        .args(["login", "--plain-http", "-u", "token", "-p", &tok, &host])
        .output()
        .unwrap();
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );
    let push = Command::new("oras")
        .args(["push", "--plain-http", &target, "hello.txt"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    let pull = Command::new("oras")
        .args(["pull", "--plain-http", &target])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        pull.status.success(),
        "{}",
        String::from_utf8_lossy(&pull.stderr)
    );
}
