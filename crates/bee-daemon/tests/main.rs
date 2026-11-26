//! The integration tests of `bee-daemon`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-daemon --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "bootstrap.rs"]
mod bootstrap;
#[path = "config_file.rs"]
mod config_file;
#[path = "e2e.rs"]
mod e2e;
#[path = "tier.rs"]
mod tier;
#[path = "tier_surface.rs"]
mod tier_surface;
#[path = "watch_status.rs"]
mod watch_status;
