//! The integration tests of `bee-git`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-git --test integration <file_stem>::`.

#[path = "collision.rs"]
mod collision;
#[path = "git_corpus.rs"]
mod git_corpus;
#[path = "parser_props.rs"]
mod parser_props;
