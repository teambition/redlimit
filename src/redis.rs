use rustis::client::{Config, PooledClientManager, ServerConfig};
use tokio::time::Duration;

pub type RedisPool = rustis::bb8::Pool<PooledClientManager>;

pub async fn new(cfg: super::conf::Redis) -> Result<RedisPool, rustis::Error> {
    let config = Config {
        server: ServerConfig::Standalone {
            host: cfg.host,
            port: cfg.port,
        },
        username: Some(cfg.username).filter(|s| !s.is_empty()),
        password: Some(cfg.password).filter(|s| !s.is_empty()),
        connect_timeout: Duration::from_secs(3),
        command_timeout: Duration::from_millis(300),
        keep_alive: Some(Duration::from_secs(65)),
        ..Config::default()
    };

    let manager = PooledClientManager::new(config).unwrap();
    RedisPool::builder()
        .max_size(1000)
        .min_idle(Some(5))
        .max_lifetime(None)
        .idle_timeout(Some(Duration::from_secs(120)))
        .connection_timeout(Duration::from_secs(3))
        .build(manager)
        .await
}

#[cfg(test)]
mod tests {
    use rustis::resp;

    use super::{super::conf, *};

    #[actix_rt::test]
    async fn redis_pool_works() -> anyhow::Result<()> {
        let pool = new(conf::Redis {
            host: "127.0.0.1".to_string(),
            port: 6379,
            username: String::new(),
            password: String::new(),
        })
        .await?;

        let data = pool.get().await?.send(resp::cmd("PING"), None).await?;
        assert_eq!("PONG", data.to::<String>()?);

        Ok(())
    }
}
