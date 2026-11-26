//! What every command runs with: the global flags, the resolved endpoint and token, an RPC
//! client, and the printer for `--json` documents and readable output.

use std::cell::Cell;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use bee_proto::methods::MethodDescriptor;
use clap::ArgMatches;
use serde_json::Value;

use crate::credentials;
use crate::error::CliError;
use crate::output;
use crate::rpc::{Client, Endpoint, StreamMode};

/// Environment variable naming the endpoint.
pub const ENV_ENDPOINT: &str = "BEE_ENDPOINT";
/// Environment variable holding the token.
pub const ENV_TOKEN: &str = "BEE_TOKEN";

/// The global flags as given on the command line (environment and credentials not applied).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlobalOptions {
    /// `--endpoint`.
    pub endpoint: Option<String>,
    /// `--token`.
    pub token: Option<String>,
    /// `--json`.
    pub json: bool,
    /// `-C <dir>`.
    pub dir: Option<PathBuf>,
    /// `--timeout`.
    pub timeout: Option<Duration>,
    /// `--no-daemon`.
    pub no_daemon: bool,
}

impl GlobalOptions {
    /// Reads the global flags from parsed arguments.
    pub fn from_matches(matches: &ArgMatches) -> Result<Self, CliError> {
        let text = |id: &str| matches.get_one::<String>(id).cloned();
        let timeout = text("timeout").map(|t| parse_duration(&t)).transpose()?;
        Ok(Self {
            endpoint: text("endpoint"),
            token: text("token"),
            json: matches.get_flag("json"),
            dir: text("dir").map(PathBuf::from),
            timeout,
            no_daemon: matches.get_flag("no-daemon"),
        })
    }
}

/// Parses `30s`, `500ms`, `2m`, `1h` or a bare number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration, CliError> {
    let t = text.trim();
    let (number, unit) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, "s"),
    };
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => {
            return Err(CliError::usage(
                "bad_flag",
                format!("--timeout: unknown unit in {text:?} (use ms, s, m or h)"),
            ));
        }
    };
    let value: f64 = number.trim().parse().map_err(|_| {
        CliError::usage("bad_flag", format!("--timeout: {text:?} is not a duration"))
    })?;
    if !value.is_finite() || value <= 0.0 {
        return Err(CliError::usage(
            "bad_flag",
            format!("--timeout: {text:?} must be positive"),
        ));
    }
    Ok(Duration::from_secs_f64(value * scale))
}

/// The environment of one command invocation.
#[derive(Debug)]
pub struct Context {
    /// Global flags.
    pub options: GlobalOptions,
    runtime: std::sync::OnceLock<tokio::runtime::Runtime>,
    streamed: Cell<usize>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Context {
    /// A context for these global flags.
    pub fn new(options: GlobalOptions) -> Self {
        Self {
            options,
            runtime: std::sync::OnceLock::new(),
            streamed: Cell::new(0),
        }
    }

    /// True in `--json` mode.
    pub fn json(&self) -> bool {
        self.options.json
    }

    /// The endpoint: `--endpoint`, then `BEE_ENDPOINT`, then `config.toml`, then the one
    /// saved by `bee login`.
    pub fn endpoint(&self) -> Result<Endpoint, CliError> {
        let chosen = match self
            .options
            .endpoint
            .clone()
            .or_else(|| env(ENV_ENDPOINT))
            .or_else(|| crate::client_config::get().endpoint)
        {
            Some(endpoint) => endpoint,
            None => credentials::load()?
                .and_then(|c| c.endpoint)
                .ok_or_else(|| {
                    CliError::usage(
                        "no_endpoint",
                        "no endpoint: pass --endpoint http://host:port, set BEE_ENDPOINT, or \
                         run `bee login --token <token> --endpoint <url>`",
                    )
                })?,
        };
        Endpoint::parse(&chosen)
    }

    /// The token: `--token`, then `BEE_TOKEN`, then the credentials file. `None` sends no
    /// credential (the server then answers `UNAUTHENTICATED` where one is needed).
    pub fn token(&self) -> Result<Option<String>, CliError> {
        if let Some(token) = self.options.token.clone().or_else(|| env(ENV_TOKEN)) {
            return Ok(Some(token));
        }
        Ok(credentials::load()?
            .map(|c| c.token)
            .filter(|t| !t.is_empty()))
    }

    /// An RPC client for the resolved endpoint and token.
    pub fn client(&self) -> Result<Client, CliError> {
        Ok(Client::new(
            self.endpoint()?,
            self.token()?,
            self.options.timeout,
        ))
    }

    fn runtime(&self) -> Result<&tokio::runtime::Runtime, CliError> {
        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::internal(format!("cannot start the I/O runtime: {e}")))?;
        Ok(self.runtime.get_or_init(|| runtime))
    }

