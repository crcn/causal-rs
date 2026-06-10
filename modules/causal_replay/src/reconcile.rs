//! Shared append-reconciliation decision procedure.
//!
//! Every `EventLogBackend` faces the same question when an
//! expected-revision check fails: is the incoming batch
//!
//! 1. a **crash-redelivery** — the identical batch already landed on a
//!    previous attempt (the caller crashed before observing the `Ok`),
//!    and now foreign events may sit after it — return the original
//!    `WriteResult`, never an error;
//! 2. a **true OCC conflict** — none of the batch is persisted, the
//!    stream simply moved — surface a typed
//!    [`ConflictError`](causal::event_log::ConflictError) so
//!    `Engine::append` can reload + retry; or
//! 3. a **partial overlap** — some-but-not-all of the batch is
//!    persisted, which violates the all-new-or-all-already-persisted
//!    precondition on `append_to_stream` — a caller bug that must fail
//!    loudly rather than corrupt the stream.
//!
//! One pure function owns that decision so the Postgres, Kurrent, and
//! (by parallel construction) Memory backends cannot drift. It fixes a
//! known hole in the original Kurrent reconcile, which matched only the
//! batch's LAST event_id: a torn batch whose tail landed but whose head
//! didn't was mis-classified as a clean redelivery. `reconcile` verifies
//! **every** batch id, in order.

use uuid::Uuid;

/// Outcome of [`reconcile`]. See the module docs for what each variant
/// obliges the backend to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciliation {
    /// Every batch event_id is present in the conflict window, in batch
    /// order — the batch already landed atomically on an earlier
    /// attempt. Return the ORIGINAL append's `WriteResult` (the last
    /// batch event's position + revision).
    Redelivery,
    /// No batch event_id is present — a genuine optimistic-concurrency
    /// conflict. Return a typed `ConflictError`.
    Conflict,
    /// Some (but not all) batch event_ids are present — or all are
    /// present but NOT in batch order, which a single atomic append can
    /// never produce. Either way the all-new-or-all-already-persisted
    /// precondition is violated: fail loudly.
    PartialOverlap,
}

/// Classify a conflicting append given the batch's `event_id`s (in
/// batch order) and the `event_id`s actually present in the stream's
/// conflict window (in stream/revision order).
///
/// The *conflict window* is every event the stream holds AFTER the
/// caller's expected revision — i.e. everything that could explain the
/// mismatch. For `StreamRevision(n)` that's revisions `> n`; for
/// `NoStream` it's the whole stream.
///
/// Pure and synchronous: callers read the window however their store
/// allows and pass the ids. `batch` must be non-empty (the append
/// primitive rejects empty batches long before reconciliation).
pub fn reconcile(batch: &[Uuid], window: &[Uuid]) -> Reconciliation {
    debug_assert!(
        !batch.is_empty(),
        "reconcile: batch must be non-empty (append_to_stream rejects \
         empty batches before any reconciliation can happen)"
    );

    // In-order subsequence match: every batch id must appear in the
    // window, in batch order. (Atomic appends land the batch at
    // consecutive revisions, so a genuinely redelivered batch is always
    // an ordered — in fact contiguous — run inside the window; ordered
    // is the property we need.)
    let mut window_iter = window.iter();
    let in_order = batch
        .iter()
        .filter(|id| window_iter.any(|w| w == *id))
        .count();
    if in_order == batch.len() {
        return Reconciliation::Redelivery;
    }

    // Not a clean redelivery. Distinguish "none present" (true OCC
    // conflict) from "some present / present-but-out-of-order" (torn
    // batch — loud failure).
    if batch.iter().any(|id| window.contains(id)) {
        Reconciliation::PartialOverlap
    } else {
        Reconciliation::Conflict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::new_v4()).collect()
    }

    #[test]
    fn all_present_in_order_is_redelivery() {
        let batch = ids(3);
        // Foreign events before AND after the batch run.
        let window = vec![
            Uuid::new_v4(),
            batch[0],
            batch[1],
            batch[2],
            Uuid::new_v4(),
        ];
        assert_eq!(reconcile(&batch, &window), Reconciliation::Redelivery);
    }

    #[test]
    fn none_present_is_conflict() {
        let batch = ids(2);
        let window = ids(4);
        assert_eq!(reconcile(&batch, &window), Reconciliation::Conflict);
    }

    #[test]
    fn empty_window_is_conflict() {
        let batch = ids(2);
        assert_eq!(reconcile(&batch, &[]), Reconciliation::Conflict);
    }

    #[test]
    fn tail_only_match_is_partial_overlap() {
        // THE known hole: the old Kurrent reconcile matched only the
        // batch tail and would have called this a redelivery.
        let batch = ids(3);
        let window = vec![Uuid::new_v4(), batch[2]];
        assert_eq!(reconcile(&batch, &window), Reconciliation::PartialOverlap);
    }

    #[test]
    fn head_only_match_is_partial_overlap() {
        let batch = ids(3);
        let window = vec![batch[0], Uuid::new_v4()];
        assert_eq!(reconcile(&batch, &window), Reconciliation::PartialOverlap);
    }

    #[test]
    fn all_present_out_of_order_is_partial_overlap() {
        // An atomic append can never land a batch out of order, so this
        // is corruption/caller-bug territory — must NOT be a redelivery.
        let batch = ids(2);
        let window = vec![batch[1], batch[0]];
        assert_eq!(reconcile(&batch, &window), Reconciliation::PartialOverlap);
    }

    #[test]
    fn single_event_batch_present_is_redelivery() {
        let batch = ids(1);
        let window = vec![Uuid::new_v4(), batch[0], Uuid::new_v4()];
        assert_eq!(reconcile(&batch, &window), Reconciliation::Redelivery);
    }

    #[test]
    fn single_event_batch_absent_is_conflict() {
        let batch = ids(1);
        let window = ids(3);
        assert_eq!(reconcile(&batch, &window), Reconciliation::Conflict);
    }
}
