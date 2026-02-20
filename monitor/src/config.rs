use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub stream_a_url: String,
    pub stream_b_url: String,
    #[serde(default = "default_bind")]
    pub bind_address: String,
    #[serde(default = "default_database")]
    pub database_url: String,
}

fn default_bind() -> String {
    "0.0.0.0:3000".to_string()
}

fn default_database() -> String {
    "sqlite:monitor.db".to_string()
}

impl Settings {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let settings = config::Config::builder()
            .set_default("bind_address", default_bind())?
            .set_default("database_url", default_database())?
            .add_source(config::Environment::default())
            .build()?;

        settings.try_deserialize().map_err(Into::into)
    }
}
