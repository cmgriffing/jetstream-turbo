/// Utility functions for JSON serialization/deserialization
pub mod json_utils {
    /// Parse JSON with better error messages
    pub fn parse_json<T: serde::de::DeserializeOwned>(
        json_str: &str,
    ) -> Result<T, serde_json::Error> {
        serde_json::from_str(json_str)
    }

    /// Serialize to JSON with error handling
    pub fn to_json_string<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
        serde_json::to_string(value)
    }

    /// Serialize to JSON pretty format
    pub fn to_json_pretty<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(value)
    }
}

/// Utility functions for string handling
pub mod string_utils {
    /// Extract DID from AT URI (e.g., "at://did:plc:test/app.bsky.feed.post/abc" -> Some("did:plc:test"))
    pub fn extract_did_from_at_uri(at_uri: &str) -> Option<&str> {
        at_uri
            .strip_prefix("at://")
            .and_then(|s| s.split('/').next())
    }

    /// Check if string is a valid DID
    pub fn is_valid_did(did: &str) -> bool {
        did.starts_with("did:plc:") && did.len() > 10
    }

    /// Truncate string with ellipsis
    pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_string()
        } else {
            format!("{}...", &s[..max_len.saturating_sub(3)])
        }
    }
}

/// Utility functions for time handling
pub mod time_utils {
    use chrono::{DateTime, Utc};

    /// Convert timestamp to ISO 8601 string
    pub fn to_iso8601(dt: &DateTime<Utc>) -> String {
        dt.to_rfc3339()
    }

    /// Parse ISO 8601 string to timestamp
    pub fn from_iso8601(s: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc))
    }

    /// Get current timestamp as seconds
    pub fn current_timestamp() -> i64 {
        Utc::now().timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_json_utils() {
        let value = serde_json::json!({
            "test": "value",
            "number": 42
        });

        let json_str = json_utils::to_json_string(&value).unwrap();
        assert!(json_str.contains("test"));
        assert!(json_str.contains("42"));

        let parsed: serde_json::Value = json_utils::parse_json(&json_str).unwrap();
        assert_eq!(parsed["test"], "value");
        assert_eq!(parsed["number"], 42);
    }

    #[test]
    fn test_string_utils() {
        assert_eq!(
            string_utils::extract_did_from_at_uri("at://did:plc:test/app.bsky.feed.post/abc"),
            Some("did:plc:test")
        );

        assert!(string_utils::is_valid_did("did:plc:abcdef123456"));
        assert!(!string_utils::is_valid_did("invalid:did"));

        let truncated = string_utils::truncate_with_ellipsis("This is a very long string", 10);
        assert_eq!(truncated, "This is...");

        let short = string_utils::truncate_with_ellipsis("Short", 10);
        assert_eq!(short, "Short");
    }

    #[test]
    fn test_time_utils() {
        let now = Utc::now();
        let iso_string = time_utils::to_iso8601(&now);

        let parsed = time_utils::from_iso8601(&iso_string).unwrap();
        assert_eq!(parsed.timestamp(), now.timestamp());

        let timestamp = time_utils::current_timestamp();
        assert!(timestamp > 0);
    }
}
