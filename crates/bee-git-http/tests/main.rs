//! The integration tests of `bee-git-http`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-git-http --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "fuzz.rs"]
mod fuzz;
#[path = "git_access.rs"]
mod git_access;
#[path = "git_cli.rs"]
mod git_cli;
#[path = "git_desktop.rs"]
mod git_desktop;
#[path = "git_extras.rs"]
mod git_extras;
#[path = "git_matrix.rs"]
mod git_matrix;
#[path = "git_metrics.rs"]
mod git_metrics;
#[path = "git_partial.rs"]
mod git_partial;
#[path = "git_rulesets.rs"]
mod git_rulesets;
#[path = "git_settings.rs"]
mod git_settings;
#[path = "git_ssh.rs"]
mod git_ssh;
