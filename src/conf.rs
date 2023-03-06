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
    pub namespace: String,
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
        Self::from(&file_name)
    }

    pub fn from(file_name: &str) -> Result<Self, ConfigError> {
        let builder = Config::builder().add_source(File::new(file_name, FileFormat::Toml));
        builder.build()?.try_deserialize::<Conf>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_web::test]
    async fn config_works() -> anyhow::Result<()> {
        let cfg = Conf::new()?;
        assert_eq!("development", cfg.env);
        assert_eq!("debug", cfg.log.level);
        assert_eq!(8080, cfg.server.port);
        assert_eq!("127.0.0.1", cfg.redis.host);
        assert_eq!(6379, cfg.redis.port);
        assert_eq!(3, cfg.job.interval);

        let default_rules = cfg
            .rules
            .get("*")
            .ok_or(anyhow::Error::msg("'*' not exists"))?;
        assert_eq!(vec![10, 10000, 3, 1000], default_rules.limit);
        assert!(default_rules.path.is_empty());

        let floor_rules = cfg
            .rules
            .get("-")
            .ok_or(anyhow::Error::msg("'-' not exists"))?;
        assert_eq!(vec![3, 10000, 1, 1000], floor_rules.limit);
        assert!(floor_rules.path.is_empty());

        let core_rules = cfg
            .rules
            .get("core")
            .ok_or(anyhow::Error::msg("'core' not exists"))?;
        assert_eq!(vec![200, 10000, 10, 2000], core_rules.limit);
        assert_eq!(
            2,
            core_rules
                .path
                .get("POST /v1/file/list")
                .unwrap()
                .to_owned()
        );

        Ok(())
    }

    #[actix_web::test]
    async fn config_from_env_works() -> anyhow::Result<()> {
        let cfg = Conf::from("./config/test.toml")?;
        assert_eq!("test", cfg.env);
        assert_eq!("info", cfg.log.level);

        Ok(())
    }
}
