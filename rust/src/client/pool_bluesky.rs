use crate::client::BlueskyClient;
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_CLIENT_BANDWIDTH: usize = 10;

#[derive(Clone)]
pub struct BlueskyClientPool {
    clients: Arc<RwLock<Vec<Arc<BlueskyClient>>>>,
    current_index: Arc<RwLock<usize>>,
}

impl BlueskyClientPool {
    pub async fn new(session_strings: Vec<String>) -> Self {
        let client_count = session_strings.len().min(DEFAULT_CLIENT_BANDWIDTH);
        
        let mut clients = Vec::with_capacity(client_count);
        
        for i in 0..client_count {
            let session = session_strings[i % session_strings.len()].clone();
            let client = BlueskyClient::new(vec![session]);
            clients.push(Arc::new(client));
            tracing::info!("Created Bluesky client {} of {}", i + 1, client_count);
        }
        
        tracing::info!(
            "BlueskyClientPool initialized with {} clients (total sessions: {})",
            client_count,
            session_strings.len()
        );

        Self {
            clients: Arc::new(RwLock::new(clients)),
            current_index: Arc::new(RwLock::new(0)),
        }
    }

    pub async fn get_client(&self) -> Arc<BlueskyClient> {
        let clients = self.clients.read().await;
        let mut index = self.current_index.write().await;
        
        let client = clients[*index % clients.len()].clone();
        *index = index.wrapping_add(1);
        
        client
    }

    pub async fn get_client_random(&self) -> Arc<BlueskyClient> {
        use rand::thread_rng;
        use rand::Rng;
        
        let clients = self.clients.read().await;
        let mut rng = thread_rng();
        let index = rng.gen_range(0..clients.len());
        clients[index].clone()
    }

    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }
}
