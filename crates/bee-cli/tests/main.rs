//! The integration tests of `bee-cli`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-cli --test integration <file_stem>::`.

#[path = "contract.rs"]
mod contract;
#[path = "daemon.rs"]
mod daemon;
#[path = "git_helper.rs"]
mod git_helper;
#[path = "import_github.rs"]
mod import_github;
#[path = "json_numbers.rs"]
mod json_numbers;
#[path = "login_server.rs"]
mod login_server;
#[path = "mcp_stdio.rs"]
mod mcp_stdio;
mod support;
#[path = "wc_aliases.rs"]
mod wc_aliases;
#[path = "wc_server.rs"]
mod wc_server;
