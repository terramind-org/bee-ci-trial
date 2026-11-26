//! The `devcontainer.json` parser: JSON with comments and trailing commas, normalized into a
//! [`DevcontainerSpec`] (image or build, features, forwarded ports and their attributes,
//! lifecycle commands, environment, host requirements, mounts, secrets).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ForgeError, Result};

/// The default devcontainer path.
pub const DEFAULT_PATH: &str = ".devcontainer/devcontainer.json";
/// Image used when a repo has no devcontainer file.
pub const DEFAULT_IMAGE: &str = "bee/devcontainer-base:latest";

/// A forwarded port.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    /// Port number.
    pub port: u32,
    /// Label (`portsAttributes.<port>.label`).
    pub label: String,
    /// `http` or `https`.
    pub protocol: String,
    /// `notify`, `openBrowser`, `silent`, `ignore`, ...
    pub on_auto_forward: String,
}

/// Minimum machine size (`hostRequirements`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRequirements {
    /// CPUs.
    pub cpus: u32,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Storage in bytes.
    pub storage_bytes: u64,
    /// Needs a GPU.
    pub gpu: bool,
}

/// A normalized devcontainer configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevcontainerSpec {
    /// Display name.
    pub name: String,
    /// Image (when not built).
    pub image: String,
    /// Dockerfile (relative to the devcontainer file).
    pub dockerfile: String,
    /// Build context.
    pub build_context: String,
    /// Build arguments.
    pub build_args: BTreeMap<String, String>,
    /// Compose file(s) and service, for compose-based configs.
    pub docker_compose_files: Vec<String>,
    /// Compose service.
    pub service: String,
    /// Features with their options (JSON text).
    pub features: BTreeMap<String, String>,
    /// Forwarded ports.
    pub forward_ports: Vec<PortSpec>,
    /// `onCreateCommand`.
    pub on_create_command: Vec<String>,
    /// `updateContentCommand` (runs in prebuilds).
    pub update_content_command: Vec<String>,
    /// `postCreateCommand`.
    pub post_create_command: Vec<String>,
    /// `postStartCommand`.
    pub post_start_command: Vec<String>,
    /// `postAttachCommand`.
    pub post_attach_command: Vec<String>,
    /// Container environment.
    pub container_env: BTreeMap<String, String>,
    /// Remote (tool) environment.
    pub remote_env: BTreeMap<String, String>,
    /// User for tools.
    pub remote_user: String,
    /// Working folder inside the container.
    pub workspace_folder: String,
    /// Mounts.
    pub mounts: Vec<String>,
    /// Extra container runtime arguments.
    pub run_args: Vec<String>,
    /// Recommended secrets (names).
    pub secrets: Vec<String>,
    /// Host requirements.
    pub host_requirements: HostRequirements,
    /// `customizations` as JSON text.
    pub customizations: String,
}

