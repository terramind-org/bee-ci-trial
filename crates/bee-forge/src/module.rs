//! Forge modules and the registry that composes them.
//!
//! Every forge area is a module that declares, as data, what it owns: its surfaces, its
//! migrations, the event kinds it emits and the audit action prefixes it writes. The
//! [`Registry`] checks that no two modules claim the same surface, table, event kind or
//! audit prefix, and hands the merged migration list to the store.
//!
//! Core modules are registered by [`Registry::core`]. A later worker adds an area by
//! filling in its module's `MODULE` declaration (for example [`crate::changes::MODULE`]) and
//! implementing its store and service in that module's directory; no other file changes
//! except the service wiring in [`crate::service`].

use std::collections::BTreeSet;

use crate::error::{ForgeError, Result};
use crate::store::{Migration, Surface};

/// One event kind a module emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventKind {
    /// Dotted name, e.g. `lease.acquired`.
    pub name: &'static str,
    /// What happened.
    pub description: &'static str,
    /// GitHub webhook event name and `action` for the GitHub payload format, if any.
    pub github: Option<(&'static str, &'static str)>,
}

impl EventKind {
    /// A kind without a GitHub equivalent.
    pub const fn new(name: &'static str, description: &'static str) -> EventKind {
        EventKind {
            name,
            description,
            github: None,
        }
    }

    /// A kind with a GitHub event name and action.
    pub const fn gh(
        name: &'static str,
        description: &'static str,
        event: &'static str,
        action: &'static str,
    ) -> EventKind {
        EventKind {
            name,
            description,
            github: Some((event, action)),
        }
    }
}

/// What a module owns.
#[derive(Debug)]
pub struct ForgeModule {
    /// Module name (snake_case; also its migration namespace).
    pub name: &'static str,
    /// Surfaces it owns.
    pub surfaces: &'static [Surface],
    /// Its migrations, in order.
    pub migrations: &'static [&'static Migration],
    /// Event kinds it emits.
    pub event_kinds: &'static [EventKind],
    /// Audit action prefixes it writes, e.g. `lease.`.
    pub audit_prefixes: &'static [&'static str],
}

/// The composed set of modules.
#[derive(Debug, Default)]
pub struct Registry {
    modules: Vec<&'static ForgeModule>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Every core module plus the declared extension modules.
    pub fn core() -> Result<Registry> {
        let mut r = Registry::new();
        for m in [
            &crate::orgs::MODULE,
            &crate::events::MODULE,
            &crate::events::webhooks::MODULE,
            &crate::leases::MODULE,
            &crate::checks::MODULE,
            &crate::audit::MODULE,
            &crate::changes::MODULE,
            &crate::posts::MODULE,
            &crate::briefs::MODULE,
            &crate::work::MODULE,
            &crate::community::MODULE,
            &crate::enterprise::MODULE,
            &crate::enterprise::apps::MODULE,
            &crate::enterprise::billing::MODULE,
            &crate::enterprise::models::MODULE,
            &crate::enterprise::moderation::MODULE,
            &crate::ci::MODULE,
            &crate::packages::MODULE,
            &crate::security::MODULE,
            &crate::agents::MODULE,
            &crate::swarms::MODULE,
            &crate::safety::MODULE,
            &crate::forks::MODULE,
        ] {
            r.register(m)?;
        }
        Ok(r)
    }

    /// Adds a module after checking it claims nothing another module owns.
    pub fn register(&mut self, m: &'static ForgeModule) -> Result<()> {
        let clash = |what: &str, name: &str| {
            Err(ForgeError::internal(format!(
                "module {} claims {what} {name}, already owned",
                m.name
            )))
        };
        for other in &self.modules {
            if other.name == m.name {
                return clash("name", m.name);
            }
            for s in m.surfaces {
                if other.surfaces.contains(s) {
                    return clash("surface", s.0);
                }
            }
            for k in m.event_kinds {
                if other.event_kinds.iter().any(|o| o.name == k.name) {
                    return clash("event kind", k.name);
                }
            }
            for p in m.audit_prefixes {
                if other
                    .audit_prefixes
                    .iter()
                    .any(|o| o.starts_with(p) || p.starts_with(o))
                {
                    return clash("audit prefix", p);
                }
            }
        }
        for mig in m.migrations {
            if mig.module != m.name {
                return Err(ForgeError::internal(format!(
                    "module {} lists migration {} of another module",
                    m.name,
                    mig.id()
                )));
            }
            for s in mig.surfaces() {
                if !m.surfaces.contains(&s) {
                    return clash("unowned surface in a migration", s.0);
                }
            }
        }
        self.modules.push(m);
        crate::store::migrate::validate(&self.migrations())?;
        Ok(())
    }

    /// Registered modules in registration order.
    pub fn modules(&self) -> &[&'static ForgeModule] {
        &self.modules
    }

    /// Every migration, module by module.
    pub fn migrations(&self) -> Vec<&'static Migration> {
        self.modules
            .iter()
            .flat_map(|m| m.migrations.iter().copied())
            .collect()
    }

    /// Every surface that has tables.
    pub fn surfaces(&self) -> Vec<Surface> {
        let set: BTreeSet<Surface> = self
            .migrations()
            .iter()
            .flat_map(|m| m.surfaces())
            .collect();
        set.into_iter().collect()
    }

    /// Looks up an event kind.
    pub fn event_kind(&self, name: &str) -> Option<&'static EventKind> {
        self.modules
            .iter()
            .flat_map(|m| m.event_kinds.iter())
            .find(|k| k.name == name)
    }

    /// Whether a subscription filter (`kind`, or `prefix.*`) names known kinds.
    pub fn filter_known(&self, filter: &str) -> bool {
        match filter.strip_suffix(".*") {
            Some(prefix) => self
                .modules
                .iter()
                .flat_map(|m| m.event_kinds.iter())
                .any(|k| k.name.starts_with(&format!("{prefix}."))),
            None => filter == "*" || self.event_kind(filter).is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_registry_is_consistent() {
        let r = Registry::core().expect("core registry");
        assert!(r.event_kind("lease.acquired").is_some());
        assert!(r.event_kind("cr.landed").is_some());
        assert!(r.filter_known("cr.*"));
        assert!(!r.filter_known("nope.*"));
        assert!(r.surfaces().contains(&Surface::LEASES));
    }

    #[test]
    fn duplicate_claims_are_rejected() {
        static DUP: ForgeModule = ForgeModule {
            name: "dup",
            surfaces: &[Surface::LEASES],
            migrations: &[],
            event_kinds: &[],
            audit_prefixes: &[],
        };
        let mut r = Registry::core().unwrap();
        assert!(r.register(&DUP).is_err());
    }
}
