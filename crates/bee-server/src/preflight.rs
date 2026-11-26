//! Operational validation before production (PRODUCT §4.22, SPEC §15.6): the checklist a
//! configuration must pass before a cell serves real traffic. `bee-server --config FILE
//! --check-production` prints it and exits 1 on any failed error-level item;
//! `OpsService.ValidateOpsConfig` with `component = "production"` returns the same items as
//! errors and warnings. Every item is a pure function of the configuration.

use crate::config::{Config, Edition};

/// How a failed item counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Production must not start with it failing.
    Error,
    /// Allowed, but an operator should know.
    Warning,
}

/// One checklist item and its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Stable id, e.g. `metadata_postgres`.
    pub id: &'static str,
    /// The configuration key it is about.
    pub key_path: &'static str,
    /// Error or warning when it fails.
    pub severity: Severity,
    /// Whether it passed.
    pub ok: bool,
    /// What is required and why.
    pub message: String,
}

fn check(
    id: &'static str,
    key_path: &'static str,
    severity: Severity,
    ok: bool,
    message: &str,
) -> Check {
    Check {
        id,
        key_path,
        severity,
        ok,
        message: message.to_string(),
    }
}

/// The production checklist for `cfg`, in a fixed order.
pub fn production_checklist(cfg: &Config) -> Vec<Check> {
    use Severity::{Error, Warning};
    let tls = !cfg.tls.cert_file.is_empty()
        || (cfg.server.public_url.starts_with("https://") && cfg.server.trust_forwarded_for);
    let backup_target = &cfg.backup.target;
    let backup_ok = backup_target == "s3"
        || backup_target
            .strip_prefix("dir:")
            .is_some_and(|d| !std::path::Path::new(d).starts_with(&cfg.server.data_dir));
    // Who can sign in: a cloud edition needs a way for people to log in; an enterprise one
    // needs single sign-on (external OIDC; SAML is stored with the enterprise, not in this
    // file) or at least one operator who can configure it.
    let login = &cfg.login;
    let oidc = !login.oidc_issuer.is_empty();
    let edition_check = match cfg.server.edition {
        Edition::Cloud => check(
            "edition_login",
            "login.github_client_id",
            Error,
            !login.github_client_id.is_empty() || oidc,
            "the cloud edition needs a login method: the GitHub app (login.github_client_id) or OIDC (login.oidc_issuer)",
        ),
        Edition::Enterprise => check(
            "edition_sso",
            "login.oidc_issuer",
            Error,
            oidc || !cfg.ops.operators.is_empty() || !cfg.forge.site_admins.is_empty(),
            "the enterprise edition needs single sign-on (login.oidc_issuer) or at least one site operator (ops.operators or forge.site_admins) to set it up",
        ),
    };
    vec![
        check(
            "metadata_postgres",
            "metadata.backend",
            Error,
            cfg.metadata.backend == "postgres",
            "metadata must be Postgres: SQLite serves one process on one machine (SPEC §6.1)",
        ),
        check(
            "metadata_replica",
            "metadata.postgres_replica_url",
            Warning,
            !cfg.metadata.postgres_replica_url.is_empty(),
            "a read replica keeps immutable reads off the primary and serving while it is down (SPEC §15.1)",
        ),
        check(
            "stale_reads",
            "metadata.stale_read_max_ms",
            Warning,
            cfg.metadata.stale_read_max_ms > 0,
            "with stale reads off, ref reads fail during a primary outage instead of answering stale (SPEC §15.1)",
        ),
        check(
            "blobs_s3",
            "blobs.backend",
            Error,
            cfg.blobs.backend == "s3",
            "blobs must be in an S3-compatible store shared by every process; disk storage is local to one machine (SPEC §6.1)",
        ),
        check(
            "tls",
            "tls.cert_file",
            Error,
            tls,
            "serve TLS (tls.cert_file), or sit behind a TLS proxy with an https public_url and trust_forwarded_for (SPEC §12.1)",
        ),
        check(
            "signing_key",
            "auth.signing_key_hex",
            Error,
            !cfg.auth.signing_key_hex.is_empty(),
            "set the token signing key (from the environment) so every process verifies the same tokens; a generated key lives in one data directory",
        ),
        check(
            "url_key",
            "blobs.url_key_hex",
            Error,
            !cfg.blobs.url_key_hex.is_empty(),
            "signed URLs need one shared key across processes, including S3 transfer and media URLs",
        ),
        check(
            "oidc_signing_key",
            "auth.oidc_key_pkcs8_hex",
            Warning,
            !cfg.auth.oidc_key_pkcs8_hex.is_empty(),
            "set the OIDC signing key so every replica publishes and signs with the same key",
        ),
        check(
            "operators",
            "ops.operators",
            Error,
            !cfg.ops.operators.is_empty() || !cfg.forge.site_admins.is_empty(),
            "name the operators allowed on server-wide RPCs (SPEC §9.7)",
        ),
        check(
            "rate_limits",
            "admission.per_principal_rps",
            Error,
            cfg.admission.per_principal_rps > 0 && cfg.admission.per_root_rps > 0,
            "rate quotas per principal and per delegation root must be on (SPEC §15.4)",
        ),
        check(
            "import_roots",
            "import.allowed_roots",
            Error,
            !cfg.import.allowed_roots.iter().any(|r| r == "/"),
            "local-path imports must not read the whole filesystem",
        ),
        check(
            "local_runner",
            "forge.local_runner.enabled",
            Error,
            !cfg.forge.local_runner.enabled.unwrap_or(false),
            "forge.local_runner runs workflow steps as processes on the server host; use registered runners in production",
        ),
        check(
            "backups",
            "backup.target",
            Error,
            backup_ok,
            "backups go to the S3 store or a directory outside the data directory, so losing the machine does not lose them (PRODUCT §4.3)",
        ),
        check(
            "logging",
            "logging.level",
            Warning,
            cfg.logging.level != "debug",
            "debug logging is verbose and slow under load",
        ),
        edition_check,
    ]
}

