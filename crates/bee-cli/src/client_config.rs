//! The client configuration file: `config.toml` in the config directory
//! ([`crate::credentials::config_dir`]: `$BEE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/bee`, else
//! `~/.config/bee`), next to the `credentials` file (mode 0600) that `bee login` writes.
//!
//! Every key is optional and unknown keys are a usage error. Each setting is resolved in one
//! order: the command-line flag, then the environment variable, then this file, then the
//! built-in default.
//!
//! | Key | Flag | Environment | Default |
//! |---|---|---|---|
//! | `endpoint` | `--endpoint` | `BEE_ENDPOINT` | the endpoint saved by `bee login` |
//! | `timeout` | `--timeout` | `BEE_TIMEOUT` | none |
//! | `json` | `--json` | `BEE_JSON` (`1`/`true`) | false |
//! | `daemon` | `--no-daemon` | `BEE_NO_DAEMON` | true |
//! | `daemon_socket` | `daemon --socket` | `BEE_DAEMON_SOCKET` | the per-user socket |
//! | `author` | `--author` | `BEE_AUTHOR` | `$USER` |

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::context::parse_duration;
use crate::error::CliError;

/// The file's settings (each `None` when the file does not set it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Server endpoint.
    pub endpoint: Option<String>,
    /// Request timeout, e.g. `30s`.
    pub timeout: Option<String>,
    /// Print `--json` documents by default.
    pub json: Option<bool>,
    /// Use the node daemon when it answers.
    pub daemon: Option<bool>,
    /// The node daemon socket.
    pub daemon_socket: Option<String>,
    /// Author name of new changesets.
    pub author: Option<String>,
}

static LOADED: OnceLock<ClientConfig> = OnceLock::new();

/// `<config dir>/config.toml`.
pub fn path() -> Result<PathBuf, CliError> {
    Ok(crate::credentials::config_dir()?.join("config.toml"))
}

/// Parses a configuration file's text.
pub fn parse(text: &str) -> Result<ClientConfig, CliError> {
    let cfg: ClientConfig = toml::from_str(text).map_err(|e| {
        CliError::usage(
            "bad_config",
            format!("config.toml: {}", e.to_string().trim()),
        )
    })?;
    if let Some(t) = &cfg.timeout {
        parse_duration(t).map_err(|e| {
            CliError::usage("bad_config", format!("config.toml: timeout: {}", e.message))
        })?;
    }
    if let Some(e) = &cfg.endpoint {
        crate::rpc::Endpoint::parse(e).map_err(|err| {
            CliError::usage(
                "bad_config",
                format!("config.toml: endpoint: {}", err.message),
            )
        })?;
    }
    Ok(cfg)
}

/// Reads the file once per process (a missing file is an empty configuration).
pub fn load() -> Result<&'static ClientConfig, CliError> {
    if let Some(c) = LOADED.get() {
        return Ok(c);
    }
    let cfg = match path() {
        Ok(p) => match std::fs::read_to_string(&p) {
            Ok(text) => parse(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ClientConfig::default(),
            Err(e) => {
                return Err(CliError::usage(
                    "bad_config",
                    format!("{}: {e}", p.display()),
                ));
            }
        },
        // No config directory at all: nothing to read.
        Err(_) => ClientConfig::default(),
    };
    Ok(LOADED.get_or_init(|| cfg))
}

/// The loaded configuration (empty before [`load`]).
pub fn get() -> ClientConfig {
    LOADED.get().cloned().unwrap_or_default()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The timeout from the environment, then the file.
pub fn timeout() -> Result<Option<Duration>, CliError> {
    match env("BEE_TIMEOUT").or_else(|| get().timeout) {
        Some(t) => parse_duration(&t).map(Some),
        None => Ok(None),
    }
}

/// `--json` by default, from the environment, then the file.
pub fn json() -> bool {
    match env("BEE_JSON") {
        Some(v) => matches!(v.trim(), "1" | "true" | "yes"),
        None => get().json.unwrap_or(false),
    }
}

/// Whether the daemon may be used: `BEE_NO_DAEMON` wins, then the file.
pub fn daemon_allowed() -> bool {
    match env("BEE_NO_DAEMON") {
        Some(v) => v.trim() == "0",
        None => get().daemon.unwrap_or(true),
    }
}

/// The daemon socket from the environment, then the file.
pub fn daemon_socket() -> Option<String> {
    env("BEE_DAEMON_SOCKET").or_else(|| get().daemon_socket)
}

/// The author from the environment, then the file.
pub fn author() -> Option<String> {
    env("BEE_AUTHOR").or_else(|| get().author.filter(|a| !a.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_parses_and_unknown_keys_fail() {
        let c = parse(
            "endpoint = \"http://127.0.0.1:7420\"\ntimeout = \"30s\"\njson = true\n\
             daemon = false\ndaemon_socket = \"/run/bee.sock\"\nauthor = \"Ada\"\n",
        )
        .unwrap();
        assert_eq!(c.endpoint.as_deref(), Some("http://127.0.0.1:7420"));
        assert_eq!(c.timeout.as_deref(), Some("30s"));
        assert_eq!((c.json, c.daemon), (Some(true), Some(false)));
        assert_eq!(c.daemon_socket.as_deref(), Some("/run/bee.sock"));
        assert_eq!(c.author.as_deref(), Some("Ada"));
        assert_eq!(parse("").unwrap(), ClientConfig::default());
        let e = parse("endpont = \"x\"\n").unwrap_err();
        assert!(e.message.contains("endpont"), "{e:?}");
        assert!(parse("timeout = \"soon\"\n").is_err());
        assert!(parse("json = \"yes\"\n").is_err());
    }
}
