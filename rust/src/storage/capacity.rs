//! Whole-store allocation, growth admission and per-volume reclamation preflight.
use super::SQLiteStore;
use crate::models::{TurboError, TurboResult};
use serde::Serialize;
use std::{path::Path, sync::atomic::Ordering};

pub const MIB: u64 = 1024 * 1024;
pub const EMERGENCY_FLOOR: u64 = 2048 * MIB;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReclamationMetrics {
    pub failures_total: u64,
    pub busy_checkpoints_total: u64,
    pub last_failure_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct StoragePolicy {
    pub target_bytes: u64,
    pub reserve_bytes: u64,
    /// Upper allowance for admitted payloads, serialization and WAL amplification.
    pub growth_bytes: u64,
    pub max_recovery_batch_bytes: u64,
}

impl Default for StoragePolicy {
    fn default() -> Self {
        // Unconfigured test stores retain their historical admission behavior.
        Self {
            target_bytes: u64::MAX,
            reserve_bytes: 0,
            growth_bytes: 0,
            max_recovery_batch_bytes: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageCapacity {
    pub reclamation: ReclamationMetrics,
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub allocated_bytes: u64,
    pub occupied_bytes: u64,
    pub freelist_bytes: u64,
    pub free_bytes: Option<u64>,
    pub target_bytes: u64,
    pub reserve_bytes: u64,
    pub growth_allowance_bytes: u64,
    pub protected_records: u64,
    pub protected_payload_bytes: u64,
    pub protected_receipts: u64,
    pub protected_receipts_capped: bool,
    /// Payload diagnostics inspect at most 1000 rows, never a whole history scan.
    pub protected_payload_sampled: bool,
    pub admission_allowed: bool,
    pub reason: &'static str,
}

pub fn vacuum_requirements(main: u64, reserve: u64, growth: u64, same_volume: bool) -> (u64, u64) {
    let allowance = growth.max(EMERGENCY_FLOOR);
    if same_volume {
        let required = reserve.max(main.saturating_mul(2).saturating_add(allowance));
        (required, required)
    } else {
        (
            reserve.max(main.saturating_add(allowance)),
            main.saturating_add(allowance),
        )
    }
}

#[cfg(unix)]
fn same_volume(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(a)?.dev() == std::fs::metadata(b)?.dev())
}

#[cfg(not(unix))]
fn same_volume(_a: &Path, _b: &Path) -> std::io::Result<bool> {
    // Conservatively assume one volume when device IDs are unavailable.
    Ok(true)
}

impl SQLiteStore {
    pub fn record_reclamation_failure(&self, error: &TurboError) {
        let description = error.to_string();
        let reason = if description.contains("checkpoint busy") {
            "busy_checkpoint"
        } else if description.contains("VACUUM requires") {
            "capacity"
        } else {
            "sqlite"
        };
        let mut metrics = self
            .reclamation_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        metrics.failures_total = metrics.failures_total.saturating_add(1);
        metrics.last_failure_reason = Some(reason);
    }

    pub(super) fn record_busy_checkpoint(&self) {
        let mut metrics = self
            .reclamation_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        metrics.busy_checkpoints_total = metrics.busy_checkpoints_total.saturating_add(1);
    }
    pub async fn allocated_size_bytes(&self) -> TurboResult<i64> {
        if self.database_path() == Path::new(":memory:") {
            return self.get_db_size().await;
        }
        let main = tokio::fs::metadata(self.database_path()).await?.len();
        let wal = match tokio::fs::metadata(format!("{}-wal", self.database_path().display())).await
        {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(main.saturating_add(wal).min(i64::MAX as u64) as i64)
    }

    pub(super) fn recovery_batch_limit(&self) -> u64 {
        self.capacity_policy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .max_recovery_batch_bytes
    }
    pub fn configure_capacity(&self, policy: StoragePolicy) {
        *self
            .capacity_policy
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = policy;
    }

    pub fn admission_allowed(&self) -> bool {
        !self.disk_pressure.load(Ordering::Acquire)
            && !self.maintenance_active.load(Ordering::Acquire)
    }

    pub fn set_disk_pressure(&self, pressure: bool) {
        self.disk_pressure.store(pressure, Ordering::Release);
    }

    pub fn set_maintenance_active(&self, active: bool) {
        self.maintenance_active.store(active, Ordering::Release);
    }

    /// Refresh physical admission between batches without scanning payloads or
    /// waiting for the SQLite writer. One already admitted batch may settle.
    pub async fn refresh_admission(&self) -> TurboResult<()> {
        if self.database_path() == Path::new(":memory:") {
            return Ok(());
        }
        let policy = *self
            .capacity_policy
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let main = tokio::fs::metadata(self.database_path()).await?.len();
        let wal_path = format!("{}-wal", self.database_path().display());
        let wal = match tokio::fs::metadata(wal_path).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        let free = fs4::available_space(self.database_path())?;
        let pressure = main.saturating_add(wal).saturating_add(policy.growth_bytes)
            > policy.target_bytes
            || free < policy.reserve_bytes.saturating_add(policy.growth_bytes);
        if self.disk_pressure.swap(pressure, Ordering::AcqRel) != pressure {
            tracing::info!(
                pressure,
                allocated_bytes = main.saturating_add(wal),
                free_bytes = free,
                "Storage admission changed"
            );
        }
        Ok(())
    }

    pub async fn capacity_snapshot(&self) -> TurboResult<StorageCapacity> {
        let policy = *self
            .capacity_policy
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (pages, page_size, free_pages): (i64, i64, i64) = sqlx::query_as(
            "SELECT page_count,page_size,freelist_count FROM pragma_page_count(),pragma_page_size(),pragma_freelist_count()"
        ).fetch_one(self.pool()).await?;
        let memory = self.database_path() == Path::new(":memory:");
        let main_bytes = if memory {
            (pages * page_size) as u64
        } else {
            tokio::fs::metadata(self.database_path()).await?.len()
        };
        let wal_bytes = if memory {
            0
        } else {
            let wal = format!("{}-wal", self.database_path().display());
            match tokio::fs::metadata(wal).await {
                Ok(m) => m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                Err(e) => return Err(e.into()),
            }
        };
        let free_bytes = if memory {
            None
        } else {
            Some(fs4::available_space(self.database_path())?)
        };
        let (protected_records, protected_payload_bytes): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*),COALESCE(SUM(length(message)+COALESCE(length(message_metadata),0)),0) FROM (SELECT source_event_id,message,message_metadata FROM records ORDER BY rowid LIMIT 1000) r WHERE r.source_event_id IS NULL OR NOT EXISTS (SELECT 1 FROM completion_receipts c WHERE c.source_event_id=r.source_event_id) OR EXISTS (SELECT 1 FROM publication_journal j WHERE j.source_event_id=r.source_event_id)"
        ).fetch_one(self.pool()).await?;
        let protected_receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM (SELECT 1 FROM completion_receipts WHERE covered=0 LIMIT 10000)",
        )
        .fetch_one(self.pool())
        .await?;
        let allocated_bytes = main_bytes.saturating_add(wal_bytes);
        let freelist_bytes = (free_pages * page_size) as u64;
        let capacity_reason = if free_bytes
            .is_some_and(|free| free < policy.reserve_bytes.saturating_add(policy.growth_bytes))
        {
            "free_space_reserve"
        } else if allocated_bytes.saturating_add(policy.growth_bytes) > policy.target_bytes {
            "aggregate_target"
        } else {
            "ready"
        };
        self.disk_pressure
            .store(capacity_reason != "ready", Ordering::Release);
        let reason = if self.maintenance_active.load(Ordering::Acquire) {
            "maintenance"
        } else {
            capacity_reason
        };
        let snapshot = StorageCapacity {
            reclamation: self
                .reclamation_metrics
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            main_bytes,
            wal_bytes,
            allocated_bytes,
            occupied_bytes: ((pages - free_pages).max(0) * page_size) as u64,
            freelist_bytes,
            free_bytes,
            target_bytes: policy.target_bytes,
            reserve_bytes: policy.reserve_bytes,
            growth_allowance_bytes: policy.growth_bytes,
            protected_records: protected_records as u64,
            protected_payload_bytes: protected_payload_bytes as u64,
            protected_receipts: protected_receipts as u64,
            protected_receipts_capped: protected_receipts == 10000,
            protected_payload_sampled: true,
            admission_allowed: reason == "ready",
            reason,
        };
        for (kind, bytes) in [
            ("allocated", allocated_bytes),
            ("occupied", snapshot.occupied_bytes),
            ("freelist", freelist_bytes),
            ("wal", wal_bytes),
            ("protected_payload", snapshot.protected_payload_bytes),
        ] {
            metrics::gauge!("sqlite_storage_bytes", "kind" => kind).set(bytes as f64);
        }
        if let Some(free) = free_bytes {
            metrics::gauge!("sqlite_free_bytes").set(free as f64);
        }
        metrics::gauge!("sqlite_admission_allowed").set(if snapshot.admission_allowed {
            1.0
        } else {
            0.0
        });
        metrics::gauge!("sqlite_protected_receipts").set(snapshot.protected_receipts as f64);
        Ok(snapshot)
    }

    pub fn verify_vacuum_capacity(&self, main: u64, temp_dir: &Path) -> TurboResult<()> {
        if self.database_path() == Path::new(":memory:") {
            return Ok(());
        }
        let db_dir = self
            .database_path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let policy = *self
            .capacity_policy
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let shared = same_volume(db_dir, temp_dir)?;
        let (db_required, temp_required) =
            vacuum_requirements(main, policy.reserve_bytes, policy.growth_bytes, shared);
        for (volume, required) in [(db_dir, db_required), (temp_dir, temp_required)] {
            let available = fs4::available_space(volume)?;
            if available < required {
                metrics::counter!("sqlite_reclamation_deferrals_total", "reason" => "capacity")
                    .increment(1);
                return Err(TurboError::Internal(format!(
                    "VACUUM requires {required} free bytes on {} (available {available}); use offline alternate-volume migration", volume.display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_reserve_is_not_counted_twice() {
        assert_eq!(
            vacuum_requirements(6 * MIB * 1024, 16 * MIB * 1024, EMERGENCY_FLOOR, true),
            (16 * MIB * 1024, 16 * MIB * 1024)
        );
        assert_eq!(
            vacuum_requirements(33 * MIB * 1024, 16 * MIB * 1024, 0, true).0,
            68 * MIB * 1024
        );
    }

    #[test]
    fn separate_volumes_each_cover_their_contribution() {
        assert_eq!(
            vacuum_requirements(6 * MIB * 1024, 16 * MIB * 1024, 0, false),
            (16 * MIB * 1024, 8 * MIB * 1024)
        );
    }

    async fn store(path: &Path) -> SQLiteStore {
        let config = super::super::SQLitePragmaConfig {
            cache_size_kib: 1024,
            mmap_size_mb: 0,
            journal_size_limit_mb: 16,
        };
        SQLiteStore::maintain_schema(path, config, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        SQLiteStore::new_with_pool_limit(path, config, 1)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn wal_allocation_gates_and_recovers_admission() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir.path().join("wal.db")).await;
        sqlx::query("PRAGMA wal_autocheckpoint=0")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE pressure(payload BLOB)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO pressure VALUES (zeroblob(1048576))")
            .execute(store.pool())
            .await
            .unwrap();
        let before = store.capacity_snapshot().await.unwrap();
        assert!(before.wal_bytes > 0);
        let target = before.main_bytes + before.wal_bytes / 2;
        assert!(before.main_bytes < target);
        store.configure_capacity(StoragePolicy {
            target_bytes: target,
            reserve_bytes: 0,
            growth_bytes: 0,
            ..StoragePolicy::default()
        });
        let gated = store.capacity_snapshot().await.unwrap();
        assert_eq!(gated.reason, "aggregate_target");
        assert_eq!(gated.allocated_bytes, gated.main_bytes + gated.wal_bytes);
        assert!(!store.admission_allowed());
        store.configure_capacity(StoragePolicy::default());
        assert!(store.capacity_snapshot().await.unwrap().admission_allowed);
    }

    #[tokio::test]
    async fn low_free_space_and_maintenance_preserve_protected_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir.path().join("reserve.db")).await;
        sqlx::query("INSERT INTO records(message,created_at,hydrated_at) VALUES ('{}','2000-01-01','2000-01-01')").execute(store.pool()).await.unwrap();
        store.configure_capacity(StoragePolicy {
            target_bytes: u64::MAX,
            reserve_bytes: u64::MAX,
            growth_bytes: 0,
            ..StoragePolicy::default()
        });
        assert_eq!(
            store.capacity_snapshot().await.unwrap().reason,
            "free_space_reserve"
        );
        assert_eq!(
            store
                .cleanup_old_records(chrono::Utc::now(), 100, 0, None)
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.count_records().await.unwrap(), 1);
        store.configure_capacity(StoragePolicy::default());
        store.set_maintenance_active(true);
        assert_eq!(
            store.capacity_snapshot().await.unwrap().reason,
            "maintenance"
        );
        assert!(!store.admission_allowed());
        store.set_maintenance_active(false);
        assert!(store.capacity_snapshot().await.unwrap().admission_allowed);
    }

    #[tokio::test]
    async fn deletion_reports_reusable_pages_and_override_cannot_skip_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir.path().join("free.db")).await;
        sqlx::query("CREATE TABLE pressure(payload BLOB)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO pressure VALUES (zeroblob(1048576))")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(store.pool())
            .await
            .unwrap();
        let before = store.capacity_snapshot().await.unwrap();
        sqlx::query("DELETE FROM pressure")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(store.pool())
            .await
            .unwrap();
        let after = store.capacity_snapshot().await.unwrap();
        assert_eq!(before.main_bytes, after.main_bytes);
        assert!(after.freelist_bytes > before.freelist_bytes);
        assert!(after.occupied_bytes < before.occupied_bytes);
        store.configure_capacity(StoragePolicy {
            growth_bytes: u64::MAX,
            ..StoragePolicy::default()
        });
        store.record_vacuum_forced();
        assert!(store
            .run_vacuum(i64::MAX, &super::super::VacuumRunPolicy::default())
            .await
            .is_err());
        assert_eq!(
            store.capacity_snapshot().await.unwrap().main_bytes,
            after.main_bytes
        );
    }

    #[tokio::test]
    async fn vacuum_reports_observed_main_and_wal_reclamation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir.path().join("physical.db")).await;
        sqlx::query("CREATE TABLE pressure(payload BLOB)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO pressure VALUES (zeroblob(1048576))")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM pressure")
            .execute(store.pool())
            .await
            .unwrap();
        let result = store
            .run_vacuum(i64::MAX, &super::super::VacuumRunPolicy::default())
            .await
            .unwrap();
        let actual = store.capacity_snapshot().await.unwrap();
        assert_eq!(result.size_after as u64, actual.allocated_bytes);
        assert_eq!(actual.wal_bytes, 0);
        assert!(result.bytes_reclaimed > 0);
        assert_eq!(
            result.bytes_reclaimed,
            result.size_before - result.size_after
        );
    }

    #[tokio::test]
    async fn busy_reader_preserves_pending_reclamation_until_checkpoint_can_finish() {
        use sqlx::Connection;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.db");
        let store = store(&path).await;
        sqlx::query("CREATE TABLE pressure(payload BLOB)")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO pressure VALUES (zeroblob(8192))")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(store.checkpoint_wal().await.unwrap());
        let mut reader = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query("BEGIN").execute(&mut reader).await.unwrap();
        let _: i64 = sqlx::query_scalar("SELECT count(*) FROM pressure")
            .fetch_one(&mut reader)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pressure")
            .execute(store.pool())
            .await
            .unwrap();
        store.check_vacuum_bloat(1, 0.1).await.unwrap();
        assert!(store
            .run_vacuum(i64::MAX, &super::super::VacuumRunPolicy::default())
            .await
            .is_err());
        assert!(store.get_vacuum_state().pending);
        assert!(
            store
                .capacity_snapshot()
                .await
                .unwrap()
                .reclamation
                .busy_checkpoints_total
                > 0
        );
        reader.close().await.unwrap();
        store
            .run_vacuum(i64::MAX, &super::super::VacuumRunPolicy::default())
            .await
            .unwrap();
        assert!(!store.get_vacuum_state().pending);
    }
}
