use crate::models::{
    recovery::{IngestionCheckpoint, IngressRange},
    TurboError, TurboResult,
};
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::trace;

/// Task coordinator for managing concurrent operations
pub struct TaskCoordinator {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl TaskCoordinator {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        }
    }

    pub async fn acquire_permit(&self) -> TaskPermit {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        trace!(
            "Acquired task permit, available: {}",
            self.semaphore.available_permits()
        );
        TaskPermit { _permit: permit }
    }

    pub fn get_current_task_count(&self) -> usize {
        self.max_concurrent - self.semaphore.available_permits()
    }

    pub fn get_max_concurrent(&self) -> usize {
        self.max_concurrent
    }
}

pub struct TaskPermit {
    _permit: OwnedSemaphorePermit,
}

/// Retains completed ingress ranges until they form a contiguous durable prefix.
#[derive(Debug, Clone)]
pub struct CompletionFrontier {
    next_ordinal: u64,
    durable_checkpoint_ordinal: Option<u64>,
    pending: BTreeMap<u64, IngressRange>,
}

impl CompletionFrontier {
    pub fn new(checkpoint: Option<&IngestionCheckpoint>) -> Self {
        Self {
            next_ordinal: checkpoint
                .map(|checkpoint| checkpoint.ingress_ordinal.saturating_add(1))
                .unwrap_or(1),
            durable_checkpoint_ordinal: checkpoint.map(|checkpoint| checkpoint.ingress_ordinal),
            pending: BTreeMap::new(),
        }
    }

    /// Records successful completion and returns a newly contiguous checkpoint.
    pub fn record_completed(
        &mut self,
        range: IngressRange,
    ) -> TurboResult<Option<IngestionCheckpoint>> {
        if !range.is_valid() {
            return Err(TurboError::InvalidMessage(format!(
                "invalid ingress range {}..={}",
                range.start_ordinal, range.end_ordinal
            )));
        }
        if range.end_ordinal < self.next_ordinal {
            return Ok(None);
        }

        self.pending
            .entry(range.start_ordinal)
            .and_modify(|current| {
                if range.end_ordinal > current.end_ordinal {
                    *current = range.clone();
                }
            })
            .or_insert(range);

        let mut advanced = None;
        loop {
            let candidate_key =
                self.pending
                    .range(..=self.next_ordinal)
                    .rev()
                    .find_map(|(start, range)| {
                        (range.end_ordinal >= self.next_ordinal).then_some(*start)
                    });
            let Some(candidate_key) = candidate_key else {
                break;
            };
            let range = self.pending.remove(&candidate_key).ok_or_else(|| {
                TurboError::Internal("completion frontier lost a pending range".to_string())
            })?;

            self.next_ordinal = range.end_ordinal.saturating_add(1);
            self.durable_checkpoint_ordinal = Some(range.end_ordinal);
            advanced = Some(IngestionCheckpoint {
                ingress_ordinal: range.end_ordinal,
                cursor: range.end_cursor,
                updated_at: Utc::now(),
            });
            self.pending
                .retain(|_, pending| pending.end_ordinal >= self.next_ordinal);
        }

        Ok(advanced)
    }

    /// Filtered source events are complete without downstream side effects.
    pub fn record_filtered(
        &mut self,
        range: IngressRange,
    ) -> TurboResult<Option<IngestionCheckpoint>> {
        self.record_completed(range)
    }

    /// Failed work deliberately leaves the frontier unchanged.
    pub fn record_failed(&self, range: &IngressRange) -> TurboResult<()> {
        if !range.is_valid() {
            return Err(TurboError::InvalidMessage(format!(
                "invalid failed ingress range {}..={}",
                range.start_ordinal, range.end_ordinal
            )));
        }
        Ok(())
    }

    pub fn pending_range_count(&self) -> usize {
        self.pending.len()
    }

    pub fn durable_checkpoint_ordinal(&self) -> Option<u64> {
        self.durable_checkpoint_ordinal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::recovery::{SourceCursor, SourceEventId};

    fn range(start: u64, end: u64) -> IngressRange {
        let cursor = |ordinal| SourceCursor {
            time_us: ordinal * 1_000,
            source_seq: Some(ordinal * 10),
            source_event_id: SourceEventId::from(format!("event-{ordinal}")),
        };
        IngressRange {
            start_ordinal: start,
            end_ordinal: end,
            start_cursor: cursor(start),
            end_cursor: cursor(end),
        }
    }

    #[tokio::test]
    async fn test_task_coordinator_basic() {
        let coordinator = TaskCoordinator::new(2);

        assert_eq!(coordinator.get_max_concurrent(), 2);
        assert_eq!(coordinator.get_current_task_count(), 0);

        {
            let _permit1 = coordinator.acquire_permit().await;
            assert_eq!(coordinator.get_current_task_count(), 1);

            {
                let _permit2 = coordinator.acquire_permit().await;
                assert_eq!(coordinator.get_current_task_count(), 2);
            }

            // Permit 2 is dropped here
            assert_eq!(coordinator.get_current_task_count(), 1);
        }

        // Permit 1 is dropped here
        assert_eq!(coordinator.get_current_task_count(), 0);
    }

    #[test]
    fn completion_frontier_retains_later_range_across_gap() {
        let mut frontier = CompletionFrontier::new(None);

        let checkpoint = frontier.record_completed(range(3, 4)).unwrap();

        assert_eq!(checkpoint, None);
        assert_eq!(frontier.pending_range_count(), 1);
    }

    #[test]
    fn completion_frontier_coalesces_when_gap_completes() {
        let mut frontier = CompletionFrontier::new(None);
        frontier.record_completed(range(3, 4)).unwrap();

        let checkpoint = frontier.record_completed(range(1, 2)).unwrap().unwrap();

        assert_eq!(checkpoint.ingress_ordinal, 4);
        assert_eq!(checkpoint.cursor.time_us, 4_000);
        assert_eq!(frontier.pending_range_count(), 0);
    }

    #[test]
    fn completion_frontier_uses_last_cursor_by_ordinal_when_time_regresses() {
        let mut frontier = CompletionFrontier::new(None);
        let mut first = range(1, 2);
        first.start_cursor.time_us = 10_000;
        first.end_cursor.time_us = 9_000;
        let mut second = range(3, 4);
        second.start_cursor.time_us = 8_000;
        second.end_cursor.time_us = 7_000;

        frontier.record_completed(second).unwrap();
        let checkpoint = frontier.record_completed(first).unwrap().unwrap();

        assert_eq!(checkpoint.ingress_ordinal, 4);
        assert_eq!(checkpoint.cursor.time_us, 7_000);
        assert_eq!(checkpoint.cursor.source_event_id.as_str(), "event-4");
    }

    #[test]
    fn completion_frontier_advances_for_filtered_events() {
        let mut frontier = CompletionFrontier::new(None);

        let checkpoint = frontier.record_filtered(range(1, 1)).unwrap().unwrap();

        assert_eq!(checkpoint.ingress_ordinal, 1);
    }

    #[test]
    fn completion_frontier_failure_does_not_fill_gap() {
        let mut frontier = CompletionFrontier::new(None);
        let failed = range(1, 2);
        frontier.record_completed(range(3, 4)).unwrap();

        frontier.record_failed(&failed).unwrap();

        assert_eq!(frontier.pending_range_count(), 1);
        assert_eq!(frontier.record_completed(range(5, 6)).unwrap(), None);
    }
}
