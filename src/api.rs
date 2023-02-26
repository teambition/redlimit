use actix_web::{get, http::header, web, Error, HttpRequest, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{timeout, Duration};

use crate::{context::ContextExt, redis::RedisPool, redlimit, redlimit::RedRules};

#[derive(Serialize, Deserialize)]
pub struct AppInfo {
    pub version: String,
}

#[get("/version")]
pub async fn version(
    req: HttpRequest,
    info: web::Data<AppInfo>,
    pool: web::Data<RedisPool>,
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

pub async fn get_limiting(
    req: HttpRequest,
    pool: web::Data<RedisPool>,
    rules: web::Data<RedRules>,
    input: web::Query<LimitRequest>,
) -> Result<HttpResponse, Error> {
    limiting(req, pool, rules, input.into_inner()).await
}

#[derive(Deserialize)]
pub struct LimitRequest {
    scope: String,
    path: String,
    id: String,
}

pub async fn post_limiting(
    req: HttpRequest,
    pool: web::Data<RedisPool>,
    rules: web::Data<RedRules>,
    input: web::Json<LimitRequest>,
) -> Result<HttpResponse, Error> {
    limiting(req, pool, rules, input.into_inner()).await
}

static APPLICATION_JSON: header::HeaderValue = header::HeaderValue::from_static("application/json");
static APPLICATION_CBOR: header::HeaderValue = header::HeaderValue::from_static("application/cbor");

async fn limiting(
    req: HttpRequest,
    pool: web::Data<RedisPool>,
    rules: web::Data<RedRules>,
    input: LimitRequest,
) -> Result<HttpResponse, Error> {
    let ts = req.context()?.time.timestamp_millis();
    let args = rules
        .limit_args(ts as u64, &input.scope, &input.path, &input.id)
        .await;

    let rt = match timeout(
        Duration::from_millis(100),
        redlimit::limiting(pool, &input.scope, &input.id, args),
    )
    .await
    {
        Ok(rt) => rt,
        Err(_) => Err(anyhow::Error::msg("limiting timeout".to_string())),
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
    let mut ctx = req.context_mut()?;
    ctx.log
        .insert("scope".to_string(), Value::from(input.scope));
    ctx.log.insert("path".to_string(), Value::from(input.path));
    ctx.log.insert("id".to_string(), Value::from(input.id));
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
