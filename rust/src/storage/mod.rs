pub mod redis;
pub mod rotation;
pub mod schema;
pub mod sqlite;

pub use redis::{EventPublisher, RedisStore};
pub use rotation::DatabaseRotator;
pub use schema::{
    reconcile_required_indexes, verify_required_indexes, RequiredIndex, SchemaMaintenanceError,
    SchemaMaintenanceReport, SchemaVerification, REQUIRED_INDEXES,
};
pub use sqlite::{
    CleanupResult, RecordStore, SQLitePragmaConfig, SQLiteStateSnapshot, SQLiteStore,
    VacuumExecutionMode, VacuumGatingReason, VacuumPendingReason, VacuumRunPolicy,
    VacuumRunResult, VacuumState,
};
