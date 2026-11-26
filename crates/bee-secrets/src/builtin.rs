//! The built-in rule set.
//!
//! Every rule names a provider, a regex in byte mode with ASCII semantics, a keyword prefilter,
//! an entropy floor where the shape alone is weak, an `example` that must match and a `near_miss`
//! that must not. Examples are fake: AWS's documented `EXAMPLE` keys, or deterministic junk made
//! by [`junk`], never real credentials.

use crate::rule::{Confidence, Rule, RuleSpec};

const B62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const B64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const HEX: &[u8] = b"0123456789abcdef";

/// `n` deterministic pseudo-random characters from `alphabet` (a fixed LCG, so examples are stable).
fn junk(alphabet: &[u8], n: usize, seed: u64) -> String {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            alphabet[((state >> 33) % alphabet.len() as u64) as usize] as char
        })
        .collect()
}

struct Def {
    id: &'static str,
    provider: &'static str,
    description: &'static str,
    regex: &'static str,
    keywords: &'static [&'static str],
    entropy: Option<f64>,
    confidence: Confidence,
    example: String,
    near_miss: String,
}

fn defs() -> Vec<Def> {
    use Confidence::{High, Low, Medium};
    let d = |id,
             provider,
             description,
             regex,
             keywords,
             entropy,
             confidence,
             example: String,
             near_miss: &str| Def {
        id,
        provider,
        description,
        regex,
        keywords,
        entropy,
        confidence,
        example,
        near_miss: near_miss.to_owned(),
    };
    let pem_body = junk(B64, 64, 11);
    vec![
        d(
            "aws-access-key-id",
            "aws",
            "AWS access key id (AKIA/ASIA/ABIA/ACCA prefix, 20 characters)",
            r"\b((?:AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16})\b",
            &["akia", "asia", "abia", "acca"],
            Some(2.5),
            High,
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE".into(),
            "key = AKIAIOSFODNN7EXAMPL and asia-pacific",
        ),
        d(
            "aws-secret-access-key",
            "aws",
            "AWS secret access key assigned next to an `aws` identifier",
            r##"(?mi)aws[a-z0-9_\-. ]{0,30}?(?:secret|private)[a-z0-9_\-. ]{0,20}?["']?[ \t]*[:=][ \t]*["']?([A-Za-z0-9/+=]{40})(?:[^A-Za-z0-9/+=]|$)"##,
            &["aws"],
            Some(3.5),
            High,
            r#"aws_secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY""#.into(),
            r#"aws_secret_access_key = "wJalrXUtnFEMI/K7MDENG""#,
        ),
        d(
            "github-token",
            "github",
            "GitHub personal, OAuth, user-to-server, server-to-server and refresh tokens (ghp_, gho_, ghu_, ghs_, ghr_)",
            r"\b(gh[pousr]_[A-Za-z0-9]{36})\b",
            &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
            Some(3.0),
            High,
            format!("token: ghp_{}", junk(B62, 36, 1)),
            "ghp_tooshort0123456789",
        ),
        d(
            "github-fine-grained-pat",
            "github",
            "GitHub fine-grained personal access token (github_pat_)",
            r"\b(github_pat_[A-Za-z0-9_]{82})\b",
            &["github_pat_"],
            Some(3.0),
            High,
            format!("github_pat_{}", junk(B62, 82, 2)),
            "github_pat_short",
        ),
        d(
            "gitlab-pat",
            "gitlab",
            "GitLab personal access token (glpat-)",
            r"(?m)\b(glpat-[A-Za-z0-9_\-]{20})(?:[^A-Za-z0-9_\-]|$)",
            &["glpat-"],
            Some(3.0),
            High,
            format!("GITLAB_TOKEN=glpat-{}", junk(B62, 20, 3)),
            "glpat-short",
        ),
        d(
            "slack-token",
            "slack",
            "Slack bot, user, app, refresh or legacy token (xox?-)",
            r"\b(xox[abeoprs]-(?:[0-9]{8,13}-){1,3}[A-Za-z0-9]{16,48})\b",
            &["xox"],
            Some(3.0),
            High,
            format!("xoxb-123456789012-1234567890123-{}", junk(B62, 24, 4)),
            "xoxb-1234-abcd",
        ),
        d(
            "slack-webhook",
            "slack",
            "Slack incoming webhook URL",
            r"(https://hooks\.slack\.com/services/T[A-Za-z0-9]{8,12}/B[A-Za-z0-9]{8,12}/[A-Za-z0-9]{24})",
            &["hooks.slack.com"],
            None,
            High,
            concat!(
                "https://hooks.slack.com/",
                "services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX"
            )
            .into(),
            "https://hooks.slack.com/services/T0/B0/short",
        ),
        d(
            "stripe-live-key",
            "stripe",
            "Stripe live secret or restricted key",
            r"\b((?:sk|rk)_live_[0-9A-Za-z]{24,99})\b",
            &["_live_"],
            Some(3.0),
            High,
            format!("sk_live_{}", junk(B62, 24, 5)),
            "sk_live_short",
        ),
        d(
            "stripe-test-key",
            "stripe",
            "Stripe test-mode secret or restricted key",
            r"\b((?:sk|rk)_test_[0-9A-Za-z]{24,99})\b",
            &["_test_"],
            Some(3.0),
            Medium,
            format!("rk_test_{}", junk(B62, 30, 6)),
            "sk_test_short",
        ),
        d(
            "google-api-key",
            "google",
            "Google API key (AIza prefix, 39 characters)",
            r"\b(AIza[0-9A-Za-z_\-]{35})(?:[^0-9A-Za-z_\-]|$)",
            &["aiza"],
            Some(3.0),
            High,
            format!("key=AIza{}", junk(B64URL, 35, 7)),
            "AIzaShort",
        ),
        d(
            "gcp-service-account",
            "google",
            "Google Cloud service-account JSON key file with an embedded private key",
            r#"(?s)("type"[ \t]*:[ \t]*"service_account".{0,4000}?"private_key"[ \t]*:[ \t]*"-----BEGIN [A-Z ]*PRIVATE KEY-----[^"]{16,})"#,
            &["service_account"],
            None,
            High,
            format!(
                "{{\"type\": \"service_account\", \"project_id\": \"p\", \"private_key\": \"-----BEGIN PRIVATE KEY-----\\n{pem_body}\\n-----END PRIVATE KEY-----\\n\"}}"
            ),
            r#"{"type": "service_account", "project_id": "p"}"#,
        ),
        d(
            "azure-storage-account-key",
            "azure",
            "Azure storage `AccountKey=` (88 base64 characters)",
            r"(?i)AccountKey=([A-Za-z0-9+/]{86}==)",
            &["accountkey"],
            Some(3.5),
            High,
            format!("AccountKey={}==", junk(B64, 86, 8)),
            "AccountKey=abc==",
        ),
        d(
            "azure-storage-connection-string",
            "azure",
            "Azure storage connection string with an account key",
            r#"(?i)(DefaultEndpointsProtocol=https?;[^\r\n"']{0,300}?AccountKey=[A-Za-z0-9+/]{86}==)"#,
            &["defaultendpointsprotocol"],
            Some(3.5),
            High,
            format!(
                "DefaultEndpointsProtocol=https;AccountName=demo;AccountKey={}==;EndpointSuffix=core.windows.net",
                junk(B64, 86, 9)
            ),
            "DefaultEndpointsProtocol=https;AccountName=demo",
        ),
        d(
            "openai-api-key",
            "openai",
            "OpenAI API key (sk-, sk-proj-, sk-svcacct-, sk-admin-)",
            r"(?m)\b(sk-(?:(?:proj|svcacct|admin)-[A-Za-z0-9_\-]{40,200}|[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20}|[A-Za-z0-9]{48}))(?:[^A-Za-z0-9_\-]|$)",
            &["sk-"],
            Some(3.5),
            High,
            format!("OPENAI_API_KEY=sk-proj-{}", junk(B64URL, 100, 10)),
            "sk-short and task-0123456789012345678901234567890123456789012345678",
        ),
        d(
            "anthropic-api-key",
            "anthropic",
            "Anthropic API or admin key (sk-ant-api03-, sk-ant-admin01-)",
            r"(?m)\b(sk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9_\-]{80,128})(?:[^A-Za-z0-9_\-]|$)",
            &["sk-ant-"],
            Some(3.5),
            High,
            format!("ANTHROPIC_API_KEY=sk-ant-api03-{}", junk(B64URL, 93, 12)),
            "sk-ant-api03-short",
        ),
        d(
            "npm-token",
            "npm",
            "npm automation or publish token (npm_)",
            r"\b(npm_[A-Za-z0-9]{36})\b",
            &["npm_"],
            Some(3.0),
            High,
            format!("//registry.npmjs.org/:_authToken=npm_{}", junk(B62, 36, 13)),
            "npm_short",
        ),
        d(
            "pypi-token",
            "pypi",
            "PyPI upload token (pypi-AgEIcHlwaS5vcmc)",
            r"\b(pypi-AgEIcHlwaS5vcmc[A-Za-z0-9_\-]{50,})",
            &["pypi-"],
            Some(3.5),
            High,
            format!("password = pypi-AgEIcHlwaS5vcmc{}", junk(B64URL, 70, 14)),
            "pypi-AgEIcHlwaS5vcmcshort",
        ),
        d(
            "private-key-pem",
            "generic",
            "PEM private key block (RSA, EC, DSA, OPENSSH, ENCRYPTED or PKCS#8) with a body",
            r"(-----BEGIN (?:(?:RSA|EC|DSA|OPENSSH|ENCRYPTED) )?PRIVATE KEY-----[A-Za-z0-9+/=\s\\:,.\-]{16,}?-----END (?:(?:RSA|EC|DSA|OPENSSH|ENCRYPTED) )?PRIVATE KEY-----)",
            &["private key"],
            None,
            High,
            format!("-----BEGIN RSA PRIVATE KEY-----\n{pem_body}\n-----END RSA PRIVATE KEY-----\n"),
            "-----BEGIN PUBLIC KEY-----\nMFwwDQYJKoZIhvcNAQEBBQADSwAwSAJBAL\n-----END PUBLIC KEY-----",
        ),
        d(
            "private-key-pgp",
            "generic",
            "PGP private key block",
            r"(-----BEGIN PGP PRIVATE KEY BLOCK-----[A-Za-z0-9+/=\s\\:,.\-]{16,}?-----END PGP PRIVATE KEY BLOCK-----)",
            &["pgp private key"],
            None,
            High,
            format!(
                "-----BEGIN PGP PRIVATE KEY BLOCK-----\n\n{pem_body}\n=abcd\n-----END PGP PRIVATE KEY BLOCK-----"
            ),
            "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQENBFakeFakeFakeFake\n-----END PGP PUBLIC KEY BLOCK-----",
        ),
        d(
            "jwt",
            "generic",
            "JSON Web Token with a high-entropy signature",
            r"\b(eyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,})",
            &["eyj"],
            Some(3.5),
            Medium,
            "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk5yJV_adQssw5c".into(),
            "eyJhbGciOiJub25lIn0.eyJhIjoxfQ.abc",
        ),
        d(
            "generic-secret-assignment",
            "generic",
            "password/secret/token/api_key assigned a high-entropy literal (heuristic)",
            r##"(?mi)(?:password|passwd|pwd|secret|token|api[_-]?key)[a-z0-9_.\-]{0,12}["']?[ \t]*(?::=|=>|[:=])[ \t]*(?:"([^\s"'\\$<{%][^\s"'\\]{7,99})"|'([^\s"'\\$<{%][^\s"'\\]{7,99})'|([A-Za-z0-9/+_\-!@#^&*]{12,100})(?:[ \t#;,]|$))"##,
            &["password", "passwd", "pwd", "secret", "token", "api_key", "api-key", "apikey"],
            Some(3.5),
            Low,
            r#"password = "q8Zr2vLx9Tm4Kp7W""#.into(),
            r#"password = "changeme" # token = "aaaaaaaaaaaa""#,
        ),
        d(
            "twilio-api-key-sid",
            "twilio",
            "Twilio API key SID (SK + 32 hex) near a `twilio` identifier",
            r"(?i)twilio[^\n]{0,80}?\b(SK[0-9a-f]{32})\b",
            &["twilio"],
            Some(2.5),
            High,
            format!("TWILIO_API_KEY=SK{}", junk(HEX, 32, 15)),
            "TWILIO_API_KEY=SK1234",
        ),
        d(
            "twilio-auth-token",
            "twilio",
            "Twilio auth token (32 hex) assigned to a `twilio` identifier",
            r##"(?i)twilio[a-z0-9_ .\-]{0,30}?(?:token|secret|auth)[a-z0-9_ .\-]{0,20}?["']?[ \t]*[:=][ \t]*["']?([0-9a-f]{32})\b"##,
            &["twilio"],
            Some(3.0),
            High,
            format!("TWILIO_AUTH_TOKEN={}", junk(HEX, 32, 16)),
            "TWILIO_AUTH_TOKEN=0123",
        ),
        d(
            "sendgrid-api-key",
            "sendgrid",
            "SendGrid API key (SG.<22>.<43>)",
            r"(?m)\b(SG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43})(?:[^A-Za-z0-9_\-]|$)",
            &["sg."],
            Some(3.5),
            High,
            format!("SENDGRID_API_KEY=SG.{}.{}", junk(B64URL, 22, 17), junk(B64URL, 43, 18)),
            "SG.short.short",
        ),
        d(
            "mailgun-api-key",
            "mailgun",
            "Mailgun private API key (key- + 32 hex)",
            r"\b(key-[0-9a-f]{32})\b",
            &["key-"],
            Some(3.0),
            High,
            format!("api_key=key-{}", junk(HEX, 32, 19)),
            "key-value-store-0123",
        ),
        d(
            "discord-bot-token",
            "discord",
            "Discord bot token near a `discord` identifier",
            r"(?:discord|DISCORD|Discord)[^\n]{0,60}?\b([MNO][A-Za-z0-9_\-]{23,25}\.[A-Za-z0-9_\-]{6}\.[A-Za-z0-9_\-]{27,38})\b",
            &["discord"],
            Some(3.5),
            High,
            format!(
                "DISCORD_TOKEN=M{}.{}.{}",
                junk(B62, 23, 20),
                junk(B62, 6, 21),
                junk(B64URL, 30, 22)
            ),
            "DISCORD_TOKEN=Mshort.abc.def",
        ),
        d(
            "discord-webhook",
            "discord",
            "Discord webhook URL",
            r"(https://(?:ptb\.|canary\.)?discord(?:app)?\.com/api/webhooks/[0-9]{17,20}/[A-Za-z0-9_\-]{60,80})",
            &["/api/webhooks/"],
            Some(3.5),
            High,
            format!(
                "https://discord.com/api/webhooks/123456789012345678/{}",
                junk(B64URL, 68, 23)
            ),
            "https://discord.com/api/webhooks/123/abc",
        ),
        d(
            "telegram-bot-token",
            "telegram",
            "Telegram bot token (<id>:AA<33>)",
            r"(?m)\b([0-9]{8,10}:AA[A-Za-z0-9_\-]{33})(?:[^A-Za-z0-9_\-]|$)",
            &[":aa"],
            Some(3.5),
            High,
            format!("BOT_TOKEN=123456789:AA{}", junk(B64URL, 33, 24)),
            "BOT_TOKEN=123:AAshort",
        ),
    ]
}

/// The built-in rules, in a fixed order, compiled.
///
/// # Panics
///
/// Never in practice: the table is validated by this crate's tests.
pub fn builtin_rules() -> Vec<Rule> {
    defs()
        .into_iter()
        .map(|d| {
            Rule::new(RuleSpec {
                id: d.id.into(),
                provider: d.provider.into(),
                description: d.description.into(),
                regex: d.regex.into(),
                keywords: d.keywords.iter().map(|k| (*k).to_owned()).collect(),
                entropy: d.entropy,
                confidence: d.confidence,
                example: Some(d.example),
                near_miss: Some(d.near_miss),
            })
            .expect("built-in rule table is valid")
        })
        .collect()
}