/// True when no error-level item failed.
pub fn passes(checks: &[Check]) -> bool {
    checks
        .iter()
        .all(|c| c.ok || c.severity == Severity::Warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(checks: &[Check]) -> Vec<&'static str> {
        checks.iter().filter(|c| !c.ok).map(|c| c.id).collect()
    }

    #[test]
    fn dev_and_default_configs_are_not_production_ready() {
        let mut dev = Config::dev("/tmp/bee-dev".into(), None);
        dev.resolve_local_runner(true);
        let checks = production_checklist(&dev);
        assert!(!passes(&checks));
        let f = failed(&checks);
        for id in [
            "metadata_postgres",
            "blobs_s3",
            "tls",
            "signing_key",
            "import_roots",
            "backups",
        ] {
            assert!(f.contains(&id), "{id} fails in dev: {f:?}");
        }
        assert!(!passes(&production_checklist(&Config::default())));
        assert!(failed(&production_checklist(&dev)).contains(&"local_runner"));
    }

    #[test]
    fn local_runner_is_refused_in_production() {
        let mut cfg = Config::default();
        cfg.forge.local_runner.enabled = Some(true);
        let item = production_checklist(&cfg)
            .into_iter()
            .find(|item| item.id == "local_runner")
            .expect("local runner check");
        assert_eq!(item.severity, Severity::Error);
        assert!(!item.ok);
        assert_eq!(
            item.message,
            "forge.local_runner runs workflow steps as processes on the server host; use registered runners in production"
        );
    }

    #[test]
    fn a_production_config_passes_and_warnings_do_not_block() {
        let text = r#"
            [server]
            public_url = "https://bee.example"
            trust_forwarded_for = true
            data_dir = "/var/lib/bee"
            [metadata]
            backend = "postgres"
            postgres_url = "postgres://db/bee"
            [blobs]
            backend = "s3"
            s3_bucket = "bee"
            [auth]
            signing_key_hex = "1111111111111111111111111111111111111111111111111111111111111111"
            [ops]
            operators = ["01J0OPERATOR"]
            [backup]
            target = "s3"
        "#;
        let mut cfg = Config::parse_with_env(text, &|_| None).expect("config");
        // The checklist requires shared values but does not parse or use their material.
        cfg.blobs.url_key_hex = "configured".into();
        cfg.auth.oidc_key_pkcs8_hex = "configured".into();
        let checks = production_checklist(&cfg);
        assert!(passes(&checks), "{:?}", failed(&checks));
        // Only the replica warning is left.
        assert_eq!(failed(&checks), vec!["metadata_replica"]);
        // Each error item flips the outcome on its own.
        let mut off = cfg.clone();
        off.admission.per_root_rps = 0;
        assert_eq!(
            failed(&production_checklist(&off)),
            vec!["metadata_replica", "rate_limits"]
        );
        assert!(!passes(&production_checklist(&off)));
        let mut local = cfg.clone();
        local.backup.target = "dir:/var/lib/bee/backups".into();
        assert!(!passes(&production_checklist(&local)));
        local.backup.target = "dir:/mnt/backups".into();
        assert!(passes(&production_checklist(&local)));
    }

    #[test]
    fn s3_requires_a_url_key_and_warns_without_an_oidc_key() {
        let mut cfg = Config::dev("/tmp/bee-preflight".into(), None);
        cfg.metadata.backend = "postgres".into();
        cfg.blobs.backend = "s3".into();
        let failures = failed(&production_checklist(&cfg));
        assert!(failures.contains(&"url_key"));
        assert!(failures.contains(&"oidc_signing_key"));
        cfg.blobs.url_key_hex = "11".repeat(32);
        cfg.auth.oidc_key_pkcs8_hex = "22".repeat(32);
        let failures = failed(&production_checklist(&cfg));
        assert!(!failures.contains(&"url_key"));
        assert!(!failures.contains(&"oidc_signing_key"));
    }

    #[test]
    fn the_edition_check_asks_for_what_that_edition_needs() {
        let mut cfg = Config::default();
        // Enterprise (the default): SSO or an operator.
        assert_eq!(cfg.server.edition, Edition::Enterprise);
        assert!(failed(&production_checklist(&cfg)).contains(&"edition_sso"));
        cfg.ops.operators = vec!["01J0OPERATOR".into()];
        assert!(!failed(&production_checklist(&cfg)).contains(&"edition_sso"));
        cfg.ops.operators.clear();
        cfg.forge.site_admins = vec!["01J0ADMIN".into()];
        assert!(!failed(&production_checklist(&cfg)).contains(&"edition_sso"));
        cfg.forge.site_admins.clear();
        cfg.login.oidc_issuer = "https://idp.example".into();
        assert!(!failed(&production_checklist(&cfg)).contains(&"edition_sso"));

        // Cloud: a login method, operators alone do not count.
        let mut cloud = Config::default();
        cloud.server.edition = Edition::Cloud;
        cloud.ops.operators = vec!["01J0OPERATOR".into()];
        let f = failed(&production_checklist(&cloud));
        assert!(
            f.contains(&"edition_login") && !f.contains(&"edition_sso"),
            "{f:?}"
        );
        cloud.login.github_client_id = "Iv1.app".into();
        assert!(!failed(&production_checklist(&cloud)).contains(&"edition_login"));
        cloud.login.github_client_id.clear();
        cloud.login.oidc_issuer = "https://idp.example".into();
        assert!(!failed(&production_checklist(&cloud)).contains(&"edition_login"));
    }
}
