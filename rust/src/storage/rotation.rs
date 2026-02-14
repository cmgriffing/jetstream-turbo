use crate::models::errors::TurboResult;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{error, info, trace, warn};

pub struct DatabaseRotator {
    db_dir: PathBuf,
    rotation_interval: Duration,
    max_databases: usize,
    cleanup_age: Duration,
}

impl DatabaseRotator {
    pub fn new<P: AsRef<Path>>(
        db_dir: P,
        rotation_interval: Duration,
        max_databases: usize,
        cleanup_age: Duration,
    ) -> Self {
        Self {
            db_dir: db_dir.as_ref().to_path_buf(),
            rotation_interval,
            max_databases,
            cleanup_age,
        }
    }

    pub async fn start_rotation_task(&self) -> TurboResult<tokio::task::JoinHandle<()>> {
        let db_dir = self.db_dir.clone();
        let rotation_interval = self.rotation_interval;
        let max_databases = self.max_databases;
        let cleanup_age = self.cleanup_age;

        info!("Starting database rotation task");
        info!("Rotation interval: {:?}", rotation_interval);
        info!("Max databases: {}", max_databases);
        info!("Cleanup age: {:?}", cleanup_age);

        let handle = tokio::spawn(async move {
            let mut interval = interval(rotation_interval);
            interval.tick().await; // Skip first tick

            loop {
                if let Err(e) = Self::rotate_databases(&db_dir, max_databases, cleanup_age).await {
                    error!("Database rotation failed: {}", e);
                }

                interval.tick().await;
            }
        });

        Ok(handle)
    }

    async fn rotate_databases(
        db_dir: &Path,
        max_databases: usize,
        cleanup_age: Duration,
    ) -> TurboResult<()> {
        trace!("Starting database rotation");

        // Create timestamped database name
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let new_db_name = format!("jetstream_{timestamp}.db");
        let new_db_path = db_dir.join(&new_db_name);

        info!("Creating new database: {}", new_db_name);

        // Create the new database (this will be handled by SQLiteStore)
        // For now, just ensure the directory exists
        tokio::fs::create_dir_all(db_dir).await?;

        // List existing databases
        let mut databases = Self::list_databases(db_dir).await?;

        // Add the new database to the list
        databases.push((new_db_name, new_db_path));

        // Sort by timestamp (newest first)
        databases.sort_by(|a, b| b.0.cmp(&a.0));

        // Keep only the max_databases most recent
        if databases.len() > max_databases {
            let to_remove = databases.split_off(max_databases);

            for (db_name, db_path) in to_remove {
                info!("Removing old database: {}", db_name);

                // Also remove associated files (.wal, .shm, etc.)
                if let Err(e) = Self::remove_database_files(&db_path).await {
                    warn!(
                        "Failed to remove database file {}: {}",
                        db_path.display(),
                        e
                    );
                }
            }
        }

        // Clean up very old files
        Self::cleanup_old_files(db_dir, cleanup_age).await?;

        info!("Database rotation completed");
        Ok(())
    }

    async fn list_databases(db_dir: &Path) -> TurboResult<Vec<(String, PathBuf)>> {
        let mut entries = tokio::fs::read_dir(db_dir).await?;
        let mut databases = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(file_name) = path.file_name() {
                if let Some(name_str) = file_name.to_str() {
                    if name_str.starts_with("jetstream_") && name_str.ends_with(".db") {
                        databases.push((name_str.to_string(), path));
                    }
                }
            }
        }

        Ok(databases)
    }

    async fn remove_database_files(db_path: &Path) -> TurboResult<()> {
        // Remove main database file
        if tokio::fs::metadata(db_path).await.is_ok() {
            tokio::fs::remove_file(db_path).await?;
        }

        // Remove WAL file
        let wal_path = db_path.with_extension("db-wal");
        if tokio::fs::metadata(&wal_path).await.is_ok() {
            tokio::fs::remove_file(&wal_path).await?;
        }

        // Remove SHM file
        let shm_path = db_path.with_extension("db-shm");
        if tokio::fs::metadata(&shm_path).await.is_ok() {
            tokio::fs::remove_file(&shm_path).await?;
        }

        trace!("Removed database files for: {}", db_path.display());
        Ok(())
    }

    async fn cleanup_old_files(db_dir: &Path, max_age: Duration) -> TurboResult<()> {
        let now = SystemTime::now();
        let mut entries = tokio::fs::read_dir(db_dir).await?;
        let mut removed_count = 0;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            if let Ok(metadata) = entry.metadata().await {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(age) = now.duration_since(modified) {
                        if age > max_age {
                            info!("Removing old file: {}", path.display());
                            if let Err(e) = tokio::fs::remove_file(&path).await {
                                warn!("Failed to remove old file {}: {}", path.display(), e);
                            } else {
                                removed_count += 1;
                            }
                        }
                    }
                }
            }
        }

        if removed_count > 0 {
            info!("Cleaned up {} old files", removed_count);
        }

        Ok(())
    }

    pub fn get_db_dir(&self) -> &Path {
        &self.db_dir
    }

    pub fn get_current_database_path(&self) -> PathBuf {
        // Get the most recent database
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        self.db_dir.join(format!("jetstream_{timestamp}.db"))
    }

    pub async fn ensure_directory_exists(&self) -> TurboResult<()> {
        tokio::fs::create_dir_all(&self.db_dir).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_rotator_creation() {
        let temp_dir = TempDir::new().unwrap();
        let rotator = DatabaseRotator::new(
            temp_dir.path(),
            Duration::from_secs(60),
            5,
            Duration::from_secs(3600),
        );

        assert_eq!(rotator.get_db_dir(), temp_dir.path());

        // Ensure directory creation
        rotator.ensure_directory_exists().await.unwrap();
        assert!(temp_dir.path().exists());
    }

    #[tokio::test]
    async fn test_database_listing() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test database files
        let db1 = temp_dir.path().join("jetstream_123456789.db");
        let db2 = temp_dir.path().join("jetstream_123456788.db");
        let not_db = temp_dir.path().join("other_file.txt");

        tokio::fs::write(&db1, "test").await.unwrap();
        tokio::fs::write(&db2, "test").await.unwrap();
        tokio::fs::write(&not_db, "test").await.unwrap();

        let databases = DatabaseRotator::list_databases(temp_dir.path())
            .await
            .unwrap();

        assert_eq!(databases.len(), 2);
        assert!(databases
            .iter()
            .any(|(name, _)| name == "jetstream_123456789.db"));
        assert!(databases
            .iter()
            .any(|(name, _)| name == "jetstream_123456788.db"));
        assert!(!databases.iter().any(|(name, _)| name == "other_file.txt"));
    }
}
