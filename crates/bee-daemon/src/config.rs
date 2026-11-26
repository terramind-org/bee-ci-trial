//! The daemon and regional cache configuration file (`bee-daemon serve|regional --config
//! FILE`, or `$BEE_DAEMON_CONFIG`; `docs/spec/cache-tiers.md` §9).
//!
//! One TOML table, `[daemon]`, every key optional. Unknown keys are errors. A string value that
//! is exactly `$NAME` or `${NAME}` is replaced by the environment variable `NAME` (an unset
//! variable is an error); a value starting with `$$` stands for a literal `$`. Precedence, low
//! to high: built-in defaults, the file, environment variables (`BEE_ENDPOINT` for the
//! upstream), command-line flags.
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `upstream` | — (required somewhere) | URL of the tier above |
//! | `socket` | `$BEE_DAEMON_SOCKET`, `$XDG_RUNTIME_DIR/bee/daemon.sock`, `~/.bee/daemon.sock` | Node daemon socket |
//! | `listen` | — (required for `regional`) | Regional cache TCP address |
//! | `cache_dir` | `$XDG_CACHE_HOME/bee/daemon` | Node daemon disk cache |
//! | `max_bytes` | 10 GiB | Disk cache high watermark |
//! | `low_watermark_pct` | 90 | Eviction stops at this share of `max_bytes` |
//! | `fsync` | true | fsync each cached object before it becomes visible |
//! | `memory_bytes` | 4 GiB | Regional memory cache size |
//! | `signed_urls` | true | Ask the tier above for signed CDN URLs |
//! | `confirm_ms` | 5000 | How long access decisions stay trusted after the last confirmation |
//! | `ref_ttl_ms` | 1000 | How long a cached `ResolveRef` answer is served |
//! | `upstream_concurrency` | 32 | Concurrent requests to the tier above |
//! | `max_retries` | 3 | Retries of a retryable upstream failure |
//! | `bootstrap` | `[]` | Repos (`org/repo`) loaded from snapshots at start and on a schedule |
//! | `bootstrap_sparse` | `[]` | Path prefixes of those snapshots |
//! | `bootstrap_every_secs` | 3600 (at least 60) | Snapshot refresh interval |

use std::path::PathBuf;

use serde::Deserialize;

use crate::{DiskConfig, TierConfig, TierKind, default_cache_dir, default_socket_path};

/// Every setting, after defaults, the file and the environment.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// URL of the tier above.
    pub upstream: Option<String>,
    /// Node daemon socket.
    pub socket: PathBuf,
    /// Regional listen address.
    pub listen: Option<String>,
    /// Disk cache directory.
    pub cache_dir: PathBuf,
    /// Disk cache high watermark.
    pub max_bytes: u64,
    /// Eviction low watermark (percent).
    pub low_watermark_pct: u8,
    /// fsync cached objects.
    pub fsync: bool,
    /// Regional memory cache size.
    pub memory_bytes: u64,
    /// Ask for signed URLs.
    pub signed_urls: bool,
    /// Decision confirmation window.
    pub confirm_ms: u64,
    /// Cached `ResolveRef` lifetime.
    pub ref_ttl_ms: u64,
    /// Concurrent upstream requests.
    pub upstream_concurrency: usize,
    /// Upstream retries.
    pub max_retries: u32,
    /// Snapshot bootstrap repos.
    pub bootstrap: Vec<String>,
    /// Snapshot bootstrap prefixes.
    pub bootstrap_sparse: Vec<String>,
    /// Snapshot refresh interval in seconds.
    pub bootstrap_every_secs: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    daemon: Section,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Section {
    upstream: Option<String>,
    socket: Option<String>,
    listen: Option<String>,
    cache_dir: Option<String>,
    max_bytes: Option<u64>,
    low_watermark_pct: Option<u8>,
    fsync: Option<bool>,
    memory_bytes: Option<u64>,
    signed_urls: Option<bool>,
    confirm_ms: Option<u64>,
    ref_ttl_ms: Option<u64>,
    upstream_concurrency: Option<usize>,
    max_retries: Option<u32>,
    bootstrap: Option<Vec<String>>,
    bootstrap_sparse: Option<Vec<String>>,
    bootstrap_every_secs: Option<u64>,
}

