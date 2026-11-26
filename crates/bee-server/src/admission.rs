//! Admission control (SPEC §15.4): priority tiers with bounded concurrency.
//!
//! Every request is classified as `interactive`, `land`, `bulk` or `anonymous`. All tiers
//! share `max_concurrent - reserved` slots; `interactive` and `land` may also use the
//! `reserved` slots, so under load `anonymous` and `bulk` are shed first while `land` and
//! `interactive` keep capacity. `bulk` and `anonymous` are further capped by their own limits
//! (unauthenticated traffic has its own pool and cannot starve authenticated traffic). A
//! request that cannot get a slot within `queue_wait` (or its deadline) is refused with
//! `RESOURCE_EXHAUSTED` and a retry delay.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::AdmissionSection;

/// A priority tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    /// A human's or agent's foreground step.
    Interactive,
    /// Land.
    Land,
    /// Imports, GC, fsck and other mass operations.
    Bulk,
    /// Requests without credentials.
    Anonymous,
}

impl Tier {
    /// Stable name (metrics label).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Interactive => "interactive",
            Tier::Land => "land",
            Tier::Bulk => "bulk",
            Tier::Anonymous => "anonymous",
        }
    }

    /// Classifies a request by its gRPC path and whether it carries credentials.
    pub fn classify(path: &str, authenticated: bool) -> Tier {
        if !authenticated {
            return Tier::Anonymous;
        }
        if path == "/bee.v1.WriteService/Land" {
            return Tier::Land;
        }
        if path.starts_with("/bee.v1.ImportService/")
            || path == "/bee.v1.AdminService/RunGc"
            || path == "/bee.v1.AdminService/RunFsck"
        {
            return Tier::Bulk;
        }
        Tier::Interactive
    }
}

/// Holds a slot until dropped.
pub struct Slot {
    _shared: Option<OwnedSemaphorePermit>,
    _reserved: Option<OwnedSemaphorePermit>,
    _tier: Option<OwnedSemaphorePermit>,
}

/// The admission controller.
pub struct Admission {
    shared: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
    bulk: Arc<Semaphore>,
    anonymous: Arc<Semaphore>,
    queue_wait: Duration,
}

impl Admission {
    /// A controller sized by config.
    pub fn new(cfg: &AdmissionSection) -> Admission {
        Admission {
            shared: Arc::new(Semaphore::new(
                cfg.max_concurrent - cfg.reserved_interactive_land,
            )),
            reserved: Arc::new(Semaphore::new(cfg.reserved_interactive_land)),
            bulk: Arc::new(Semaphore::new(cfg.max_bulk.max(1))),
            anonymous: Arc::new(Semaphore::new(cfg.max_anonymous.max(1))),
            queue_wait: Duration::from_millis(cfg.queue_wait_ms),
        }
    }

    /// Waits for a slot for `tier` until `deadline` or the queue wait, whichever is first.
    /// `Err` carries a suggested retry delay in milliseconds.
    pub async fn acquire(&self, tier: Tier, deadline: Instant) -> Result<Slot, u64> {
        let until = deadline.min(Instant::now() + self.queue_wait);
        let retry_ms = (self.queue_wait.as_millis() as u64 / 4).clamp(50, 2_000);
        let tier_permit = match tier {
            Tier::Bulk => Some(wait(&self.bulk, until).await.ok_or(retry_ms)?),
            Tier::Anonymous => Some(wait(&self.anonymous, until).await.ok_or(retry_ms)?),
            _ => None,
        };
        match tier {
            Tier::Interactive | Tier::Land => {
                // Prefer the shared pool; fall back to the reserved one.
                if let Ok(p) = self.shared.clone().try_acquire_owned() {
                    return Ok(Slot {
                        _shared: Some(p),
                        _reserved: None,
                        _tier: tier_permit,
                    });
                }
                if let Ok(p) = self.reserved.clone().try_acquire_owned() {
                    return Ok(Slot {
                        _shared: None,
                        _reserved: Some(p),
                        _tier: tier_permit,
                    });
                }
                let shared = self.shared.clone();
                let reserved = self.reserved.clone();
                let got = tokio::time::timeout_at(until.into(), async move {
                    tokio::select! {
                        p = shared.acquire_owned() => (p.ok(), None),
                        p = reserved.acquire_owned() => (None, p.ok()),
                    }
                })
                .await
                .map_err(|_| retry_ms)?;
                if got.0.is_none() && got.1.is_none() {
                    return Err(retry_ms);
                }
                Ok(Slot {
                    _shared: got.0,
                    _reserved: got.1,
                    _tier: tier_permit,
                })
            }
            Tier::Bulk | Tier::Anonymous => {
                let p = wait(&self.shared, until).await.ok_or(retry_ms)?;
                Ok(Slot {
                    _shared: Some(p),
                    _reserved: None,
                    _tier: tier_permit,
                })
            }
        }
    }
}

async fn wait(s: &Arc<Semaphore>, until: Instant) -> Option<OwnedSemaphorePermit> {
    tokio::time::timeout_at(until.into(), s.clone().acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AdmissionSection {
        AdmissionSection {
            max_concurrent: 3,
            reserved_interactive_land: 1,
            max_bulk: 1,
            max_anonymous: 1,
            queue_wait_ms: 20,
            ..AdmissionSection::default()
        }
    }

    #[tokio::test]
    async fn bulk_is_shed_before_land_and_land_keeps_reserved_capacity() {
        let a = Admission::new(&cfg());
        let far = Instant::now() + Duration::from_secs(5);
        let _b = a.acquire(Tier::Bulk, far).await.unwrap();
        // The bulk cap is 1.
        assert!(a.acquire(Tier::Bulk, far).await.is_err());
        let _i = a.acquire(Tier::Interactive, far).await.unwrap();
        // Shared pool (2) is now full: anonymous is shed ...
        assert!(a.acquire(Tier::Anonymous, far).await.is_err());
        // ... but a land still gets the reserved slot.
        let _l = a.acquire(Tier::Land, far).await.unwrap();
        assert!(a.acquire(Tier::Land, far).await.is_err());
    }

    #[test]
    fn classification() {
        assert_eq!(
            Tier::classify("/bee.v1.WriteService/Land", true),
            Tier::Land
        );
        assert_eq!(
            Tier::classify("/bee.v1.WriteService/Land", false),
            Tier::Anonymous
        );
        assert_eq!(
            Tier::classify("/bee.v1.ImportService/StartImport", true),
            Tier::Bulk
        );
        assert_eq!(
            Tier::classify("/bee.v1.QueryService/Log", true),
            Tier::Interactive
        );
    }
}
