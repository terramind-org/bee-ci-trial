//! Upkeep: an item left `PENDING` for [`super::PENDING_TTL_MS`] is deleted with its files.
//! The server runs [`Briefs::maintain`] on its maintenance interval.

use super::model::EvidenceRec;
use super::store::Briefs;
use super::{EVIDENCE, SURFACE};
use crate::cx::Cx;
use crate::error::Result;
use crate::store::{Order, Scan};
use bee_proto::v1::EvidenceState;

/// Most expired items one pass deletes.
pub const BATCH: usize = 1000;

impl Briefs {
    /// Deletes pending items past their expiry and their files; returns how many.
    pub async fn maintain(&self, cx: &Cx) -> Result<usize> {
        let now = self.db.now_ms() as i64;
        let expired = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let scan = Scan {
                order: Order::N1,
                n1_max: Some(now),
                limit: BATCH,
                ..Scan::default()
            };
            tx.scan_msgs::<EvidenceRec>(&EVIDENCE, &scan).await?
        };
        let mut n = 0;
        for (_, row) in expired {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            // Re-read: it may have been finalized since.
            let Some((cur, _)) = tx
                .get_msg::<EvidenceRec>(&EVIDENCE, &row.scope, &row.id)
                .await?
            else {
                continue;
            };
            if cur.state() != EvidenceState::Pending || cur.expires_ms as i64 > now {
                continue;
            }
            tx.delete(&EVIDENCE, &row.scope, &row.id).await?;
            tx.commit().await?;
            let blobs: Vec<String> = cur
                .assets
                .iter()
                .filter(|a| !a.blob.is_empty())
                .map(|a| a.blob.clone())
                .collect();
            self.discard(&cur.org_id, &blobs).await;
            n += 1;
        }
        Ok(n)
    }
}
