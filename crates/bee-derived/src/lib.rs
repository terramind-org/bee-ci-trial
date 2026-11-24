//! The derived-data framework (SPEC §8, PRODUCT §4.4).
//!
//! A derived type ([`DerivedType`]) is `{name, version, mode, key, derive}` with a
//! deterministic `derive`. Values live in the store's `derived` table under
//! `(key, name, version)`, written insert-or-ignore, so concurrent derivations are harmless
//! and [`Leases`] only avoid duplicated work.
//!
//! - **Eager** types are computed in CreateChangeset's transaction: [`Registry`] implements
//!   `bee_history::EagerRows`, so a `History` built `with_eager(registry)` writes `graph`
//!   and `path_history` (from `bee-history`), plus any other eager type registered, next to
//!   the `object_paths` rows `bee_store::Writer::build` computes.
//! - **Lazy** types are derived on first read ([`Derived::get`]): types with dependencies
//!   find the nearest stored values and derive forward iteratively, never recursively,
//!   writing each value back (`bee_history::derive`).
//! - **Versions**: bumping a type's version makes reads derive afresh under the new version;
//!   old rows stay readable through [`Derived::read_stored`] until GC.
//!
//! Built-in types ([`Registry::standard`]):
//!
//! | Type | Mode | Key | Module |
//! |---|---|---|---|
//! | `graph` | eager | changeset | `bee_history::graph` |
//! | `path_history` | eager | changeset (and node ids) | `bee_history::pathhist` |
//! | `blame` | lazy | file history node | `bee_history::blame` |
//! | `diff_class` | lazy | changeset | [`diff_class`] |
//! | `copy_detection` | lazy | changeset | [`copies`] |
//! | `language_stats` | lazy | changeset | [`languages`] |
//! | `git_mapping` | importer | changeset / foreign id | `bee_history::gitmap` |

pub mod attributes;
pub mod copies;
pub mod core_types;
pub mod diff_class;
pub mod engine;
pub mod languages;
pub mod registry;
pub mod types;

pub use copies::{CopyDetectionType, CopyKind, CopyRecord};
pub use core_types::{BlameType, GitMappingType, GraphType, PathHistoryType};
pub use diff_class::{DiffClassType, DiffClassValue};
pub use engine::{Derived, Leases};
pub use languages::{LanguageStats, LanguageStatsType, LanguageTotals};
pub use registry::Registry;
pub use types::{DerivedType, DerivedValue, KeyKind, Mode};
