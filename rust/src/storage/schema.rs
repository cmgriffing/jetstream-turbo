use sqlx::{Row, SqlitePool};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{error, info};

/// A required SQLite index and the canonical DDL used to create it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredIndex {
    pub name: &'static str,
    pub sql: &'static str,
}

/// Required production indexes, shared by verification and maintenance.
pub const REQUIRED_INDEXES: &[RequiredIndex] = &[
    RequiredIndex {
        name: "idx_records_at_uri",
        sql: "CREATE INDEX idx_records_at_uri ON records(at_uri)",
    },
    RequiredIndex {
        name: "idx_records_did",
        sql: "CREATE INDEX idx_records_did ON records(did)",
    },
    RequiredIndex {
        name: "idx_records_time_us",
        sql: "CREATE INDEX idx_records_time_us ON records(time_us)",
    },
    RequiredIndex {
        name: "idx_records_created_at",
        sql: "CREATE INDEX idx_records_created_at ON records(created_at)",
    },
    RequiredIndex {
        name: "idx_records_hydration_quality",
        sql: "CREATE INDEX idx_records_hydration_quality ON records(hydration_quality)",
    },
    RequiredIndex {
        name: "idx_records_source_event_id",
        sql: "CREATE UNIQUE INDEX idx_records_source_event_id ON records(source_event_id) WHERE source_event_id IS NOT NULL",
    },
];

/// Result of bounded, read-only required-index verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaVerification {
    Ready,
    MaintenanceRequired {
        missing_indexes: Vec<String>,
        incompatible_indexes: Vec<String>,
    },
}

impl SchemaVerification {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Summary returned by a successful schema-maintenance invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMaintenanceReport {
    pub created_indexes: Vec<String>,
    pub skipped_indexes: Vec<String>,
    pub elapsed: Duration,
}

/// Typed failures emitted by explicit schema maintenance.
#[derive(Debug, Error)]
pub enum SchemaMaintenanceError {
    #[error("failed to prepare the SQLite schema for maintenance: {0}")]
    Preparation(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("SQLite lock timeout while maintaining index {index} after {timeout:?}: {source}")]
    LockTimeout {
        index: String,
        timeout: Duration,
        #[source]
        source: Box<sqlx::Error>,
    },

    #[error("required index {index} has an incompatible definition; expected `{expected}`, found `{actual}`")]
    InvalidIndexDefinition {
        index: String,
        expected: String,
        actual: String,
    },

    #[error("failed to create required index {index}: {source}")]
    Ddl {
        index: String,
        #[source]
        source: Box<sqlx::Error>,
    },

    #[error("failed to verify required indexes: {0}")]
    Verification(#[source] Box<sqlx::Error>),
}

/// Inspects `sqlite_schema` only; this function never executes DDL.
pub async fn verify_required_indexes(pool: &SqlitePool) -> Result<SchemaVerification, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT name, sql FROM sqlite_schema WHERE type = 'index' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await?;

    let definitions: std::collections::HashMap<String, String> = rows
        .into_iter()
        .filter_map(|row| {
            let name = row.try_get::<String, _>("name").ok()?;
            let sql = row.try_get::<Option<String>, _>("sql").ok()??;
            Some((name, sql))
        })
        .collect();

    let mut missing_indexes = Vec::new();
    let mut incompatible_indexes = Vec::new();
    for required in REQUIRED_INDEXES {
        match definitions.get(required.name) {
            None => missing_indexes.push(required.name.to_string()),
            Some(actual) if normalize_sql(actual) != normalize_sql(required.sql) => {
                incompatible_indexes.push(required.name.to_string());
            }
            Some(_) => {}
        }
    }

