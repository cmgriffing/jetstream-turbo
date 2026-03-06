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

#[cfg(test)]
mod tests {
    use super::*;

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
}
