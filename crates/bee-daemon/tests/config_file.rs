//! The `bee-daemon` binary reads its configuration file (`--config` or
//! `$BEE_DAEMON_CONFIG`) with flags and the environment layered on top, and refuses unknown
//! keys before it starts.

use std::process::Command;

fn daemon(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bee-daemon"));
    cmd.args(args)
        .env_remove("BEE_ENDPOINT")
        .env_remove("BEE_DAEMON_CONFIG");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run bee-daemon");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn file_then_environment_then_flags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.toml");
    std::fs::write(
        &path,
        "[daemon]\nupstream = \"https://file.example\"\ncache_dir = \"${CACHE_ROOT}\"\n\
         max_bytes = 1000\nconfirm_ms = 250\nbootstrap = [\"acme/app\"]\n",
    )
    .unwrap();
    let file = path.to_str().unwrap();
    let (code, out, err) = daemon(
        &[
            "serve",
            "--config",
            file,
            "--print-config",
            "--max-bytes",
            "2000",
        ],
        &[("CACHE_ROOT", "/var/cache/bee")],
    );
    assert_eq!(code, 0, "{err}");
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["upstream"], "https://file.example");
    assert_eq!(doc["cache_dir"], "/var/cache/bee");
    assert_eq!(doc["max_bytes"], 2000, "the flag wins");
    assert_eq!(doc["confirm_ms"], 250);
    assert_eq!(doc["bootstrap"], serde_json::json!(["acme/app"]));
    assert_eq!(doc["upstream_concurrency"], 32, "a default");
    // The environment names the file and overrides the upstream.
    let (code, out, err) = daemon(
        &["regional", "--print-config", "--listen", "127.0.0.1:0"],
        &[
            ("BEE_DAEMON_CONFIG", file),
            ("CACHE_ROOT", "/c"),
            ("BEE_ENDPOINT", "https://env.example"),
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["upstream"], "https://env.example");
    assert_eq!(doc["listen"], "127.0.0.1:0");
}

#[test]
fn unknown_keys_and_unset_variables_stop_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.toml");
    std::fs::write(&path, "[daemon]\nmax_byts = 1\n").unwrap();
    let (code, _, err) = daemon(
        &[
            "serve",
            "--config",
            path.to_str().unwrap(),
            "--print-config",
        ],
        &[],
    );
    assert_eq!(code, 1);
    assert!(err.contains("max_byts"), "{err}");
    std::fs::write(&path, "[daemon]\ncache_dir = \"$UNSET_FOR_THIS_TEST\"\n").unwrap();
    let (code, _, err) = daemon(
        &[
            "serve",
            "--config",
            path.to_str().unwrap(),
            "--print-config",
        ],
        &[],
    );
    assert_eq!(code, 1);
    assert!(err.contains("UNSET_FOR_THIS_TEST"), "{err}");
}