    if missing_indexes.is_empty() && incompatible_indexes.is_empty() {
        Ok(SchemaVerification::Ready)
    } else {
        Ok(SchemaVerification::MaintenanceRequired {
            missing_indexes,
            incompatible_indexes,
        })
    }
}

/// Creates missing required indexes and verifies the resulting schema.
pub async fn reconcile_required_indexes(
    pool: &SqlitePool,
    busy_timeout: Duration,
) -> Result<SchemaMaintenanceReport, SchemaMaintenanceError> {
    let command_started = Instant::now();
    let initial = verify_required_indexes(pool)
        .await
        .map_err(|source| SchemaMaintenanceError::Verification(Box::new(source)))?;

    let (missing_indexes, incompatible_indexes) = match initial {
        SchemaVerification::Ready => (Vec::new(), Vec::new()),
        SchemaVerification::MaintenanceRequired {
            missing_indexes,
            incompatible_indexes,
        } => (missing_indexes, incompatible_indexes),
    };

    if let Some(index_name) = incompatible_indexes.first() {
        let Some(required) = REQUIRED_INDEXES
            .iter()
            .find(|required| required.name == index_name)
        else {
            return Err(SchemaMaintenanceError::InvalidIndexDefinition {
                index: index_name.clone(),
                expected: "required manifest definition".to_string(),
                actual: "verification returned an unknown index".to_string(),
            });
        };
        let actual: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = ?")
                .bind(index_name)
                .fetch_optional(pool)
                .await
                .map_err(|source| SchemaMaintenanceError::Verification(Box::new(source)))?;
        return Err(SchemaMaintenanceError::InvalidIndexDefinition {
            index: index_name.clone(),
            expected: required.sql.to_string(),
            actual: actual.unwrap_or_else(|| "<missing>".to_string()),
        });
    }

    let mut created_indexes = Vec::new();
    let mut skipped_indexes = Vec::new();
    for required in REQUIRED_INDEXES {
        if !missing_indexes.iter().any(|name| name == required.name) {
            info!(
                index = required.name,
                lifecycle = "skip",
                outcome = "already_present",
                "Schema maintenance skipped existing index"
            );
            skipped_indexes.push(required.name.to_string());
            continue;
        }

        let started = Instant::now();
        info!(
            index = required.name,
            lifecycle = "start",
            "Schema maintenance started index creation"
        );
        if let Err(source) = sqlx::query(required.sql).execute(pool).await {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            error!(
                index = required.name,
                lifecycle = "failure",
                elapsed_ms,
                error = %source,
                "Schema maintenance failed index creation"
            );
            if is_lock_contention(&source) {
                return Err(SchemaMaintenanceError::LockTimeout {
                    index: required.name.to_string(),
                    timeout: busy_timeout,
                    source: Box::new(source),
                });
            }
            return Err(SchemaMaintenanceError::Ddl {
                index: required.name.to_string(),
                source: Box::new(source),
            });
        }

        info!(
            index = required.name,
            lifecycle = "completion",
            outcome = "created",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Schema maintenance completed index creation"
        );
        created_indexes.push(required.name.to_string());
    }

    match verify_required_indexes(pool)
        .await
        .map_err(|source| SchemaMaintenanceError::Verification(Box::new(source)))?
    {
        SchemaVerification::Ready => Ok(SchemaMaintenanceReport {
            created_indexes,
            skipped_indexes,
            elapsed: command_started.elapsed(),
        }),
        SchemaVerification::MaintenanceRequired {
            missing_indexes,
            incompatible_indexes,
        } => Err(SchemaMaintenanceError::InvalidIndexDefinition {
            index: missing_indexes
                .first()
                .or_else(|| incompatible_indexes.first())
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string()),
            expected: "required manifest definition".to_string(),
            actual: "post-maintenance verification did not match".to_string(),
        }),
    }
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .replace(" if not exists", "")
}

fn is_lock_contention(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database_error) = error else {
        return false;
    };
    matches!(database_error.code().as_deref(), Some("5" | "6"))
        || database_error.message().contains("database is locked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool_with_records_table() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE records (at_uri TEXT, did TEXT, time_us INTEGER, created_at TEXT, hydration_quality TEXT, source_event_id TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn verification_reports_every_missing_required_index() {
        let pool = pool_with_records_table().await;

        let result = verify_required_indexes(&pool).await.unwrap();

        assert_eq!(
            result,
            SchemaVerification::MaintenanceRequired {
                missing_indexes: REQUIRED_INDEXES
                    .iter()
                    .map(|index| index.name.to_string())
                    .collect(),
                incompatible_indexes: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn verification_reports_incompatible_required_index() {
        let pool = pool_with_records_table().await;
        sqlx::query("CREATE INDEX idx_records_at_uri ON records(did)")
            .execute(&pool)
            .await
            .unwrap();

        let result = verify_required_indexes(&pool).await.unwrap();

        assert!(matches!(
            result,
            SchemaVerification::MaintenanceRequired {
                incompatible_indexes,
                ..
            } if incompatible_indexes == vec!["idx_records_at_uri"]
        ));
    }

    #[tokio::test]
    async fn repeated_verification_is_stable() {
        let pool = pool_with_records_table().await;
        reconcile_required_indexes(&pool, Duration::from_secs(1))
            .await
            .unwrap();

        let first = verify_required_indexes(&pool).await.unwrap();
        let second = verify_required_indexes(&pool).await.unwrap();

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn repeated_maintenance_skips_every_existing_index() {
        let pool = pool_with_records_table().await;
        reconcile_required_indexes(&pool, Duration::from_secs(1))
            .await
            .unwrap();

        let report = reconcile_required_indexes(&pool, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(report.skipped_indexes.len(), REQUIRED_INDEXES.len());
    }

    #[tokio::test]
    async fn maintenance_rejects_incompatible_definition_without_replacing_it() {
        let pool = pool_with_records_table().await;
        sqlx::query("CREATE INDEX idx_records_at_uri ON records(did)")
            .execute(&pool)
            .await
            .unwrap();

        let error = reconcile_required_indexes(&pool, Duration::from_secs(1))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SchemaMaintenanceError::InvalidIndexDefinition { index, .. }
                if index == "idx_records_at_uri"
        ));
    }
}