    /// Calls a unary RPC with a proto3-JSON request and returns the proto3-JSON response.
    pub fn call_unary(
        &self,
        method: &MethodDescriptor,
        request: &Value,
    ) -> Result<Value, CliError> {
        let client = self.client()?;
        let body = serde_json::to_vec(request).map_err(|e| CliError::internal(e.to_string()))?;
        let bytes = self.runtime()?.block_on(client.unary(method.path, body))?;
        parse_response(method, &bytes)
    }

    /// Calls a streaming RPC, sending `requests` and handing each response message to
    /// `on_item` as it arrives.
    pub fn call_stream(
        &self,
        method: &MethodDescriptor,
        requests: &[Value],
        mode: StreamMode,
        on_item: &mut dyn FnMut(Value) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        let client = self.client()?;
        let mut bodies = Vec::with_capacity(requests.len());
        for request in requests {
            bodies
                .push(serde_json::to_vec(request).map_err(|e| CliError::internal(e.to_string()))?);
        }
        let mut handle = |bytes: &[u8]| on_item(parse_response(method, bytes)?);
        self.runtime()?
            .block_on(client.stream(method.path, &bodies, mode, &mut handle))
    }

    /// Prints a command's single result: the envelope in `--json` mode, else `human`.
    pub fn print_document(&self, schema: &str, data: Value, human: &str) -> Result<(), CliError> {
        let text = if self.json() {
            serde_json::to_string(&output::envelope(schema, data))
                .map_err(|e| CliError::internal(e.to_string()))?
        } else {
            human.to_owned()
        };
        write_line(&text)
    }

    /// Prints one item of a stream as soon as it arrives: one envelope per line (NDJSON) in
    /// `--json` mode, else `human` with `---` between items.
    pub fn emit(&self, schema: &str, data: Value, human: &str) -> Result<(), CliError> {
        let count = self.streamed.get();
        self.streamed.set(count + 1);
        if self.json() {
            let line = serde_json::to_string(&output::envelope(schema, data))
                .map_err(|e| CliError::internal(e.to_string()))?;
            return write_line(&line);
        }
        if count > 0 {
            write_line("---")?;
        }
        write_line(human)
    }

    /// Prints an error: the error document on stdout in `--json` mode, else text on stderr.
    pub fn print_error(&self, error: &CliError) {
        print_error(self.json(), error);
    }

    /// Reads all of standard input.
    pub fn read_stdin(&self) -> Result<String, CliError> {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| CliError::usage("bad_input", format!("cannot read stdin: {e}")))?;
        Ok(text)
    }
}

/// Prints an error in the given mode (also used before a context exists).
pub fn print_error(json: bool, error: &CliError) {
    if error
        .payload
        .get("subcode")
        .and_then(serde_json::Value::as_str)
        == Some(BROKEN_PIPE)
    {
        return;
    }
    if json {
        let text = serde_json::to_string(&error.document()).unwrap_or_else(|_| {
            "{\"schema\":\"bee.v1.error\",\"error\":{\"reason\":\"INTERNAL\",\"message\":\"\",\"retryable\":false}}".to_owned()
        });
        let _ = write_line(&text);
    } else {
        eprintln!("{}", error.render_human());
    }
}

/// Subcode of the error raised when stdout is closed (e.g. piped into `head`); such errors are
/// not printed.
pub const BROKEN_PIPE: &str = "broken_pipe";

fn write_line(text: &str) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .map_err(|e| {
            let error = CliError::internal(format!("cannot write output: {e}"));
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                error.with_payload("subcode", serde_json::json!(BROKEN_PIPE))
            } else {
                error
            }
        })
}

fn parse_response(method: &MethodDescriptor, bytes: &[u8]) -> Result<Value, CliError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(bytes).map_err(|e| {
        CliError::internal(format!(
            "server sent an invalid {} (not JSON: {e})",
            method.output_type
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").expect("s"), Duration::from_secs(30));
        assert_eq!(
            parse_duration("500ms").expect("ms"),
            Duration::from_millis(500)
        );
        assert_eq!(parse_duration("2m").expect("m"), Duration::from_secs(120));
        assert_eq!(
            parse_duration("1.5").expect("bare"),
            Duration::from_millis(1500)
        );
        for bad in ["", "0", "-1s", "3days", "abc"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }
}
