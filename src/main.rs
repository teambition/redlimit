use actix_web::{
    get, http::header, web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder,
};
use rustis::client::{Config, PooledClientManager};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{timeout, Duration};

mod context;
mod redlimit;

use crate::context::ContextExt;

#[derive(Serialize, Deserialize)]
struct AppInfo {
    version: String,
}

#[get("/version")]
async fn version(
    req: HttpRequest,
    info: web::Data<AppInfo>,
    pool: web::Data<redlimit::RedisPool>,
) -> impl Responder {
    let state = pool.state();
    let mut ctx = req.context_mut().unwrap();
    ctx.log
        .insert("connections".to_string(), Value::from(state.connections));
    ctx.log.insert(
        "idle_connections".to_string(),
        Value::from(state.idle_connections),
    );
    web::Json(info)
}

#[derive(Deserialize)]
struct LimitQuery {
    id: String,
    args: String,
}

async fn get_limiting(
    req: HttpRequest,
    pool: web::Data<redlimit::RedisPool>,
    info: web::Query<LimitQuery>,
) -> Result<HttpResponse, Error> {
    let res = serde_json::from_str::<Vec<u64>>(info.args.as_str());
    if res.is_err() {
        return Ok(HttpResponse::BadRequest().body(res.err().unwrap().to_string()));
    }
    limiting(req, pool, info.id.to_string(), res.unwrap()).await
}

#[derive(Deserialize)]
struct LimitRequest {
    id: String,
    args: Vec<u64>,
}

async fn post_limiting(
    req: HttpRequest,
    pool: web::Data<redlimit::RedisPool>,
    input: web::Json<LimitRequest>,
) -> Result<HttpResponse, Error> {
    let input = input.into_inner();
    limiting(req, pool, input.id, input.args).await
}

static APPLICATION_JSON: header::HeaderValue = header::HeaderValue::from_static("application/json");
static APPLICATION_CBOR: header::HeaderValue = header::HeaderValue::from_static("application/cbor");

async fn limiting(
    req: HttpRequest,
    pool: web::Data<redlimit::RedisPool>,
    id: String,
    args: Vec<u64>,
) -> Result<HttpResponse, Error> {
    let rt = match timeout(
        Duration::from_millis(100),
        redlimit::limiting(pool, id.clone(), args),
    )
    .await
    {
        Ok(rt) => rt,
        Err(_) => Err("limiting timeout".to_string()),
    };

    let rt = match rt {
        Ok(rt) => rt,
        Err(err) => {
            log::error!("redis error: {}", err);
            redlimit::LimitResult(0, 0)
        }
    };

    let accept = req.headers().get(header::ACCEPT);
    let accept_cbor = accept.is_some() && accept.unwrap() == APPLICATION_CBOR;
    let mut ctx = req.context_mut().unwrap();
    ctx.log.insert("id".to_string(), Value::from(id.clone()));
    ctx.log.insert("count".to_string(), Value::from(rt.0));
    ctx.log.insert("limited".to_string(), Value::from(rt.1 > 0));
    ctx.log.insert("cbor".to_string(), Value::from(accept_cbor));

    match serde_json::to_string(&rt) {
        Ok(body) => Ok(HttpResponse::Ok()
            .content_type(&APPLICATION_JSON)
            .body(body)),
        Err(err) => Ok(HttpResponse::BadRequest().body(err.to_string())),
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "info");

    // set up redis connection pool
    let manager = PooledClientManager::new(Config::default()).unwrap();
    let pool = redlimit::RedisPool::builder()
        .max_size(100)
        .min_idle(Some(2))
        .max_lifetime(None)
        .idle_timeout(Some(Duration::new(600, 0)))
        .connection_timeout(Duration::new(1, 0))
        .build(manager)
        .await
        .unwrap();

    if let Err(err) = redlimit::load_fn(pool.clone()).await {
        log::error!("redis FUNCTION Load error: {}", err);
        panic!("redis FUNCTION Load error: {}", err)
    }

    json_env_logger2::init();
    json_env_logger2::panic_hook();
    log::info!("starting HTTP server at http://localhost:8080");

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(AppInfo {
                version: String::from("v0.1.0"),
            }))
            .app_data(web::Data::new(pool.clone()))
            .wrap(context::ContextTransform {})
            .service(
                web::resource("/limiting")
                    .route(web::get().to(get_limiting))
                    .route(web::post().to(post_limiting)),
            )
            .service(version)
    })
    .workers(3)
    .keep_alive(Duration::from_secs(75))
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
