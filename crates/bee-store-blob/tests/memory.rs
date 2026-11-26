//! The conformance suite against the in-memory reference store: it proves the suite itself
//! describes the reference behaviour that the disk and S3 stores are held to.

use crate::common;
use std::sync::Arc;

use bee_store::{ManualClock, MemoryBlobStore};
use common::{Fixture, fresh_org};

async fn make() -> Option<Fixture> {
    Some(Fixture {
        store: Box::new(MemoryBlobStore::new(Arc::new(ManualClock::new(1_000)))),
        org: fresh_org(),
        keep: Box::new(()),
    })
}

conformance_suite!(make);
