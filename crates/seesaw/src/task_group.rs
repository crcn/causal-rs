//! Task group for tracking spawned tasks.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{anyhow, Error, Result};
use futures::FutureExt;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Tracks spawned tasks, captures first error, supports cancellation.
pub struct TaskGroup {
    pending: AtomicUsize,
    notify: Notify,
    error: Mutex<Option<Error>>,
    spawn_handles: Mutex<Vec<JoinHandle<()>>>,
    cancelled: AtomicBool,
    settled: AtomicBool,
    /// Transient groups to cancel when this group settles or is dropped.
    transient_groups: Mutex<Vec<Arc<TaskGroup>>>,
    /// Points to the head (foreground) task group. None if this IS the head.
    head: Option<Weak<TaskGroup>>,
}

impl TaskGroup {
    /// Create a new task group.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(0),
            notify: Notify::new(),
            error: Mutex::new(None),
            spawn_handles: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            transient_groups: Mutex::new(Vec::new()),
            head: None,
        })
    }

    /// Create a transient task group.
    /// Tasks in this group don't count toward this group's `settled()`.
    /// Transient groups are cancelled when this group settles or is dropped.
    pub fn transient(self: &Arc<Self>) -> Arc<Self> {
        let bg = Arc::new(Self {
            pending: AtomicUsize::new(0),
            notify: Notify::new(),
            error: Mutex::new(None),
            spawn_handles: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            transient_groups: Mutex::new(Vec::new()),
            head: Some(Arc::downgrade(&self.head_or_self())),
        });
        self.transient_groups.lock().push(bg.clone());
        bg
    }

    /// Get the head (foreground) task group, or self if this is the head.
    pub fn head_or_self(self: &Arc<Self>) -> Arc<Self> {
        self.head
            .as_ref()
            .and_then(|w| w.upgrade())
            .unwrap_or_else(|| self.clone())
    }

    /// Check if this task group has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until this task group is cancelled.
    /// Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                return;
            }
            self.notify.notified().await;
            // Re-check after being notified
            if self.cancelled.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    /// Spawn a tracked task.
    pub fn spawn<F>(self: &Arc<Self>, future: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        self.pending.fetch_add(1, Ordering::Relaxed);

        let group = self.clone();
        let handle = tokio::spawn(async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;

            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => group.capture_error(e),
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    group.capture_error(anyhow!("task panicked: {}", msg));
                }
            }

            group.decrement();
        });

        self.spawn_handles.lock().push(handle);
    }

    /// Capture an error directly.
    pub fn capture_error(&self, error: Error) {
        let mut lock = self.error.lock();
        if lock.is_none() {
            *lock = Some(error);
            self.notify.notify_waiters();
        }
    }

    /// Wait for all tasks to complete, then return first error if any.
    /// Cancels background groups when complete.
    ///
    /// Note: Errors don't cause early exit. All tasks complete before returning.
    /// This ensures effect chains complete even when individual effects error.
    pub fn settled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            // Wait for all pending tasks to complete
            loop {
                if self.pending.load(Ordering::Relaxed) == 0 {
                    break;
                }
                self.notify.notified().await;
            }

            self.settled.store(true, Ordering::SeqCst);

            // Cancel all background groups
            for bg in self.transient_groups.lock().iter() {
                bg.cancel();
            }

            // Return first error after all tasks complete
            if let Some(err) = self.error.lock().take() {
                return Err(err);
            }

            Ok(())
        })
    }

    /// Cancel all tracked tasks and background groups.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let handles: Vec<_> = self.spawn_handles.lock().drain(..).collect();
        let mut aborted_count = 0;
        for handle in handles {
            // Only count tasks that weren't already finished - aborting a finished
            // task does nothing, but we'd incorrectly decrement pending for it
            if !handle.is_finished() {
                aborted_count += 1;
            }
            handle.abort();
        }
        // Decrement pending count for aborted tasks since their cleanup won't run
        if aborted_count > 0 {
            self.pending.fetch_sub(aborted_count, Ordering::Relaxed);
        }
        // Cancel all background groups
        for bg in self.transient_groups.lock().iter() {
            bg.cancel();
        }
        self.notify.notify_waiters();
    }

    fn decrement(&self) {
        if self.pending.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.notify.notify_waiters();
        }
    }
}

