//! Dependency graph, SBOM export and vulnerability matching from manifests and lockfiles (PRODUCT 4.18).

pub mod cvss;
mod cvss4_table;
pub mod error;
pub mod license;
pub mod matching;
pub mod model;
pub mod osv;
pub mod parse;
pub mod purl;
pub mod review;
pub mod sbom;
pub mod update;
pub mod version;
pub mod xml;
pub mod yaml;

pub use error::{DepsError, DepsErrorKind};
pub use model::{DependencyGraph, Ecosystem, ManifestInfo, Package, Relationship, Scope};
pub use parse::SourceFile;
pub use purl::Purl;