/// Removes `//` and `/* */` comments outside strings and trailing commas before `]` or `}`.
pub fn strip_jsonc(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                out.push('"');
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                // Keep multi-byte UTF-8 intact: copy the whole character.
                let ch = src[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    // With comments gone, a trailing comma is followed only by whitespace and a bracket.
    strip_trailing_commas(&out)
}

fn strip_trailing_commas(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' && i + 1 < b.len() {
                out.push_str(&s[i..i + 2]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b',' {
            let mut j = i + 1;
            while j < b.len() && (b[j] as char).is_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b']' || b[j] == b'}') {
                i += 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parses `8gb`, `512mb`, `1tb`, `4096` (bytes) into bytes (binary units).
pub fn parse_size(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            let (num, mult) = [
                ("tb", 1u64 << 40),
                ("gb", 1 << 30),
                ("mb", 1 << 20),
                ("kb", 1 << 10),
            ]
            .iter()
            .find_map(|(u, m)| t.strip_suffix(u).map(|n| (n.trim().to_string(), *m)))
            .unwrap_or((t.clone(), 1));
            let f: f64 = num.parse().ok()?;
            (f >= 0.0).then_some((f * mult as f64) as u64)
        }
        _ => None,
    }
}

fn commands(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => {
            let parts: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![parts.join(" ")]
            }
        }
        Some(Value::Object(o)) => o
            .iter()
            .flat_map(|(k, v)| {
                commands(Some(v))
                    .into_iter()
                    .map(move |c| format!("{k}: {c}"))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    if let Some(Value::Object(o)) = v {
        for (k, v) in o {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            m.insert(k.clone(), s);
        }
    }
    m
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

impl DevcontainerSpec {
    /// The spec used when a repo has no devcontainer file.
    pub fn default_spec() -> DevcontainerSpec {
        DevcontainerSpec {
            name: "default".into(),
            image: DEFAULT_IMAGE.into(),
            ..DevcontainerSpec::default()
        }
    }

    /// Parses a devcontainer file.
    pub fn parse(text: &str) -> Result<DevcontainerSpec> {
        let bad = |m: String| ForgeError::invalid("invalid_devcontainer", m);
        let v: Value = serde_json::from_str(&strip_jsonc(text))
            .map_err(|e| bad(format!("devcontainer.json is not valid JSON: {e}")))?;
        let o = v
            .as_object()
            .ok_or_else(|| bad("devcontainer.json must be an object".into()))?;
        let s = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let mut spec = DevcontainerSpec {
            name: s("name"),
            image: s("image"),
            remote_user: s("remoteUser"),
            workspace_folder: s("workspaceFolder"),
            service: s("service"),
            docker_compose_files: strings(o.get("dockerComposeFile")),
            on_create_command: commands(o.get("onCreateCommand")),
            update_content_command: commands(o.get("updateContentCommand")),
            post_create_command: commands(o.get("postCreateCommand")),
            post_start_command: commands(o.get("postStartCommand")),
            post_attach_command: commands(o.get("postAttachCommand")),
            container_env: string_map(o.get("containerEnv")),
            remote_env: string_map(o.get("remoteEnv")),
            mounts: strings(o.get("mounts")),
            run_args: strings(o.get("runArgs")),
            customizations: o
                .get("customizations")
                .map(|c| c.to_string())
                .unwrap_or_default(),
            ..DevcontainerSpec::default()
        };
        if let Some(b) = o.get("build").and_then(|b| b.as_object()) {
            spec.dockerfile = b
                .get("dockerfile")
                .or_else(|| b.get("dockerFile"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            spec.build_context = b
                .get("context")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            spec.build_args = string_map(b.get("args"));
        } else if let Some(d) = o.get("dockerFile").and_then(|x| x.as_str()) {
            spec.dockerfile = d.to_string();
        }
        if spec.image.is_empty()
            && spec.dockerfile.is_empty()
            && spec.docker_compose_files.is_empty()
        {
            return Err(bad(
                "devcontainer.json needs an image, a build.dockerfile or a dockerComposeFile"
                    .into(),
            ));
        }
        if !spec.docker_compose_files.is_empty() && spec.service.is_empty() {
            return Err(bad("a compose-based devcontainer needs a service".into()));
        }
        if let Some(Value::Object(f)) = o.get("features") {
            for (k, v) in f {
                spec.features.insert(k.clone(), v.to_string());
            }
        }
        let attrs = o.get("portsAttributes").and_then(|a| a.as_object());
        for p in o
            .get("forwardPorts")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let port = match &p {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.rsplit(':').next().and_then(|x| x.parse().ok()),
                _ => None,
            }
            .filter(|n| (1..=65_535).contains(n))
            .ok_or_else(|| bad(format!("bad forwarded port {p}")))? as u32;
            let key = port.to_string();
            let a = attrs.and_then(|a| a.get(&key)).and_then(|a| a.as_object());
            let get = |k: &str| {
                a.and_then(|a| a.get(k))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            if spec.forward_ports.iter().any(|x| x.port == port) {
                continue;
            }
            let protocol = get("protocol");
            spec.forward_ports.push(PortSpec {
                port,
                label: get("label"),
                protocol: if protocol.is_empty() {
                    "http".into()
                } else {
                    protocol
                },
                on_auto_forward: get("onAutoForward"),
            });
        }
        if let Some(h) = o.get("hostRequirements").and_then(|h| h.as_object()) {
            spec.host_requirements = HostRequirements {
                cpus: h.get("cpus").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                memory_bytes: h.get("memory").and_then(parse_size).unwrap_or(0),
                storage_bytes: h.get("storage").and_then(parse_size).unwrap_or(0),
                gpu: match h.get("gpu") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(s)) => s == "true" || s == "optional",
                    Some(Value::Object(_)) => true,
                    _ => false,
                },
            };
        }
        // Codespaces-style recommended secrets: `"secrets": {"NAME": {...}}`.
        if let Some(Value::Object(sec)) = o.get("secrets") {
            spec.secrets = sec.keys().cloned().collect();
        }
        Ok(spec)
    }

    /// The spec as stored JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A stored spec.
    pub fn from_json(s: &str) -> DevcontainerSpec {
        serde_json::from_str(s).unwrap_or_else(|_| DevcontainerSpec::default_spec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_with_everything() {
        let text = r#"{
            // A comment
            "name": "app",
            "build": { "dockerfile": "Dockerfile", "context": "..", "args": {"V": "1"} },
            "features": { "ghcr.io/devcontainers/features/node:1": { "version": "20" } },
            "forwardPorts": [3000, "db:5432", 3000],
            "portsAttributes": { "3000": { "label": "web", "onAutoForward": "notify" } },
            /* block
               comment */
            "postCreateCommand": ["npm", "ci"],
            "postStartCommand": { "server": "npm start", "watch": "npm run watch" },
            "containerEnv": { "A": "b", "N": 3 },
            "hostRequirements": { "cpus": 4, "memory": "8gb", "storage": "32gb" },
            "secrets": { "API_KEY": { "description": "x" } },
            "remoteUser": "dev",
            "url": "http://example.com//not-a-comment",
        }"#;
        let s = DevcontainerSpec::parse(text).unwrap();
        assert_eq!(s.name, "app");
        assert_eq!(s.dockerfile, "Dockerfile");
        assert_eq!(s.build_args["V"], "1");
        assert_eq!(s.forward_ports.len(), 2);
        assert_eq!(s.forward_ports[0].label, "web");
        assert_eq!(s.forward_ports[1].port, 5432);
        assert_eq!(s.post_create_command, vec!["npm ci".to_string()]);
        assert_eq!(s.post_start_command.len(), 2);
        assert_eq!(s.container_env["N"], "3");
        assert_eq!(s.host_requirements.cpus, 4);
        assert_eq!(s.host_requirements.memory_bytes, 8 << 30);
        assert_eq!(s.secrets, vec!["API_KEY".to_string()]);
        assert_eq!(s.remote_user, "dev");
        assert_eq!(DevcontainerSpec::from_json(&s.to_json()), s);
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(DevcontainerSpec::parse("{}").is_err());
        assert!(DevcontainerSpec::parse("[1]").is_err());
        assert!(DevcontainerSpec::parse("{\"image\": \"x\", \"forwardPorts\": [70000]}").is_err());
        assert!(DevcontainerSpec::parse("{\"dockerComposeFile\": \"c.yml\"}").is_err());
        assert!(DevcontainerSpec::parse("{\"image\": \"x\",}").is_ok());
        assert_eq!(parse_size(&Value::String("1.5gb".into())), Some(3 << 29));
    }
}
