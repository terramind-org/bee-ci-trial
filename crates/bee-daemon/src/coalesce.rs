//! Request coalescing ("single flight") over batches of keys.
//!
//! Concurrent callers asking for overlapping keys share one computation per key: the first
//! caller to ask for a key leads it (its `fetch` receives every key it leads, so one upstream
//! request covers them), later callers wait for the leader's value. A leader that is cancelled
//! or produces no value for a key releases it, and its waiters retry, leading it themselves.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

type Slot<V> = watch::Receiver<Option<V>>;

/// Coalesces concurrent work per key.
pub struct Coalescer<K, V> {
    inflight: Arc<Mutex<HashMap<K, Slot<V>>>>,
}

impl<K, V> Default for Coalescer<K, V> {
    fn default() -> Self {
        Self {
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Releases led keys when the leader finishes or is dropped.
struct Lead<K: Eq + Hash, V> {
    map: Arc<Mutex<HashMap<K, Slot<V>>>>,
    keys: Vec<K>,
    senders: Vec<watch::Sender<Option<V>>>,
}

impl<K: Eq + Hash, V> Drop for Lead<K, V> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.map.lock() {
            for k in &self.keys {
                map.remove(k);
            }
        }
    }
}

impl<K, V> Coalescer<K, V>
where
    K: Eq + Hash + Clone + Send,
    V: Clone + Send + Sync,
{
    /// A coalescer with nothing in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// Keys currently being computed.
    pub fn in_flight(&self) -> usize {
        self.inflight.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Values for `keys`. Each key not in flight is first offered to `check` (under the
    /// coalescer's lock, so a value that just landed is seen); the rest that nobody computes
    /// are led by this call: `fetch` gets them in one batch and returns what it found. Keys
    /// with no value are absent from the result.
    pub async fn run<C, F, Fut>(&self, keys: &[K], check: C, fetch: F) -> HashMap<K, V>
    where
        C: Fn(&K) -> Option<V>,
        F: Fn(Vec<K>) -> Fut,
        Fut: Future<Output = HashMap<K, V>>,
    {
        let mut out: HashMap<K, V> = HashMap::new();
        let mut pending: Vec<K> = Vec::new();
        for k in keys {
            if !pending.contains(k) {
                pending.push(k.clone());
            }
        }
        // Waiters whose leader gave up retry, leading the key themselves; three rounds bound
        // the loop even under repeated cancellation.
        for _round in 0..3 {
            if pending.is_empty() {
                break;
            }
            let mut waits: Vec<(K, Slot<V>)> = Vec::new();
            let mut lead = Lead {
                map: self.inflight.clone(),
                keys: Vec::new(),
                senders: Vec::new(),
            };
            {
                let mut map = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
                for k in pending.drain(..) {
                    if let Some(rx) = map.get(&k) {
                        waits.push((k, rx.clone()));
                    } else if let Some(v) = check(&k) {
                        out.insert(k, v);
                    } else {
                        let (tx, rx) = watch::channel(None);
                        map.insert(k.clone(), rx);
                        lead.keys.push(k);
                        lead.senders.push(tx);
                    }
                }
            }
            if !lead.keys.is_empty() {
                let found = fetch(lead.keys.clone()).await;
                for (k, tx) in lead.keys.iter().zip(lead.senders.iter()) {
                    if let Some(v) = found.get(k) {
                        let _ = tx.send(Some(v.clone()));
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            drop(lead);
            for (k, mut rx) in waits {
                let value = match rx.wait_for(Option::is_some).await {
                    Ok(v) => v.clone(),
                    Err(_) => None,
                };
                match value {
                    Some(v) => {
                        out.insert(k, v);
                    }
                    None => pending.push(k),
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn thousand_concurrent_callers_share_one_fetch() {
        let c: Arc<Coalescer<u32, u64>> = Arc::new(Coalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..1000 {
            let c = c.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                c.run(
                    &[7, 8],
                    |_| None,
                    |keys| {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            keys.into_iter().map(|k| (k, u64::from(k) * 10)).collect()
                        }
                    },
                )
                .await
            }));
        }
        for t in tasks {
            let got = t.await.expect("join");
            assert_eq!(got.get(&7), Some(&70));
            assert_eq!(got.get(&8), Some(&80));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(c.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_leader_hands_over() {
        let c: Arc<Coalescer<u32, u32>> = Arc::new(Coalescer::new());
        let leader = {
            let c = c.clone();
            tokio::spawn(async move {
                c.run(
                    &[1],
                    |_| None,
                    |_| async {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        HashMap::new()
                    },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move {
                c.run(
                    &[1],
                    |_| None,
                    |keys| async move { keys.into_iter().map(|k| (k, 5)).collect() },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        leader.abort();
        let got = waiter.await.expect("join");
        assert_eq!(got.get(&1), Some(&5));
    }

    #[tokio::test]
    async fn check_short_circuits_and_missing_values_are_absent() {
        let c: Coalescer<u32, u32> = Coalescer::new();
        let got = c
            .run(
                &[1, 2, 2],
                |k| (*k == 1).then_some(100),
                |_| async { HashMap::new() },
            )
            .await;
        assert_eq!(got.get(&1), Some(&100));
        assert!(!got.contains_key(&2));
    }
}
