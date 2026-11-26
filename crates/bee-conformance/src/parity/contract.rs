//! Contract facts the suite checks every form against: which RPCs the server serves, and the
//! CLI's exit code for each outcome (SPEC §12.4, PRODUCT §3.2).

use bee_proto::methods::{METHODS, MethodDescriptor};

/// The CLI exit code for an outcome: 0 for success; for server errors 3 for
/// `MERGE_CONFLICT`, 5 for the policy refusals (`POLICY_DENIED`, `LEASED`,
/// `SECRET_DETECTED`, `BUDGET_EXHAUSTED`) and 1 otherwise.
pub fn cli_exit_code(reason: Option<&str>) -> i32 {
    match reason {
        None => 0,
        Some("MERGE_CONFLICT") => 3,
        Some("POLICY_DENIED" | "LEASED" | "SECRET_DETECTED" | "BUDGET_EXHAUSTED") => 5,
        Some(_) => 1,
    }
}

/// The documented UNIMPLEMENTED allow-list, read from the source of the server test that
/// enforces it (`crates/bee-server/tests/forge_services.rs`): whole services
/// (`UNREGISTERED`) and single RPC paths (`UNIMPLEMENTED_RPCS`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowList {
    /// Fully qualified services that are not registered.
    pub services: Vec<String>,
    /// RPC paths (`/bee.v1.S/M`) that answer UNIMPLEMENTED.
    pub rpcs: Vec<String>,
}

impl AllowList {
    /// Parses the two `&[&str]` constants out of the test source.
    pub fn parse(source: &str) -> Result<Self, String> {
        Ok(Self {
            services: string_array(source, "UNREGISTERED")?,
            rpcs: string_array(source, "UNIMPLEMENTED_RPCS")?,
        })
    }

    /// True when the server serves `m`.
    pub fn serves(&self, m: &MethodDescriptor) -> bool {
        !self.services.iter().any(|s| s == m.service) && !self.rpcs.iter().any(|p| p == m.path)
    }

    /// Every served RPC, in descriptor order.
    pub fn served(&self) -> Vec<&'static MethodDescriptor> {
        METHODS.iter().filter(|m| self.serves(m)).collect()
    }
}

/// The string literals of `const <name>: &[&str] = &[ ... ];`, skipping `//` comments.
fn string_array(source: &str, name: &str) -> Result<Vec<String>, String> {
    let marker = format!("const {name}: &[&str] = &[");
    let start = source
        .find(&marker)
        .ok_or_else(|| format!("{name} not found"))?
        + marker.len();
    let end = source[start..]
        .find("];")
        .ok_or_else(|| format!("{name} is not closed"))?
        + start;
    let mut out = Vec::new();
    for line in source[start..end].lines() {
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let close = after
                .find('"')
                .ok_or_else(|| format!("{name}: unterminated string"))?;
            out.push(after[..close].to_owned());
            rest = &after[close + 1..];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_allow_list() {
        let src = r#"
pub const UNREGISTERED: &[&str] = &[
    "bee.v1.SearchService",
];
pub const UNIMPLEMENTED_RPCS: &[&str] = &[
    // "commented" text is skipped
    "/bee.v1.AdminService/CreateBackup",
];
"#;
        let a = AllowList::parse(src).expect("parses");
        assert_eq!(a.services, vec!["bee.v1.SearchService"]);
        assert_eq!(a.rpcs, vec!["/bee.v1.AdminService/CreateBackup"]);
        let served = a.served();
        assert!(served.iter().all(|m| m.service != "bee.v1.SearchService"));
        assert!(
            served
                .iter()
                .any(|m| m.path == "/bee.v1.IssueService/CreateIssue")
        );
        assert!(AllowList::parse("nothing").is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(cli_exit_code(None), 0);
        assert_eq!(cli_exit_code(Some("MERGE_CONFLICT")), 3);
        assert_eq!(cli_exit_code(Some("LEASED")), 5);
        assert_eq!(cli_exit_code(Some("NOT_FOUND")), 1);
    }
}
