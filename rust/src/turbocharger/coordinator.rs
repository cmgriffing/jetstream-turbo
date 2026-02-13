use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::debug;

/// Task coordinator for managing concurrent operations
pub struct TaskCoordinator {
    max_concurrent: usize,
    current_tasks: Arc<RwLock<usize>>,
}

impl TaskCoordinator {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            current_tasks: Arc::new(RwLock::new(0)),
        }
    }
    
    pub async fn acquire_permit(&self) -> TaskPermit {
        let mut current = self.current_tasks.write().await;
        
        while *current >= self.max_concurrent {
            debug!("Waiting for task permit, current: {}, max: {}", *current, self.max_concurrent);
            
            // Simple backoff - in a real implementation this would use a proper semaphore
            drop(current);
            tokio::time::sleep(Duration::from_millis(10)).await;
            current = self.current_tasks.write().await;
        }
        
        *current += 1;
        debug!("Acquired task permit, current tasks: {}", *current);
        
        TaskPermit {
            current_tasks: self.current_tasks.clone(),
        }
    }
    
    pub async fn get_current_task_count(&self) -> usize {
        *self.current_tasks.read().await
    }
    
    pub fn get_max_concurrent(&self) -> usize {
        self.max_concurrent
    }
}

pub struct TaskPermit {
    current_tasks: Arc<RwLock<usize>>,
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        let mut current = self.current_tasks.try_write().unwrap();
        let count = (*current).saturating_sub(1);
        *current = count;
        debug!("Released task permit, current tasks: {}", count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_task_coordinator_basic() {
        let coordinator = TaskCoordinator::new(2);
        
        assert_eq!(coordinator.get_max_concurrent(), 2);
        assert_eq!(coordinator.get_current_task_count().await, 0);
        
        {
            let _permit1 = coordinator.acquire_permit().await;
            assert_eq!(coordinator.get_current_task_count().await, 1);
            
            {
                let _permit2 = coordinator.acquire_permit().await;
                assert_eq!(coordinator.get_current_task_count().await, 2);
            }
            
            // Permit 2 is dropped here
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(coordinator.get_current_task_count().await, 1);
        }
        
        // Permit 1 is dropped here
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(coordinator.get_current_task_count().await, 0);
    }
}