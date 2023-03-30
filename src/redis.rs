use async_trait::async_trait;
use rustis::bb8::{CustomizeConnection, ErrorSink, Pool};
use rustis::client::{Config, PooledClientManager, ServerConfig};
use tokio::time::Duration;

pub type RedisPool = Pool<PooledClientManager>;

pub async fn new(cfg: super::conf::Redis) -> Result<RedisPool, rustis::Error> {
    let config = Config {
        server: ServerConfig::Standalone {
            host: cfg.host,
            port: cfg.port,
        },
        username: Some(cfg.username).filter(|s| !s.is_empty()),
        password: Some(cfg.password).filter(|s| !s.is_empty()),
        connect_timeout: Duration::from_secs(3),
        command_timeout: Duration::from_millis(100),
        keep_alive: Some(Duration::from_secs(600)),
        ..Config::default()
    };

    let max_size = if cfg.max_connections > 0 {
        cfg.max_connections as u32
    } else {
        10
    };
    let min_idle = if max_size <= 10 { 1 } else { max_size / 10 };

    let manager = PooledClientManager::new(config).unwrap();
    RedisPool::builder()
        .max_size(max_size)
        .min_idle(Some(min_idle))
        .max_lifetime(None)
        .idle_timeout(Some(Duration::from_secs(600)))
        .connection_timeout(Duration::from_secs(3))
        .error_sink(Box::new(RedisMonitor {}))
        .connection_customizer(Box::new(RedisMonitor {}))
        .build(manager)
        .await
}

#[derive(Debug, Clone, Copy)]
struct RedisMonitor;

impl<E: std::fmt::Display> ErrorSink<E> for RedisMonitor {
    fn sink(&self, error: E) {
        log::error!(target: "redis", "{}", error);
    }

    fn boxed_clone(&self) -> Box<dyn ErrorSink<E>> {
        Box::new(*self)
    }
}

#[async_trait]
impl<C: Send + 'static, E: 'static> CustomizeConnection<C, E> for RedisMonitor {
    async fn on_acquire(&self, _connection: &mut C) -> Result<(), E> {
        log::info!(target: "redis", "connection acquired");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rustis::resp;

    use super::{super::conf, *};

    #[actix_web::test]
    async fn redis_pool_works() -> anyhow::Result<()> {
        let pool = new(conf::Redis {
            host: "127.0.0.1".to_string(),
            port: 6379,
            username: String::new(),
            password: String::new(),
            max_connections: 10,
        })
        .await?;

        let data = pool.get().await?.send(resp::cmd("PING"), None).await?;
        assert_eq!("PONG", data.to::<String>()?);

        Ok(())
    }
}
