//! Writes the CLI output schemas into `schemas/v1/cli/`.
//!
//! Usage: `cargo run -p bee-cli --bin bee-cli-gen [REPO_ROOT]`. Without an argument the
//! repository is found relative to this crate. Stale schema files are removed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bee_cli::schemas::{CLI_SCHEMA_DIR, generated_files};

fn main() -> ExitCode {
    let root = std::env::args().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    );
    match write_all(&root) {
        Ok(count) => {
            println!(
                "bee-cli-gen: wrote {count} files under {}",
                root.join(CLI_SCHEMA_DIR).display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bee-cli-gen: {e}");
            ExitCode::FAILURE
        }
    }
}

fn write_all(root: &Path) -> std::io::Result<usize> {
    let files = generated_files();
    let dir = root.join(CLI_SCHEMA_DIR);
    std::fs::create_dir_all(&dir)?;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let relative = format!(
            "{CLI_SCHEMA_DIR}/{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        );
        if relative.ends_with(".schema.json") && !files.contains_key(&relative) {
            std::fs::remove_file(&path)?;
        }
    }
    for (relative, content) in &files {
        let path = root.join(relative);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&path, content)?;
        }
    }
    Ok(files.len())
}
