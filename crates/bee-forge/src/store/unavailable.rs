//! Stores that fail on purpose, for failure-isolation tests (PRODUCT §4.22).
//!
//! - [`UnavailableStore`]: every call is `UNAVAILABLE`, as if the forge database were stopped.
//! - [`SwitchableStore`]: wraps a real store; individual surfaces (or all of them) can be
//!   taken down and brought back, to prove that one surface's outage leaves the others
//!   working and that outboxed events arrive after recovery.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::time::Instant;

use super::{ForgeStore, ForgeTx, Migration, MigrationReport, Surface};
use crate::error::{ForgeError, Result};

/// A forge store whose database is down.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableStore;

#[async_trait]
impl ForgeStore for UnavailableStore {
    fn backend(&self) -> &'static str {
        "unavailable"
    }

    async fn migrate(&self, _migrations: &[&Migration]) -> Result<MigrationReport> {
        Err(ForgeError::unavailable("forge database unavailable"))
    }

    async fn begin(&self, surface: Surface, _deadline: Instant) -> Result<Box<dyn ForgeTx>> {
        Err(ForgeError::unavailable(format!(
            "forge database unavailable (surface {surface})"
        )))
    }
}

/// A store whose surfaces can be switched off and on.
pub struct SwitchableStore {
    inner: Arc<dyn ForgeStore>,
    down: Mutex<BTreeSet<&'static str>>,
}

/// Name that takes every surface down at once.
pub const ALL_SURFACES: &str = "*";

impl SwitchableStore {
    /// Wraps `inner` with every surface up.
    pub fn new(inner: Arc<dyn ForgeStore>) -> SwitchableStore {
        SwitchableStore {
            inner,
            down: Mutex::new(BTreeSet::new()),
        }
    }

    /// Takes a surface down (`Surface(ALL_SURFACES)` for all).
    pub fn take_down(&self, s: Surface) {
        if let Ok(mut d) = self.down.lock() {
            d.insert(s.0);
        }
    }

    /// Brings a surface back.
    pub fn bring_up(&self, s: Surface) {
        if let Ok(mut d) = self.down.lock() {
            d.remove(s.0);
        }
    }

    fn is_down(&self, s: Surface) -> bool {
        self.down
            .lock()
            .map(|d| d.contains(s.0) || d.contains(ALL_SURFACES))
            .unwrap_or(true)
    }
}

#[async_trait]
impl ForgeStore for SwitchableStore {
    fn backend(&self) -> &'static str {
        self.inner.backend()
    }

    async fn migrate(&self, migrations: &[&Migration]) -> Result<MigrationReport> {
        self.inner.migrate(migrations).await
    }

    async fn begin(&self, surface: Surface, deadline: Instant) -> Result<Box<dyn ForgeTx>> {
        if self.is_down(surface) {
            return Err(ForgeError::unavailable(format!(
                "surface {surface} is down"
            )));
        }
        self.inner.begin(surface, deadline).await
    }
}
