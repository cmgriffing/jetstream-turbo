pub mod auth;
pub mod bluesky;
pub(crate) mod coordination;
pub mod jetstream;
pub mod pool;
pub mod resilience;

pub use auth::BlueskyAuthClient;
pub use bluesky::{
    BlueskyClient, BlueskyCoordinationDiagnostics, BlueskyFetchDiagnostics,
    BlueskyFetchKindDiagnostics, PostFetchOutcome, PostFetcher, ProfileFetcher,
    DEFAULT_POST_COORDINATION_KEY_CAPACITY, DEFAULT_POST_COORDINATION_WAITER_CAPACITY,
    DEFAULT_PROFILE_COORDINATION_KEY_CAPACITY, DEFAULT_PROFILE_COORDINATION_WAITER_CAPACITY,
};
pub use coordination::CoordinationSnapshot;
pub use jetstream::{JetstreamClient, MessageSource};
pub use resilience::{
    sanitize_diagnostic_summary, BlueskyOperation, ContainmentPolicy, HydrationFailure,
    IsolationOutcome, RequestRetryPolicy, UpstreamFailureCategory, UpstreamHttpError,
};
