//! Partitioning strategy for Kafka topics
//!
//! This module provides deterministic partitioning by correlation_id to ensure:
//! - All events for the same workflow go to the same partition
//! - Per-partition ordering guarantees per-workflow FIFO semantics
//! - Parallel processing across different workflows

use uuid::Uuid;

/// Compute the Kafka partition for a given correlation ID.
///
/// This function ensures deterministic partitioning - the same correlation_id
/// will always map to the same partition. This is critical for maintaining
/// per-workflow FIFO ordering while allowing parallel processing across workflows.
///
/// # Arguments
/// * `correlation_id` - The workflow identifier
/// * `num_partitions` - Total number of partitions in the topic
///
/// # Returns
/// The partition index (0-based)
///
/// # Example
/// ```
/// use uuid::Uuid;
/// use seesaw_kafka::partition_strategy::compute_partition;
///
/// let correlation_id = Uuid::new_v4();
/// let partition = compute_partition(&correlation_id, 16);
/// assert!(partition >= 0 && partition < 16);
///
/// // Same correlation_id always maps to same partition
/// assert_eq!(
///     compute_partition(&correlation_id, 16),
///     compute_partition(&correlation_id, 16)
/// );
/// ```
pub fn compute_partition(correlation_id: &Uuid, num_partitions: i32) -> i32 {
    // Use the UUID's 128-bit value for deterministic hashing
    let hash = correlation_id.as_u128();
    (hash % num_partitions as u128) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_determinism() {
        let id = Uuid::new_v4();
        let num_partitions = 10;

        // Same ID should always map to same partition
        let partition1 = compute_partition(&id, num_partitions);
        let partition2 = compute_partition(&id, num_partitions);
        assert_eq!(partition1, partition2);
    }

    #[test]
    fn test_partition_range() {
        let id = Uuid::new_v4();
        let num_partitions = 16;

        let partition = compute_partition(&id, num_partitions);
        assert!(partition >= 0 && partition < num_partitions);
    }

    #[test]
    fn test_partition_distribution() {
        let num_partitions = 8;
        let num_ids = 1000;

        // Generate many UUIDs and verify distribution
        let mut partition_counts = vec![0; num_partitions as usize];
        for _ in 0..num_ids {
            let id = Uuid::new_v4();
            let partition = compute_partition(&id, num_partitions);
            partition_counts[partition as usize] += 1;
        }

        // Each partition should have roughly equal distribution
        // Allow 50% variance (realistic for random UUIDs)
        let expected = num_ids / num_partitions as usize;
        for count in partition_counts {
            assert!(count > expected / 2 && count < expected * 2);
        }
    }

    #[test]
    fn test_different_ids_can_have_different_partitions() {
        let num_partitions = 16;
        let mut partitions = std::collections::HashSet::new();

        // Generate enough IDs to likely hit different partitions
        for _ in 0..100 {
            let id = Uuid::new_v4();
            let partition = compute_partition(&id, num_partitions);
            partitions.insert(partition);
        }

        // Should hit multiple partitions (very high probability)
        assert!(partitions.len() > 1);
    }
}
