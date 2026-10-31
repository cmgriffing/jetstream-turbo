use jetstream_turbo_rs::hydration::entity_cache::{
    CacheKind, PersistentCacheConfig, PersistentEntityCache,
};
use jetstream_turbo_rs::models::enriched::EnrichedRecord;
use jetstream_turbo_rs::models::jetstream::{JetstreamMessage, MessageKind};
use jetstream_turbo_rs::models::recovery::{IngestionCheckpoint, SourceCursor};
use jetstream_turbo_rs::storage::{RecordStore, SQLitePragmaConfig, SQLiteStore};
use std::sync::Arc;
use std::time::Duration;

fn record(time_us: u64) -> EnrichedRecord {
    EnrichedRecord::new_with_timestamp(
        JetstreamMessage {
            did: "did:plc:maintenance-test".into(),
            time_us: Some(time_us),
            seq: None,
            kind: MessageKind::Identity,
            commit: None,
            raw_json: None,
        },
        chrono::Utc::now(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_and_bounded_cache_maintenance_both_progress_on_shared_sqlite() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("concurrent-maintenance.db");
        let pragmas = SQLitePragmaConfig {
            cache_size_kib: 128,
            mmap_size_mb: 0,
            journal_size_limit_mb: 2,
        };
        SQLiteStore::maintain_schema(&path, pragmas, Duration::from_secs(1))
            .await
            .unwrap();
        let store = Arc::new(
            SQLiteStore::new_with_pool_limit(&path, pragmas, 4)
                .await
                .unwrap(),
        );
        let cache = Arc::new(
            PersistentEntityCache::new(
                store.pool().clone(),
                PersistentCacheConfig {
                    queue_capacity: 32,
                    timeout: Duration::from_millis(200),
                    ..PersistentCacheConfig::default()
                },
            )
            .await
            .unwrap(),
        );
        cache.set_max_bytes(4096);

        // An unrelated prior-epoch pending payload must survive every cleanup,
        // even as this epoch commits progress far beyond its source timestamp.
        let pending = record(1);
        store.store_batch(&[pending.clone()]).await.unwrap();

        // Seed disposable state so eviction progress is observable independently
        // of optional producer drops under contention.
        for index in 0..8 {
            cache.store(
                CacheKind::Profile,
                format!("seed-{index}"),
                &"disposable",
                cache.now_ms(),
                cache.generation(),
                cache.next_fetch_order(),
            );
        }
        assert!(cache.flush().await);

        let writer_store = Arc::clone(&store);
        let writer = tokio::spawn(async move {
            let mut last_checkpoint = None;
            for ordinal in 1..=50 {
                let completed = record(1_000_000 + ordinal);
                let id = completed.source_event_id();
                writer_store.store_batch(&[completed]).await.unwrap();
                writer_store
                    .bind_ingress(
                        "maintenance-epoch",
                        &[(id.clone(), ordinal, 1_000_000 + ordinal)],
                    )
                    .await
                    .unwrap();
                writer_store
                    .clear_publication_journal(&[id.clone()])
                    .await
                    .unwrap();
                let checkpoint = IngestionCheckpoint {
                    ingress_ordinal: ordinal,
                    cursor: SourceCursor {
                        time_us: 1_000_000 + ordinal,
                        source_seq: None,
                        source_event_id: id,
                    },
                    updated_at: chrono::Utc::now(),
                };
                assert!(writer_store
                    .advance_ingestion_checkpoint_for_epoch(&checkpoint, "maintenance-epoch")
                    .await
                    .unwrap());
                last_checkpoint = Some(checkpoint);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            last_checkpoint.unwrap()
        });
        let maintenance_store = Arc::clone(&store);
        let maintenance_cache = Arc::clone(&cache);
        let maintenance = tokio::spawn(async move {
            let mut evicted = 0;
            for _ in 0..100 {
                evicted += maintenance_cache.maintain(4096, 4, true).await.unwrap();
                assert_eq!(
                    maintenance_store
                        .cleanup_old_records(
                            chrono::Utc::now() + chrono::Duration::days(1),
                            4,
                            0,
                            None
                        )
                        .await
                        .unwrap(),
                    0
                );
                maintenance_store
                    .prune_completion_receipts(3600, 4)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            evicted
        });
        let producer_cache = Arc::clone(&cache);
        let producer = tokio::spawn(async move {
            for index in 0..100 {
                producer_cache.store(
                    CacheKind::Profile,
                    format!("churn-{index}"),
                    &format!("disposable-{index}"),
                    producer_cache.now_ms(),
                    producer_cache.generation(),
                    producer_cache.next_fetch_order(),
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let (writer, maintenance, producer) = tokio::join!(writer, maintenance, producer);
        let expected_checkpoint = writer.unwrap();
        let mut evicted = maintenance.unwrap();
        producer.unwrap();
        assert!(cache.flush().await);
        // Drain remaining optional state in bounded chunks after producers stop.
        for _ in 0..32 {
            let removed = cache.maintain(0, 4, true).await.unwrap();
            evicted += removed;
            if removed == 0 {
                break;
            }
        }
        assert!(evicted >= 8, "maintenance must reclaim the seeded entries");
        assert_eq!(cache.diagnostics().logical_bytes, 0);
        assert_eq!(
            store.load_ingestion_checkpoint().await.unwrap(),
            Some(expected_checkpoint)
        );
        let (completed, unpublished) = store
            .resolve_completion_state(&[pending.source_event_id()])
            .await
            .unwrap();
        assert!(completed.is_empty());
        assert_eq!(unpublished.len(), 1);
        assert_eq!(store.count_records().await.unwrap(), 1);
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completion_receipts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(receipts, 50);
        store.close().await.unwrap();
    })
    .await
    .expect("required recovery writes and bounded optional maintenance must both finish");
}
