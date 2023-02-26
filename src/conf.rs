use std::collections::HashMap;

use config::{Config, ConfigError, File, FileFormat};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Log {
    pub level: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Server {
    pub port: u16,
    pub cert_file: String,
    pub key_file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Redis {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Job {
    pub interval: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Rule {
    pub limit: Vec<u64>,

    #[serde(default)]
    pub path: HashMap<String, u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Conf {
    pub env: String,
    pub log: Log,
    pub server: Server,
    pub redis: Redis,
    pub job: Job,
    pub rules: HashMap<String, Rule>,
}

impl Conf {
    pub fn new() -> Result<Self, ConfigError> {
        let file_name =
            std::env::var("CONFIG_FILE_PATH").unwrap_or_else(|_| "./config/default.toml".into());
        let builder = Config::builder()
        // .set_default("default", "1")?
        .add_source(File::new(file_name.as_str(), FileFormat::Toml));
        builder.build()?.try_deserialize::<Conf>()
    }
}
