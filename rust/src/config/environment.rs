// Environment variable utilities
use std::env;

pub fn get_required_env(key: &str) -> Result<String, anyhow::Error> {
    env::var(key).map_err(|_| anyhow::anyhow!("Required environment variable {key} is not set"))
}

pub fn get_optional_env(key: &str) -> Option<String> {
    env::var(key).ok()
}

pub fn get_env_with_default(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_optional_env() {
        // This should return None for non-existent variable
        let result = get_optional_env("NON_EXISTENT_VAR_12345");
        assert!(result.is_none());
    }

    #[test]
    fn test_get_env_with_default() {
        let result = get_env_with_default("NON_EXISTENT_VAR_67890", "default_value");
        assert_eq!(result, "default_value");
    }
}
