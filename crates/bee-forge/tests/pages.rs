//! Pages: the site and deployment services (permissions, builds from a tree, atomic switch,
//! workflow artifacts, previews per change request), custom domains with DNS verification and
//! certificates, and the serving rules through `Pages::respond`.

use crate::packages_support as support;

#[path = "pages/deploy.rs"]
mod deploy;
#[path = "pages/domains.rs"]
mod domains;
