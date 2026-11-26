//! Structured JSON logs on stderr (PRODUCT §4.22).
//!
//! Every line is one JSON object with the required fields `ts_ms`, `level`, `msg` and
//! `component`, plus explicit extra fields. Callers never pass raw repository paths, file
//! contents, tokens or secrets: paths are logged as [`path_digest`], principals by id.

use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::{Map, Value};

static LEVEL: AtomicU8 = AtomicU8::new(2);

/// Log levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Errors.
    Error = 0,
    /// Warnings.
    Warn = 1,
    /// Information.
    Info = 2,
    /// Debugging detail.
    Debug = 3,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    /// Parses a config value.
    pub fn parse(s: &str) -> Level {
        match s {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "debug" => Level::Debug,
            _ => Level::Info,
        }
    }
}

/// Sets the process-wide level.
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// A digest that identifies a path in logs without revealing it.
pub fn path_digest(path: &[u8]) -> String {
    let h = blake3::hash(path);
    bee_core::hex::encode(&h.as_bytes()[..8])
}

/// Formats one log line (exposed for tests).
pub fn line(
    level: Level,
    component: &str,
    msg: &str,
    fields: &[(&str, Value)],
    ts_ms: u64,
) -> String {
    let mut m = Map::new();
    m.insert("ts_ms".into(), Value::from(ts_ms));
    m.insert("level".into(), Value::from(level.as_str()));
    m.insert("component".into(), Value::from(component));
    m.insert("msg".into(), Value::from(msg));
    for (k, v) in fields {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m).to_string()
}

/// Writes a log line if `level` is enabled, and keeps it in the in-memory ring that
/// `OpsService.QueryOpsLogs` and `TailOpsLogs` read.
pub fn log(level: Level, component: &str, msg: &str, fields: &[(&str, Value)]) {
    if (level as u8) > LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    eprintln!("{}", line(level, component, msg, fields, ts));
    let rec = LogRecord {
        ts_ms: ts,
        level,
        component: component.to_string(),
        msg: msg.to_string(),
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    };
    if let Ok(mut r) = RING.lock() {
        if r.len() >= RING_CAPACITY {
            r.pop_front();
        }
        r.push_back(rec.clone());
    }
    let _ = tail().send(rec);
}

/// Entries kept for log queries.
pub const RING_CAPACITY: usize = 10_000;

/// One kept log entry.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    /// Time.
    pub ts_ms: u64,
    /// Level.
    pub level: Level,
    /// Component.
    pub component: String,
    /// Message.
    pub msg: String,
    /// Extra fields (never raw paths or secrets, as for every line).
    pub fields: Vec<(String, Value)>,
}

static RING: std::sync::Mutex<std::collections::VecDeque<LogRecord>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static TAIL: std::sync::OnceLock<tokio::sync::broadcast::Sender<LogRecord>> =
    std::sync::OnceLock::new();

fn tail() -> &'static tokio::sync::broadcast::Sender<LogRecord> {
    TAIL.get_or_init(|| tokio::sync::broadcast::channel(1024).0)
}

/// The kept entries, oldest first.
pub fn recent() -> Vec<LogRecord> {
    RING.lock()
        .map(|r| r.iter().cloned().collect())
        .unwrap_or_default()
}

/// Entries logged from now on.
pub fn subscribe() -> tokio::sync::broadcast::Receiver<LogRecord> {
    tail().subscribe()
}

impl Level {
    /// The level's name (`error`, `warn`, `info`, `debug`).
    pub fn name(self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_json_with_required_fields() {
        let l = line(
            Level::Info,
            "rpc",
            "done",
            &[("path", Value::from(path_digest(b"secret/dir/file")))],
            5,
        );
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["level"], "info");
        assert_eq!(v["component"], "rpc");
        assert_eq!(v["ts_ms"], 5);
        assert!(!l.contains("secret/dir"));
        assert_eq!(v["path"].as_str().unwrap().len(), 16);
        assert_eq!(Level::parse("debug"), Level::Debug);
    }
}
