use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::oneshot;

/// A shared batcher that accumulates work items across concurrent handler
/// invocations, then flushes them together when a threshold is met.
///
/// This is the dataloader pattern applied to event handlers: each handler
/// thinks it's doing one unit of work, but the batcher transparently groups
/// items and processes them in a single batch call.
///
/// ```ignore
/// let sources = ctx.deps().batcher
///     .submit("source_expansion", concern.id, concern.clone())
///     .flush_when(|batch| batch.len() >= 8 || batch.age() >= Duration::from_secs(5))
///     .then(discover_response_sources_batch)
///     .await?;
/// ```
#[derive(Clone)]
pub struct Batcher {
    groups: Arc<DashMap<String, Arc<dyn std::any::Any + Send + Sync>>>,
}

impl Batcher {
    pub fn new() -> Self {
        Self {
            groups: Arc::new(DashMap::new()),
        }
    }

    /// Submit an item to a named batch group.
    ///
    /// Items with the same `group` name are collected together. The returned
    /// `Submission` lets you configure flush policy and the batch function.
    pub fn submit<K, I>(&self, group: impl Into<String>, key: K, item: I) -> Submission<K, I>
    where
        K: Eq + Hash + Clone + Send + Sync + 'static,
        I: Send + Sync + 'static,
    {
        Submission {
            batcher: self.clone(),
            group: group.into(),
            key,
            item: Some(item),
        }
    }
}

impl Default for Batcher {
    fn default() -> Self {
        Self::new()
    }
}

/// A pending submission to a batch group. Configure flush policy with
/// `flush_when`, then provide the batch function with `then`.
pub struct Submission<K, I> {
    batcher: Batcher,
    group: String,
    key: K,
    item: Option<I>,
}

impl<K, I> Submission<K, I>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    I: Send + Sync + 'static,
{
    /// Set the flush policy for this batch group.
    ///
    /// The predicate receives a `BatchInfo` with the current batch size
    /// and age, and returns `true` when the batch should flush.
    ///
    /// ```ignore
    /// .flush_when(|batch| batch.len() >= 8 || batch.age() >= Duration::from_secs(5))
    /// ```
    pub fn flush_when<P>(self, predicate: P) -> FlushConfigured<K, I, P>
    where
        P: Fn(&BatchInfo) -> bool + Send + Sync + 'static,
    {
        FlushConfigured {
            batcher: self.batcher,
            group: self.group,
            key: self.key,
            item: self.item,
            predicate,
        }
    }
}

/// Batch metadata passed to the `flush_when` predicate.
pub struct BatchInfo {
    len: usize,
    created_at: Instant,
}

impl BatchInfo {
    /// Number of items currently in the batch.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How long since the first item was added to this batch.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

/// A submission with flush policy configured. Call `then` to provide the
/// batch processing function.
pub struct FlushConfigured<K, I, P> {
    batcher: Batcher,
    group: String,
    key: K,
    item: Option<I>,
    predicate: P,
}

impl<K, I, P> FlushConfigured<K, I, P>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    I: Send + Sync + 'static,
    P: Fn(&BatchInfo) -> bool + Send + Sync + 'static,
{
    /// Provide the batch processing function and await the result.
    ///
    /// The function receives all accumulated items and must return a
    /// `HashMap<K, O>` mapping each item's key to its result.
    ///
    /// ```ignore
    /// .then(|items: Vec<Concern>| async move {
    ///     let results = process_batch(&items).await?;
    ///     Ok(results.into_iter().map(|r| (r.id, r)).collect())
    /// })
    /// .await?
    /// ```
    pub async fn then<F, Fut, O>(mut self, f: F) -> Result<O>
    where
        F: FnOnce(Vec<I>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HashMap<K, O>>> + Send + 'static,
        O: Send + Sync + 'static,
    {
        let item = self.item.take().expect("item already consumed");
        let key = self.key.clone();

        type GroupState<K, I, O> = Arc<Mutex<BatchGroupInner<K, I, O>>>;

        let group_state: GroupState<K, I, O> = {
            let groups = &self.batcher.groups;
            let entry = groups
                .entry(self.group.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(BatchGroupInner::<K, I, O> {
                        items: Vec::new(),
                        keys: Vec::new(),
                        waiters: Vec::new(),
                        created_at: Instant::now(),
                        timer_active: false,
                    })) as Arc<dyn std::any::Any + Send + Sync>
                });
            entry
                .value()
                .clone()
                .downcast::<Mutex<BatchGroupInner<K, I, O>>>()
                .expect("batch group type mismatch")
        };

        let (tx, rx) = oneshot::channel::<Result<O>>();

        let (should_flush, should_spawn_timer) = {
            let mut inner = group_state.lock();
            inner.keys.push(key);
            inner.items.push(item);
            inner.waiters.push(tx);

            let info = BatchInfo {
                len: inner.items.len(),
                created_at: inner.created_at,
            };
            let flush_now = (self.predicate)(&info);
            let spawn_timer = !flush_now && !inner.timer_active;
            if spawn_timer {
                inner.timer_active = true;
            }
            (flush_now, spawn_timer)
        };

        if should_flush {
            do_flush(&self.batcher.groups, &self.group, &group_state, f).await;
        } else if should_spawn_timer {
            let groups = self.batcher.groups.clone();
            let group_name = self.group.clone();
            let predicate = self.predicate;
            let group_state = group_state.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let should_flush = {
                        let inner = group_state.lock();
                        if inner.items.is_empty() {
                            // Already flushed by a count-triggered submitter
                            break;
                        }
                        let info = BatchInfo {
                            len: inner.items.len(),
                            created_at: inner.created_at,
                        };
                        predicate(&info)
                    };
                    if should_flush {
                        do_flush(&groups, &group_name, &group_state, f).await;
                        break;
                    }
                }
            });
        }

        rx.await.map_err(|_| anyhow::anyhow!("batch group dropped without flushing"))?
    }
}

