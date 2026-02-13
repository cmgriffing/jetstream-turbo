// Connection pool management for API clients
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tracing::debug;

pub struct ClientPool<T> {
    clients: Arc<RwLock<Vec<PooledClient<T>>>>,
    #[allow(dead_code)]
    max_size: usize,
    semaphore: Arc<Semaphore>,
    client_factory: Arc<dyn Fn() -> T + Send + Sync>,
}

#[derive(Debug, Clone)]
pub struct PooledClient<T> {
    client: T,
    created_at: Instant,
    last_used: Instant,
    usage_count: u64,
}

impl<T> PooledClient<T> {
    pub fn new(client: T) -> Self {
        let now = Instant::now();
        Self {
            client,
            created_at: now,
            last_used: now,
            usage_count: 0,
        }
    }
    
    pub fn get_client(&self) -> &T {
        &self.client
    }
    
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
        self.usage_count += 1;
    }
    
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
    
    pub fn idle_time(&self) -> Duration {
        self.last_used.elapsed()
    }
}

impl<T> ClientPool<T>
where
    T: Clone + Send + Sync + 'static
{
    pub fn new<F>(max_size: usize, factory: F) -> Self 
    where 
        F: Fn() -> T + Send + Sync + 'static
    {
        Self {
            clients: Arc::new(RwLock::new(Vec::new())),
            max_size,
            semaphore: Arc::new(Semaphore::new(max_size)),
            client_factory: Arc::new(factory),
        }
    }
    
    pub async fn get(&self) -> PooledClientGuard<T> {
        let _permit = self.semaphore.acquire().await.unwrap();
        
        // Try to get an existing client
        {
            let mut clients = self.clients.write().await;
            if let Some(mut client) = clients.pop() {
                client.touch();
                return PooledClientGuard {
                    client: Some(client),
                    pool: self.clients.clone(),
                };
            }
        }
        
        // Create a new client if none available
        let client = (self.client_factory)();
        let pooled_client = PooledClient::new(client);
        
        PooledClientGuard {
            client: Some(pooled_client),
            pool: self.clients.clone(),
        }
    }
    
    pub async fn cleanup_idle_clients(&self, max_idle_time: Duration) {
        let mut clients = self.clients.write().await;
        let initial_count = clients.len();
        
        clients.retain(|client| client.idle_time() <= max_idle_time);
        
        let removed = initial_count - clients.len();
        if removed > 0 {
            debug!("Cleaned up {} idle clients", removed);
        }
    }
    
    pub async fn size(&self) -> usize {
        self.clients.read().await.len()
    }
}

pub struct PooledClientGuard<T: Send + Sync + 'static> {
    client: Option<PooledClient<T>>,
    pool: Arc<RwLock<Vec<PooledClient<T>>>>,
}

impl<T: Send + Sync + 'static> Drop for PooledClientGuard<T> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                let mut clients = pool.write().await;
                clients.push(client);
            });
        }
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for PooledClientGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.client.as_ref().unwrap().client
    }
}

impl<T: Send + Sync + 'static> std::ops::DerefMut for PooledClientGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        panic!("Cannot get mutable reference to pooled client");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    
    #[tokio::test]
    async fn test_client_pool_basic_operations() {
        static CREATE_COUNT: AtomicU64 = AtomicU64::new(0);
        
        let pool = ClientPool::new(2, || {
            CREATE_COUNT.fetch_add(1, Ordering::SeqCst);
            "test_client".to_string()
        });
        
        assert_eq!(pool.size().await, 0);
        assert_eq!(CREATE_COUNT.load(Ordering::SeqCst), 0);
        
        {
            let client1 = pool.get().await;
            assert_eq!(CREATE_COUNT.load(Ordering::SeqCst), 1);
            assert_eq!(*client1, "test_client");
            assert_eq!(pool.size().await, 0);
        }
        
        // Client should be returned to pool
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(pool.size().await, 1);
        
        {
            let client2 = pool.get().await;
            assert_eq!(CREATE_COUNT.load(Ordering::SeqCst), 1); // Should reuse
            assert_eq!(*client2, "test_client");
        }
        
        assert_eq!(pool.size().await, 1);
    }
    
    #[tokio::test]
    async fn test_client_pool_multiple_clients() {
        let pool = ClientPool::new(3, || "test_client".to_string());
        
        let client1 = pool.get().await;
        let client2 = pool.get().await;
        let client3 = pool.get().await;
        
        assert_eq!(pool.size().await, 0);
        
        drop(client1);
        drop(client2);
        drop(client3);
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(pool.size().await, 3);
    }
    
    #[tokio::test]
    async fn test_client_pool_cleanup() {
        let pool = ClientPool::new(3, || "test_client".to_string());
        
        // Create and return clients
        {
            let _client1 = pool.get().await;
            let _client2 = pool.get().await;
        }
        
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(pool.size().await, 2);
        
        // Wait longer than max_idle_time and cleanup
        tokio::time::sleep(Duration::from_millis(50)).await;
        pool.cleanup_idle_clients(Duration::from_millis(25)).await;
        
        assert_eq!(pool.size().await, 0);
    }
}