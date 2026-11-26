//! Writes the generated OpenAPI document and JSON Schemas into the repository.
//!
//! Usage: `cargo run -p bee-proto --bin bee-proto-gen [REPO_ROOT]`. Without an argument
//! the repository is found relative to this crate. Stale message schemas are removed.

use std::path::PathBuf;
use std::process::ExitCode;

use bee_proto::schema::{Catalog, MESSAGES_DIR};

fn main() -> ExitCode {
    let root = std::env::args().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    );
    match write_all(&root) {
        Ok(count) => {
            println!(
                "bee-proto-gen: wrote {count} files under {}",
                root.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bee-proto-gen: {e}");
            ExitCode::FAILURE
        }
    }
}

fn write_all(root: &std::path::Path) -> std::io::Result<usize> {
    let files = Catalog::load().generated_files();
    let messages_dir = root.join(MESSAGES_DIR);
    if messages_dir.is_dir() {
        for entry in std::fs::read_dir(&messages_dir)? {
            let path = entry?.path();
            let relative = format!(
                "{MESSAGES_DIR}/{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
            );
            if !files.contains_key(&relative) {
                std::fs::remove_file(&path)?;
            }
        }
    }
    for (relative, content) in &files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&path, content)?;
        }
    }
    Ok(files.len())
}