async fn do_flush<K, I, F, Fut, O>(
    groups: &DashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
    group_name: &str,
    group_state: &Mutex<BatchGroupInner<K, I, O>>,
    f: F,
)
where
    K: Eq + Hash + Clone,
    F: FnOnce(Vec<I>) -> Fut,
    Fut: Future<Output = Result<HashMap<K, O>>> + Send,
    O: Send,
{
    let (items, keys, waiters) = {
        let mut inner = group_state.lock();
        let items = std::mem::take(&mut inner.items);
        let keys = std::mem::take(&mut inner.keys);
        let waiters = std::mem::take(&mut inner.waiters);
        inner.created_at = Instant::now();
        inner.timer_active = false;
        (items, keys, waiters)
    };

    groups.remove(group_name);

    match f(items).await {
        Ok(mut results) => {
            for (key, tx) in keys.into_iter().zip(waiters) {
                match results.remove(&key) {
                    Some(val) => { let _ = tx.send(Ok(val)); }
                    None => { let _ = tx.send(Err(anyhow::anyhow!("batch function did not return result for key"))); }
                }
            }
        }
        Err(e) => {
            let error_msg = e.to_string();
            for tx in waiters {
                let _ = tx.send(Err(anyhow::anyhow!("{}", error_msg)));
            }
        }
    }
}

struct BatchGroupInner<K, I, O> {
    items: Vec<I>,
    keys: Vec<K>,
    waiters: Vec<oneshot::Sender<Result<O>>>,
    created_at: Instant,
    timer_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_item_flushes_immediately() {
        let batcher = Batcher::new();

        let result = batcher
            .submit("test", "key1", 42)
            .flush_when(|batch| batch.len() >= 1)
            .then(|items| async move {
                Ok(items.into_iter().map(|i| ("key1", i * 2)).collect())
            })
            .await
            .unwrap();

        assert_eq!(result, 84);
    }

    #[tokio::test]
    async fn batch_collects_multiple_items() {
        let batcher = Batcher::new();
        let batcher2 = batcher.clone();

        let (a, b) = tokio::join!(
            async {
                batcher
                    .submit("add", 1u32, 10)
                    .flush_when(|batch| batch.len() >= 2)
                    .then(|items| async move {
                        Ok(items.into_iter().enumerate().map(|(i, v)| ((i + 1) as u32, v * 10)).collect())
                    })
                    .await
            },
            async {
                // Small delay so first submit registers the group
                tokio::time::sleep(Duration::from_millis(5)).await;
                batcher2
                    .submit("add", 2u32, 20)
                    .flush_when(|batch| batch.len() >= 2)
                    .then(|items| async move {
                        Ok(items.into_iter().enumerate().map(|(i, v)| ((i + 1) as u32, v * 10)).collect())
                    })
                    .await
            },
        );

        assert_eq!(a.unwrap(), 100);
        assert_eq!(b.unwrap(), 200);
    }

    #[tokio::test]
    async fn batch_error_propagates_to_all_waiters() {
        let batcher = Batcher::new();
        let batcher2 = batcher.clone();

        let (a, b) = tokio::join!(
            async {
                batcher
                    .submit("fail", 1u32, 10)
                    .flush_when(|batch| batch.len() >= 2)
                    .then(|_items: Vec<i32>| async move {
                        Err::<HashMap<u32, i32>, _>(anyhow::anyhow!("batch failed"))
                    })
                    .await
            },
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                batcher2
                    .submit("fail", 2u32, 20)
                    .flush_when(|batch| batch.len() >= 2)
                    .then(|_items: Vec<i32>| async move {
                        Err::<HashMap<u32, i32>, _>(anyhow::anyhow!("batch failed"))
                    })
                    .await
            },
        );

        assert!(a.is_err());
        assert!(b.is_err());
    }

    #[tokio::test]
    async fn single_item_flushes_on_timeout() {
        let batcher = Batcher::new();

        // Only age-based flush — no count threshold met.
        // This single item must still flush after the timeout.
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            batcher
                .submit("timeout_test", "k1", 99)
                .flush_when(|batch| batch.len() >= 100 || batch.age() >= Duration::from_millis(50))
                .then(|items| async move {
                    Ok(items.into_iter().map(|v| ("k1", v + 1)).collect())
                }),
        )
        .await
        .expect("should not hang — timeout flush must fire")
        .unwrap();

        assert_eq!(result, 100);
    }

    #[tokio::test]
    async fn different_groups_are_independent() {
        let batcher = Batcher::new();
        let batcher2 = batcher.clone();

        let (a, b) = tokio::join!(
            async {
                batcher
                    .submit("group_a", "k", 100)
                    .flush_when(|batch| batch.len() >= 1)
                    .then(|items| async move {
                        Ok(items.into_iter().map(|v| ("k", v + 1)).collect())
                    })
                    .await
            },
            async {
                batcher2
                    .submit("group_b", "k", 200)
                    .flush_when(|batch| batch.len() >= 1)
                    .then(|items| async move {
                        Ok(items.into_iter().map(|v| ("k", v + 2)).collect())
                    })
                    .await
            },
        );

        assert_eq!(a.unwrap(), 101);
        assert_eq!(b.unwrap(), 202);
    }
}
