use std::{fs::File, io::BufReader};

use actix_web::{web, App, HttpServer};
use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys};
use tokio::time::Duration;

mod api;
mod conf;
mod context;
mod redis;
mod redlimit;
mod redlimit_lua;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = conf::Conf::new().unwrap_or_else(|err| panic!("config error: {}", err));

    std::env::set_var("LOG_LEVEL", cfg.log.level.as_str());
    std_logger::Config::gcloud().init();
    log::debug!("{:?}", cfg);

    let pool = web::Data::new(
        redis::new(cfg.redis)
            .await
            .unwrap_or_else(|err| panic!("redis connection pool error: {}", err)),
    );

    if let Err(err) = redlimit::init_redlimit_fn(pool.clone()).await {
        panic!("redis FUNCTION error: {}", err)
    }

    let redrules = web::Data::new(redlimit::RedRules::new(&cfg.namespace, &cfg.rules));

    // background jobs relating to local, disposable tasks
    let (redlimit_sync_handle, cancel_redlimit_sync) =
        redlimit::init_redlimit_sync(pool.clone(), redrules.clone(), cfg.job.interval);

    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(api::AppInfo {
                version: String::from("v0.1.0"),
            }))
            .app_data(pool.clone())
            .app_data(redrules.clone())
            .wrap(context::ContextTransform {})
            .service(web::resource("/limiting").route(web::post().to(api::post_limiting)))
            .service(
                web::resource("/redlist")
                    .route(web::get().to(api::get_redlist))
                    .route(web::post().to(api::post_redlist)),
            )
            .service(
                web::resource("/redrules")
                    .route(web::get().to(api::get_redrules))
                    .route(web::post().to(api::post_redrules)),
            )
            .route("/version", web::get().to(api::version))
    })
    .workers(2)
    .keep_alive(Duration::from_secs(25))
    .shutdown_timeout(10);

    log::info!("redlimit service start at 0.0.0.0:{}", cfg.server.port);
    let addr = ("0.0.0.0", cfg.server.port);
    if cfg.server.key_file.is_empty() || cfg.server.cert_file.is_empty() {
        server.bind(addr)?.run().await?;
    } else {
        let config = load_rustls_config(cfg.server);
        server.bind_rustls(addr, config)?.run().await?;
    }

    cancel_redlimit_sync.cancel();
    redlimit_sync_handle.await.unwrap();
    log::info!("redlimit service shutdown gracefully");

    Ok(())
}

fn load_rustls_config(cfg: conf::Server) -> rustls::ServerConfig {
    // init server config builder with safe defaults
    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth();

    // load TLS key/cert files
    let cert_file = &mut BufReader::new(File::open(cfg.cert_file.as_str()).unwrap());
    let key_file = &mut BufReader::new(File::open(cfg.key_file.as_str()).unwrap());

    // convert files to key/cert objects
    let cert_chain = certs(cert_file)
        .unwrap()
        .into_iter()
        .map(Certificate)
        .collect();
    let mut keys: Vec<PrivateKey> = pkcs8_private_keys(key_file)
        .unwrap()
        .into_iter()
        .map(PrivateKey)
        .collect();

    // exit if no keys could be parsed
    if keys.is_empty() {
        eprintln!("Could not locate PKCS 8 private keys.");
        std::process::exit(1);
    }

    config.with_single_cert(cert_chain, keys.remove(0)).unwrap()
}
