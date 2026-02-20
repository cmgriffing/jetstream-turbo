use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub stream_a_url: String,
    #[serde(default = "default_stream_a_name")]
    pub stream_a_name: String,
    #[serde(default = "default_stream_b_name")]
    pub stream_b_url: String,

    pub stream_b_name: String,
    #[serde(default = "default_bind")]
    pub bind_address: String,
    #[serde(default = "default_database")]
    pub database_url: String,
}

fn default_stream_a_name() -> String {
    "Stream A".to_string()
}

fn default_stream_b_name() -> String {
    "Stream B".to_string()
}

fn default_bind() -> String {
    "0.0.0.0:3001".to_string()
}

fn default_database() -> String {
    "sqlite://monitor.db?mode=rwc".to_string()
}

impl Settings {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let settings = config::Config::builder()
            .set_default("bind_address", default_bind())?
            .set_default("database_url", default_database())?
            .set_default("stream_a_name", default_stream_a_name())?
            .set_default("stream_b_name", default_stream_b_name())?
            .add_source(config::Environment::default())
            .build()?;

        settings.try_deserialize().map_err(Into::into)
    }
}
