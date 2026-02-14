use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct DidInterner {
    cache: RwLock<HashMap<String, Arc<str>>>,
}

impl DidInterner {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn intern(&self, did: &str) -> Arc<str> {
        {
            let cache = self.cache.read().await;
            if let Some(interned) = cache.get(did) {
                return Arc::clone(interned);
            }
        }

        let interned: Arc<str> = Arc::from(did);
        let mut cache = self.cache.write().await;
        cache.entry(did.to_string()).or_insert_with(|| interned.clone());
        interned
    }

    pub fn intern_sync(&self, did: &str) -> Arc<str> {
        {
            let cache = self.cache.blocking_read();
            if let Some(interned) = cache.get(did) {
                return Arc::clone(interned);
            }
        }

        let interned: Arc<str> = Arc::from(did);
        let mut cache = self.cache.blocking_write();
        cache.entry(did.to_string()).or_insert_with(|| interned.clone());
        interned
    }

    pub async fn clear(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    pub async fn len(&self) -> usize {
        let cache = self.cache.read().await;
        cache.len()
    }
}

impl Default for DidInterner {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct DidInternerHandle(Arc<DidInterner>);

impl DidInternerHandle {
    pub fn new() -> Self {
        Self(Arc::new(DidInterner::new()))
    }

    pub async fn intern(&self, did: &str) -> Arc<str> {
        self.0.intern(did).await
    }

    pub fn intern_sync(&self, did: &str) -> Arc<str> {
        self.0.intern_sync(did)
    }

    pub async fn clear(&self) {
        self.0.clear().await;
    }

    pub async fn len(&self) -> usize {
        self.0.len().await
    }
}

impl Default for DidInternerHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_interner() {
        let interner = DidInterner::new();

        let did1 = interner.intern("did:plc:abc123").await;
        let did2 = interner.intern("did:plc:abc123").await;

        assert!(Arc::ptr_eq(&did1, &did2));

        let did3 = interner.intern("did:plc:xyz789").await;
        assert!(!Arc::ptr_eq(&did1, &did3));
    }

    #[tokio::test]
    async fn test_interner_handle() {
        let handle = DidInternerHandle::new();

        let did1 = handle.intern("did:plc:test").await;
        let did2 = handle.intern("did:plc:test").await;

        assert!(Arc::ptr_eq(&did1, &did2));
    }
}