impl Settings {
    /// The built-in defaults.
    pub fn defaults() -> Settings {
        let tier = TierConfig::new(TierKind::Daemon);
        let disk = DiskConfig::new(default_cache_dir());
        Settings {
            upstream: None,
            socket: default_socket_path(),
            listen: None,
            cache_dir: disk.root,
            max_bytes: disk.max_bytes,
            low_watermark_pct: disk.low_watermark_pct,
            fsync: disk.fsync,
            memory_bytes: 4 << 30,
            signed_urls: tier.signed_urls,
            confirm_ms: tier.confirm_ms,
            ref_ttl_ms: tier.ref_ttl_ms,
            upstream_concurrency: tier.upstream_concurrency,
            max_retries: tier.max_retries,
            bootstrap: Vec::new(),
            bootstrap_sparse: Vec::new(),
            bootstrap_every_secs: 3600,
        }
    }

    /// Defaults overlaid by a configuration file's text, then by the environment `env`.
    pub fn load(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Settings, String> {
        let mut value: toml::Value =
            toml::from_str(text).map_err(|e| format!("daemon config: {e}"))?;
        resolve(&mut value, "", env)?;
        let file: File = value
            .try_into()
            .map_err(|e: toml::de::Error| format!("daemon config: {e}"))?;
        let d = file.daemon;
        let mut s = Settings::defaults();
        if let Some(v) = d.upstream {
            s.upstream = Some(v);
        }
        if let Some(v) = d.socket {
            s.socket = PathBuf::from(v);
        }
        if let Some(v) = d.listen {
            s.listen = Some(v);
        }
        if let Some(v) = d.cache_dir {
            s.cache_dir = PathBuf::from(v);
        }
        s.max_bytes = d.max_bytes.unwrap_or(s.max_bytes);
        s.low_watermark_pct = d.low_watermark_pct.unwrap_or(s.low_watermark_pct);
        s.fsync = d.fsync.unwrap_or(s.fsync);
        s.memory_bytes = d.memory_bytes.unwrap_or(s.memory_bytes);
        s.signed_urls = d.signed_urls.unwrap_or(s.signed_urls);
        s.confirm_ms = d.confirm_ms.unwrap_or(s.confirm_ms);
        s.ref_ttl_ms = d.ref_ttl_ms.unwrap_or(s.ref_ttl_ms);
        s.upstream_concurrency = d.upstream_concurrency.unwrap_or(s.upstream_concurrency);
        s.max_retries = d.max_retries.unwrap_or(s.max_retries);
        s.bootstrap = d.bootstrap.unwrap_or_default();
        s.bootstrap_sparse = d.bootstrap_sparse.unwrap_or_default();
        s.bootstrap_every_secs = d.bootstrap_every_secs.unwrap_or(s.bootstrap_every_secs);
        s.apply_env(env);
        s.validate()?;
        Ok(s)
    }

    /// Environment overrides: `BEE_ENDPOINT` names the upstream.
    pub fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        if let Some(v) = env("BEE_ENDPOINT").filter(|v| !v.is_empty()) {
            self.upstream = Some(v);
        }
    }

    /// Cross-key rules.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_bytes == 0 || self.memory_bytes == 0 {
            return Err("daemon config: max_bytes and memory_bytes must be positive".into());
        }
        if !(1..=100).contains(&self.low_watermark_pct) {
            return Err("daemon config: low_watermark_pct must be 1 to 100".into());
        }
        if self.upstream_concurrency == 0 {
            return Err("daemon config: upstream_concurrency must be at least 1".into());
        }
        if let Some(u) = &self.upstream
            && !(u.starts_with("http://") || u.starts_with("https://"))
        {
            return Err(format!(
                "daemon config: upstream {u:?} is not an http(s) URL"
            ));
        }
        Ok(())
    }

    /// The disk cache settings.
    pub fn disk(&self) -> DiskConfig {
        let mut d = DiskConfig::new(self.cache_dir.clone());
        d.max_bytes = self.max_bytes;
        d.low_watermark_pct = self.low_watermark_pct;
        d.fsync = self.fsync;
        d
    }

    /// The tier settings for `kind`.
    pub fn tier(&self, kind: TierKind) -> TierConfig {
        let mut t = TierConfig::new(kind);
        t.signed_urls = self.signed_urls;
        t.confirm_ms = self.confirm_ms;
        t.ref_ttl_ms = self.ref_ttl_ms;
        t.upstream_concurrency = self.upstream_concurrency;
        t.max_retries = self.max_retries;
        t
    }
}