impl Default for TaskGroup {
    fn default() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            notify: Notify::new(),
            error: Mutex::new(None),
            spawn_handles: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            transient_groups: Mutex::new(Vec::new()),
            head: None,
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        // Cancel all tasks when dropped (RAII cleanup)
        self.cancelled.store(true, Ordering::SeqCst);
        for handle in self.spawn_handles.lock().drain(..) {
            handle.abort();
        }
        // Cancel all transient groups
        for bg in self.transient_groups.lock().iter() {
            bg.cancel();
        }
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_task_group_spawn_and_settle() {
        let group = TaskGroup::new();
        group.spawn(async { Ok(()) });
        group.settled().await.unwrap();
    }

    #[tokio::test]
    async fn test_task_group_error_capture() {
        let group = TaskGroup::new();

        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err(anyhow!("test error"))
        });

        let result = group.settled().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test error"));
    }

    #[tokio::test]
    async fn test_task_group_cancel() {
        let group = TaskGroup::new();

        group.spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        });

        assert!(!group.is_cancelled());
        group.cancel();
        assert!(group.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_propagates_to_transient() {
        let head = TaskGroup::new();
        let transient = head.transient();

        assert!(!head.is_cancelled());
        assert!(!transient.is_cancelled());

        head.cancel();

        assert!(head.is_cancelled());
        assert!(
            transient.is_cancelled(),
            "Transient should be cancelled when head is cancelled"
        );
    }

    #[tokio::test]
    async fn test_settled_cancels_transient() {
        let head = TaskGroup::new();
        let transient = head.transient();

        assert!(!transient.is_cancelled());

        head.settled().await.unwrap();

        assert!(
            transient.is_cancelled(),
            "Transient should be cancelled when head settles"
        );
    }

    #[tokio::test]
    async fn test_transient_cancel_does_not_affect_head() {
        let head = TaskGroup::new();
        let transient = head.transient();

        transient.cancel();

        assert!(
            !head.is_cancelled(),
            "Head should NOT be cancelled when transient is cancelled"
        );
        assert!(transient.is_cancelled());
    }

    #[tokio::test]
    async fn test_transient_does_not_block_head_settled() {
        let head = TaskGroup::new();
        let transient = head.transient();

        // Spawn a long-running task on transient
        transient.spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        });

        // Head should settle immediately (no pending tasks)
        head.settled().await.unwrap();

        assert!(
            transient.is_cancelled(),
            "Transient should be cancelled after head settles"
        );
    }

    #[tokio::test]
    async fn test_nested_transient_groups() {
        let head = TaskGroup::new();
        let t1 = head.transient();
        let t2 = t1.transient();

        head.cancel();

        // Cancellation propagates through the chain
        assert!(t1.is_cancelled());
        assert!(t2.is_cancelled());
    }

    #[tokio::test]
    async fn test_task_panic_captured_as_error() {
        let group = TaskGroup::new();

        group.spawn(async {
            panic!("intentional test panic");
        });

        let result = group.settled().await;
        assert!(result.is_err(), "Panic should be captured as error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic") && err_msg.contains("intentional test panic"),
            "Error should contain panic message: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_multiple_task_errors_first_wins() {
        let group = TaskGroup::new();

        // Spawn tasks with different delays - first to fail should win
        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err(anyhow!("second error"))
        });

        group.spawn(async {
            // No delay - this should fail first
            Err(anyhow!("first error"))
        });

        let result = group.settled().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("first error"),
            "First error should win: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_task_error_and_panic_first_wins() {
        let group = TaskGroup::new();

        // Error task - no delay, should complete first
        group.spawn(async { Err(anyhow!("error before panic")) });

        // Panic task - small delay
        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            panic!("panic after error");
        });

        let result = group.settled().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("error before panic"),
            "Error should win over later panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_transient_error_does_not_affect_head() {
        let head = TaskGroup::new();
        let transient = head.transient();

        // Spawn a failing task on transient
        transient.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err(anyhow!("transient error"))
        });

        // Head should settle successfully - transient errors don't affect it
        let result = head.settled().await;
        assert!(
            result.is_ok(),
            "Head should settle OK despite transient error"
        );
    }

    #[tokio::test]
    async fn test_capture_error_only_captures_first() {
        let group = TaskGroup::new();

        group.capture_error(anyhow!("first captured"));
        group.capture_error(anyhow!("second captured"));
        group.capture_error(anyhow!("third captured"));

        let result = group.settled().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("first captured"),
            "Only first captured error should be returned: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_error_notifies_waiters() {
        let group = TaskGroup::new();

        // Spawn a task that waits a bit before erroring
        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err(anyhow!("delayed error"))
        });

        // settled() should return once error is captured, not wait indefinitely
        let result = tokio::time::timeout(Duration::from_secs(5), group.settled()).await;

        assert!(result.is_ok(), "settled() should complete after error");
        assert!(result.unwrap().is_err(), "Should have error");
    }

    #[tokio::test]
    async fn test_panic_with_string_message() {
        let group = TaskGroup::new();

        group.spawn(async {
            panic!("{}", "formatted panic message".to_string());
        });

        let result = group.settled().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("formatted panic message"),
            "Should capture formatted panic message: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_panic_with_static_str() {
        let group = TaskGroup::new();

        group.spawn(async { panic!("static str panic") });

        let result = group.settled().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("static str panic"),
            "Should capture static str panic: {}",
            err_msg
        );
    }

    // =========================================================================
    // CONCURRENT SPAWN TESTS
    // =========================================================================

    #[tokio::test]
    async fn test_many_concurrent_spawns() {
        let group = TaskGroup::new();
        let counter = Arc::new(AtomicUsize::new(0));

        const SPAWN_COUNT: usize = 100;

        for _ in 0..SPAWN_COUNT {
            let c = counter.clone();
            group.spawn(async move {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            });
        }

        group.settled().await.unwrap();

        assert_eq!(
            counter.load(Ordering::Relaxed),
            SPAWN_COUNT,
            "All spawned tasks should complete"
        );
    }

    #[tokio::test]
    async fn test_concurrent_spawns_from_multiple_tasks() {
        let group = Arc::new(TaskGroup::new());
        let counter = Arc::new(AtomicUsize::new(0));

        const SPAWNER_COUNT: usize = 10;
        const SPAWNS_PER_SPAWNER: usize = 10;

        let mut handles = Vec::new();
        for _ in 0..SPAWNER_COUNT {
            let g = group.clone();
            let c = counter.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..SPAWNS_PER_SPAWNER {
                    let c2 = c.clone();
                    g.spawn(async move {
                        c2.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    });
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        group.settled().await.unwrap();

        assert_eq!(
            counter.load(Ordering::Relaxed),
            SPAWNER_COUNT * SPAWNS_PER_SPAWNER,
            "All tasks spawned from multiple spawners should complete"
        );
    }

    #[tokio::test]
    async fn test_spawn_while_settling() {
        let group = Arc::new(TaskGroup::new());
        let counter = Arc::new(AtomicUsize::new(0));

        // Spawn a task that spawns more tasks
        let g = group.clone();
        let c = counter.clone();
        group.spawn(async move {
            for _ in 0..5 {
                let c2 = c.clone();
                g.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    c2.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                });
            }
            Ok(())
        });

        group.settled().await.unwrap();

        assert_eq!(
            counter.load(Ordering::Relaxed),
            5,
            "Tasks spawned during settlement should complete"
        );
    }

    // =========================================================================
    // EDGE CASE TESTS
    // =========================================================================

    #[tokio::test]
    async fn test_empty_group_settles_immediately() {
        let group = TaskGroup::new();
        let start = std::time::Instant::now();
        group.settled().await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "Empty group should settle immediately"
        );
    }

    #[tokio::test]
    async fn test_settle_multiple_times() {
        let group = TaskGroup::new();

        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(())
        });

        // First settle
        group.settled().await.unwrap();

        // Second settle should also work (no pending tasks)
        group.settled().await.unwrap();
    }

    #[tokio::test]
    async fn test_cancel_idempotent() {
        let group = TaskGroup::new();

        group.spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        });

        // Cancel multiple times should not panic
        group.cancel();
        group.cancel();
        group.cancel();

        assert!(group.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_empty_group() {
        let group = TaskGroup::new();
        group.cancel();
        assert!(group.is_cancelled());
    }

    #[tokio::test]
    async fn test_spawn_after_cancel() {
        let group = TaskGroup::new();
        let counter = Arc::new(AtomicUsize::new(0));

        group.cancel();

        // Spawning after cancel - task still gets spawned but may be aborted
        let c = counter.clone();
        group.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        // Give it time to potentially run
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The task may or may not have run depending on timing
        // but the group should still be in cancelled state
        assert!(group.is_cancelled());
    }

    #[tokio::test]
    async fn test_spawn_after_settle() {
        let group = Arc::new(TaskGroup::new());

        group.settled().await.unwrap();

        // Spawning after settle
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        group.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        // Wait for the new task
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Task should have run
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_deeply_nested_transient_groups() {
        let head = TaskGroup::new();
        let t1 = head.transient();
        let t2 = t1.transient();
        let t3 = t2.transient();
        let t4 = t3.transient();

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        t4.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        // When head settles, all transient groups should be cancelled
        head.settled().await.unwrap();

        // Check cascade cancellation
        assert!(t1.is_cancelled());
        assert!(t2.is_cancelled());
        assert!(t3.is_cancelled());
        assert!(t4.is_cancelled());
    }

    #[tokio::test]
    async fn test_error_during_settle_wait() {
        let group = TaskGroup::new();

        // Spawn a task that will error after a delay
        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err(anyhow!("delayed error during settle"))
        });

        // Also spawn a successful task
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        group.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        let result = group.settled().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("delayed error"));
    }

    #[tokio::test]
    async fn test_head_or_self_for_head_group() {
        let head = TaskGroup::new();
        let head_or_self = head.head_or_self();

        // head_or_self should return the same Arc for a head group
        assert!(Arc::ptr_eq(&head, &head_or_self));
    }

    #[tokio::test]
    async fn test_head_or_self_for_transient_group() {
        let head = TaskGroup::new();
        let transient = head.transient();
        let head_from_transient = transient.head_or_self();

        // head_or_self on transient should return the head
        assert!(Arc::ptr_eq(&head, &head_from_transient));
    }

    #[tokio::test]
    async fn test_multiple_transients_same_head() {
        let head = TaskGroup::new();
        let t1 = head.transient();
        let t2 = head.transient();
        let t3 = head.transient();

        // All transients should point to the same head
        assert!(Arc::ptr_eq(&head, &t1.head_or_self()));
        assert!(Arc::ptr_eq(&head, &t2.head_or_self()));
        assert!(Arc::ptr_eq(&head, &t3.head_or_self()));

        // Cancelling head should cancel all transients
        head.cancel();
        assert!(t1.is_cancelled());
        assert!(t2.is_cancelled());
        assert!(t3.is_cancelled());
    }

    // =========================================================================
    // PANIC EDGE CASES
    // =========================================================================

    #[tokio::test]
    async fn test_panic_with_box_dyn_error() {
        let group = TaskGroup::new();

        group.spawn(async {
            let error: Box<dyn std::error::Error + Send + Sync> =
                Box::new(std::io::Error::other("boxed error"));
            std::panic::panic_any(error);
        });

        let result = group.settled().await;
        assert!(result.is_err());
        // Should capture as unknown panic since it's not a string type
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic"),
            "Should identify as panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_panic_in_multiple_tasks_concurrent() {
        let group = TaskGroup::new();

        // Spawn multiple tasks that all panic at roughly the same time
        for i in 0..5 {
            group.spawn(async move { panic!("concurrent panic {}", i) });
        }

        let result = group.settled().await;
        assert!(result.is_err());
        // Should have captured one of the panics
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic"),
            "Should capture concurrent panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_panic_after_successful_tasks() {
        let group = TaskGroup::new();
        let success_counter = Arc::new(AtomicUsize::new(0));

        // Spawn successful tasks first
        for _ in 0..5 {
            let c = success_counter.clone();
            group.spawn(async move {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            });
        }

        // Then spawn a panicking task with delay
        group.spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            panic!("delayed panic");
        });

        let result = group.settled().await;
        assert!(result.is_err());

        // Successful tasks should have completed
        assert_eq!(
            success_counter.load(Ordering::Relaxed),
            5,
            "Successful tasks should complete before panic"
        );
    }

    // =========================================================================
    // TIMING AND ORDERING TESTS
    // =========================================================================

    #[tokio::test]
    async fn test_task_completion_order() {
        let group = TaskGroup::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Spawn tasks with different delays
        for i in (0..5).rev() {
            let o = order.clone();
            let delay = i * 10;
            group.spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                o.lock().push(i);
                Ok(())
            });
        }

        group.settled().await.unwrap();

        let completed_order = order.lock().clone();
        // Tasks should complete in reverse order of their delays (0 first, then 1, etc.)
        assert_eq!(completed_order, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_settle_timeout_behavior() {
        let group = TaskGroup::new();

        // Spawn a long-running task
        group.spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        });

        // Settle with timeout should timeout
        let result = tokio::time::timeout(Duration::from_millis(100), group.settled()).await;

        assert!(
            result.is_err(),
            "Settle should timeout when tasks are still running"
        );

        // Clean up by cancelling
        group.cancel();
    }
}
