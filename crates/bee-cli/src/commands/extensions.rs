//! Extension point for hand-written ergonomic commands.
//!
//! Modules that add commands (for example `crate::wc` for working-copy commands such as
//! `status`, `commit` or `land`) implement [`super::Command`] for each and list them here, e.g.
//! `crate::wc::commands()`. Nothing else needs to change: help, dispatch, the `--json`
//! envelope, `bee schema`, `bee help-agent`, the generated output schemas under
//! `schemas/v1/cli/` and the collision and coverage tests all read [`super::registry`].

use super::Command;

/// Ergonomic commands registered by other modules, in the order they appear in help.
pub fn extensions() -> Vec<&'static dyn Command> {
    crate::wc::commands()
}
