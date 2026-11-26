//! GitHub Actions-compatible workflow engine core (PRODUCT 4.14, spec in
//! `docs/spec/workflows.md`).
//!
//! Beehive runs `.github/workflows` files unmodified. This layer-1 library holds everything
//! that is a pure function of the files and the event, so the runner service only executes:
//!
//! * [`yaml`]: a YAML 1.2 reader with line/column positions (GitHub's core schema: `on` is a
//!   string key, `3.10` is the number 3.1).
//! * [`parse`]: workflow syntax and validation into [`model::Workflow`], reporting every
//!   problem with its position ([`error::WorkflowError`], mapped to SPEC §12.2 reasons).
//! * [`expr`] and [`value`]: the `${{ }}` expression language with GitHub's loose comparison and
//!   coercion rules, all functions, contexts through [`expr::ContextProvider`], and `if:`
//!   conditions with the implicit `success() &&`.
//! * [`matrix`]: matrix expansion with include/exclude semantics and the 256-job limit.
//! * [`graph`]: job ordering, cycle detection and result propagation through `needs`.
//! * [`concurrency`]: concurrency group admission and cancellation decisions.
//! * [`event`] and [`glob`] and [`cron`]: mapping Beehive events to Actions events and
//!   deciding which workflows trigger, with GitHub's filter patterns and cron schedules.
//! * [`plan`]: the deterministic plan of runs, jobs and steps for an event.
//!
//! Nothing here reads a clock, a random source or the environment; maps that affect output
//! keep source order or sort.

pub mod concurrency;
pub mod cron;
pub mod error;
pub mod event;
pub mod expr;
pub mod glob;
pub mod graph;
pub mod matrix;
pub mod model;
pub mod parse;
pub mod plan;
pub mod value;
pub mod yaml;

pub use error::{Diagnostic, ErrorKind, WorkflowError};
pub use event::{ActionsEvent, BeeEvent, Cause, RepoInfo, map_event, select_workflows, triggers};
pub use model::Workflow;
pub use parse::parse_workflow;
pub use plan::{Plan, PlanEntry, PlanInput, RunInfo, plan_event, plan_job, plan_workflow};
pub use value::Value;
