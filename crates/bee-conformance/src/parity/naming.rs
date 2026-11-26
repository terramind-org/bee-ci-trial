//! The names an RPC has in each form (SPEC §12.4), derived here from the SPEC rules alone so
//! the suite checks the CLI and MCP generators instead of reusing them.
//!
//! - CLI: `bee <noun> <verb>`: the noun is the service name without `Service`, the verb the
//!   method name, both in kebab-case with acronyms kept together (`WhoAmI` -> `who-am-i`,
//!   `GetSBOMExport` -> `get-sbom-export`).
//! - CLI output schema id: `bee.v1.<noun>.<verb>`.
//! - MCP: one tool per RPC named `<service>_<method>` in snake_case, `Service` dropped.

/// Splits a CamelCase identifier into lowercase words, keeping acronyms together and starting
/// a new word after a digit run only before an uppercase letter.
pub fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        let boundary = c.is_ascii_uppercase()
            && i > 0
            && !current.is_empty()
            && match chars[i - 1] {
                p if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                p if p.is_ascii_uppercase() => {
                    chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase())
                }
                _ => false,
            };
        if boundary {
            out.push(std::mem::take(&mut current));
        }
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `bee.v1.IssueService` -> `Issue`.
pub fn service_short(service: &str) -> &str {
    let short = service.rsplit('.').next().unwrap_or(service);
    short.strip_suffix("Service").unwrap_or(short)
}

/// The CLI noun of a service: `bee.v1.WriteService` -> `write`.
pub fn cli_noun(service: &str) -> String {
    words(service_short(service)).join("-")
}

/// The CLI verb of a method: `LandChange` -> `land-change`.
pub fn cli_verb(method: &str) -> String {
    words(method).join("-")
}

/// The `schema` field of a CLI document: `bee.v1.<noun>.<verb>`.
pub fn cli_schema_id(service: &str, method: &str) -> String {
    format!("bee.v1.{}.{}", cli_noun(service), cli_verb(method))
}

/// The file holding a CLI command's output schema, relative to the repository root.
pub fn cli_schema_file(service: &str, method: &str) -> String {
    format!(
        "schemas/v1/cli/{}.{}.schema.json",
        cli_noun(service),
        cli_verb(method)
    )
}

/// The MCP tool of an RPC: `bee.v1.AuthService`, `WhoAmI` -> `auth_who_am_i`.
pub fn mcp_tool(service: &str, method: &str) -> String {
    format!(
        "{}_{}",
        words(service_short(service)).join("_"),
        words(method).join("_")
    )
}

/// The flag of a request field given by its proto name: `change_request_id` ->
/// `change-request-id`; names the CLI reserves for itself get a `-field` suffix.
pub fn cli_flag(field: &str) -> String {
    const RESERVED: &[&str] = &[
        "help",
        "json",
        "endpoint",
        "token",
        "timeout",
        "json-input",
        "input-file",
        "follow",
    ];
    let base = field.replace('_', "-");
    if RESERVED.contains(&base.as_str()) {
        format!("{base}-field")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acronyms_stay_together() {
        assert_eq!(cli_verb("WhoAmI"), "who-am-i");
        assert_eq!(cli_verb("GetSBOMExport"), "get-sbom-export");
        assert_eq!(cli_verb("GitLFSBatch"), "git-lfs-batch");
        assert_eq!(cli_verb("CreateOAuthApp"), "create-o-auth-app");
        assert_eq!(cli_verb("Get2FAStatus"), "get2-fa-status");
        assert_eq!(cli_noun("bee.v1.WriteService"), "write");
        assert_eq!(cli_noun("bee.v1.GitService"), "git");
    }

    #[test]
    fn tool_and_schema_names() {
        assert_eq!(mcp_tool("bee.v1.AuthService", "WhoAmI"), "auth_who_am_i");
        assert_eq!(
            mcp_tool("bee.v1.IssueService", "CreateIssue"),
            "issue_create_issue"
        );
        assert_eq!(
            cli_schema_id("bee.v1.ChangeService", "LandChange"),
            "bee.v1.change.land-change"
        );
        assert_eq!(
            cli_schema_file("bee.v1.QueryService", "Log"),
            "schemas/v1/cli/query.log.schema.json"
        );
        assert_eq!(cli_flag("change_request_id"), "change-request-id");
        assert_eq!(cli_flag("follow"), "follow-field");
    }
}
