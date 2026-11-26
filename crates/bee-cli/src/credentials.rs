//! The credentials file: `~/.config/bee/credentials`, a JSON object `{"token", "endpoint"?}`
//! readable only by its owner (mode 0600).
//!
//! The directory is `$BEE_CONFIG_DIR` when set, else `$XDG_CONFIG_HOME/bee`, else
//! `$HOME/.config/bee`.

use std::path::{Path, PathBuf};

use bee_proto::v1::ErrorReason;
use serde_json::{Map, Value, json};

use crate::error::CliError;

/// File name inside the config directory.
pub const FILE_NAME: &str = "credentials";

/// What `bee login` stores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// Bearer token.
    pub token: String,
    /// Default endpoint, if one was given at login.
    pub endpoint: Option<String>,
}

/// The config directory.
pub fn config_dir() -> Result<PathBuf, CliError> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(dir) = var("BEE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("bee"));
    }
    if let Some(home) = var("HOME") {
        return Ok(PathBuf::from(home).join(".config").join("bee"));
    }
    Err(CliError::usage(
        "no_config_dir",
        "cannot locate the config directory: set HOME or BEE_CONFIG_DIR",
    ))
}

/// Path of the credentials file.
pub fn path() -> Result<PathBuf, CliError> {
    Ok(config_dir()?.join(FILE_NAME))
}

fn io_error(action: &str, path: &Path, e: &std::io::Error) -> CliError {
    CliError::new(
        ErrorReason::Internal,
        format!("cannot {action} {}: {e}", path.display()),
    )
    .with_payload("subcode", json!("credentials_io"))
}

/// Reads the credentials file; `None` if it does not exist. Fails if the file is readable or
/// writable by anyone but its owner.
pub fn load() -> Result<Option<Credentials>, CliError> {
    let path = path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("read", &path, &e)),
    };
    check_private(&path)?;
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        CliError::new(
            ErrorReason::InvalidState,
            format!("{} is not valid JSON: {e}", path.display()),
        )
        .with_payload("subcode", json!("bad_credentials"))
    })?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let endpoint = value
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Some(Credentials { token, endpoint }))
}

#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .map_err(|e| io_error("stat", path, &e))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(CliError::new(
            ErrorReason::InvalidState,
            format!(
                "{} is accessible by other users (mode {:o}); run `chmod 600 {}` or \
                 `bee login` again",
                path.display(),
                mode & 0o777,
                path.display()
            ),
        )
        .with_payload("subcode", json!("insecure_credentials")));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<(), CliError> {
    Ok(())
}

/// Writes the credentials file with mode 0600 (directory 0700), replacing any previous one.
pub fn store(credentials: &Credentials) -> Result<PathBuf, CliError> {
    let path = path()?;
    let dir = path.parent().unwrap_or(Path::new("."));
    create_private_dir(dir).map_err(|e| io_error("create", dir, &e))?;
    let mut object = Map::new();
    object.insert("token".into(), json!(credentials.token));
    if let Some(endpoint) = &credentials.endpoint {
        object.insert("endpoint".into(), json!(endpoint));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(object))
        .map_err(|e| CliError::internal(e.to_string()))?;
    text.push('\n');
    // Write a private temporary file, then rename over the old one: the token is never in a
    // file with broader permissions, and readers never see a partial file.
    let temporary = dir.join(format!(".{FILE_NAME}.tmp"));
    write_private(&temporary, text.as_bytes()).map_err(|e| io_error("write", &temporary, &e))?;
    std::fs::rename(&temporary, &path).map_err(|e| io_error("replace", &path, &e))?;
    Ok(path)
}

/// Deletes the credentials file. Returns whether one existed.
pub fn remove() -> Result<(PathBuf, bool), CliError> {
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok((path, true)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((path, false)),
        Err(e) => Err(io_error("remove", &path, &e)),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // The umask can only narrow the mode; set it explicitly anyway.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}