fn resolve(
    v: &mut toml::Value,
    key: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    match v {
        toml::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("$$") {
                *s = format!("${rest}");
            } else if let Some(name) = var_name(s) {
                *s = env(&name).ok_or_else(|| {
                    format!("daemon config: {key}: environment variable {name} is not set")
                })?;
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                resolve(item, key, env)?;
            }
        }
        toml::Value::Table(t) => {
            for (k, item) in t.iter_mut() {
                let path = if key.is_empty() {
                    k.clone()
                } else {
                    format!("{key}.{k}")
                };
                resolve(item, &path, env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn var_name(s: &str) -> Option<String> {
    let body = s.strip_prefix('$')?;
    let body = body
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .unwrap_or(body);
    (!body.is_empty() && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn an_empty_file_gives_the_defaults() {
        let s = Settings::load("", &none).unwrap();
        assert_eq!(s, Settings::defaults());
        assert_eq!(s.max_bytes, 10 << 30);
        assert_eq!(s.confirm_ms, 5_000);
        assert_eq!(s.upstream_concurrency, 32);
        assert!(s.fsync && s.signed_urls);
    }

    #[test]
    fn every_key_is_read() {
        let text = r#"
            [daemon]
            upstream = "https://bee.example"
            socket = "/run/bee.sock"
            listen = "0.0.0.0:7430"
            cache_dir = "/var/cache/bee"
            max_bytes = 1000000
            low_watermark_pct = 80
            fsync = false
            memory_bytes = 2000000
            signed_urls = false
            confirm_ms = 250
            ref_ttl_ms = 50
            upstream_concurrency = 4
            max_retries = 1
            bootstrap = ["acme/app", "acme/lib"]
            bootstrap_sparse = ["src"]
            bootstrap_every_secs = 600
        "#;
        let s = Settings::load(text, &none).unwrap();
        assert_eq!(s.upstream.as_deref(), Some("https://bee.example"));
        assert_eq!(s.socket, PathBuf::from("/run/bee.sock"));
        assert_eq!(s.listen.as_deref(), Some("0.0.0.0:7430"));
        let d = s.disk();
        assert_eq!(d.root, PathBuf::from("/var/cache/bee"));
        assert_eq!(
            (d.max_bytes, d.low_watermark_pct, d.fsync),
            (1_000_000, 80, false)
        );
        assert_eq!(s.memory_bytes, 2_000_000);
        let t = s.tier(TierKind::Regional);
        assert_eq!(t.kind, TierKind::Regional);
        assert!(!t.signed_urls);
        assert_eq!(
            (
                t.confirm_ms,
                t.ref_ttl_ms,
                t.upstream_concurrency,
                t.max_retries
            ),
            (250, 50, 4, 1)
        );
        assert_eq!(s.bootstrap, vec!["acme/app", "acme/lib"]);
        assert_eq!(s.bootstrap_sparse, vec!["src"]);
        assert_eq!(s.bootstrap_every_secs, 600);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_errors() {
        let e = Settings::load("[daemon]\nmax_byte = 1\n", &none).unwrap_err();
        assert!(e.contains("max_byte"), "{e}");
        let e = Settings::load("[cache]\nmax_bytes = 1\n", &none).unwrap_err();
        assert!(e.contains("cache"), "{e}");
        assert!(Settings::load("[daemon]\nmax_bytes = \"big\"\n", &none).is_err());
        assert!(Settings::load("[daemon]\nlow_watermark_pct = 0\n", &none).is_err());
        assert!(Settings::load("[daemon]\nupstream_concurrency = 0\n", &none).is_err());
        assert!(Settings::load("[daemon]\nupstream = \"ftp://x\"\n", &none).is_err());
    }

    #[test]
    fn environment_values_and_overrides() {
        let env = |name: &str| match name {
            "CACHE" => Some("/tmp/c".to_string()),
            "BEE_ENDPOINT" => Some("https://override.example".to_string()),
            _ => None,
        };
        let s = Settings::load(
            "[daemon]\ncache_dir = \"${CACHE}\"\nsocket = \"$$literal\"\nupstream = \"https://file.example\"\n",
            &env,
        )
        .unwrap();
        assert_eq!(s.cache_dir, PathBuf::from("/tmp/c"));
        assert_eq!(s.socket, PathBuf::from("$literal"));
        // BEE_ENDPOINT beats the file.
        assert_eq!(s.upstream.as_deref(), Some("https://override.example"));
        let e = Settings::load("[daemon]\ncache_dir = \"$NOPE\"\n", &none).unwrap_err();
        assert!(e.contains("NOPE") && e.contains("daemon.cache_dir"), "{e}");
    }
}
