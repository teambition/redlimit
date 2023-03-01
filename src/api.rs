use std::collections::HashMap;

use actix_web::{get, http::StatusCode, web, Error, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, to_value, Value};
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
) -> Result<HttpResponse, Error> {
    let state = pool.state();
    let mut ctx = req.context_mut().unwrap();
    ctx.log
        .insert("connections".to_string(), Value::from(state.connections));
    ctx.log.insert(
        "idle_connections".to_string(),
        Value::from(state.idle_connections),
    );
    respond_result(info)
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
    let input = input.into_inner();
    let ts = req.context()?.unix_ms;
    let args = rules
        .limit_args(ts, &input.scope, &input.path, &input.id)
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
            log::error!("post_limiting error: {}", err);
            redlimit::LimitResult(0, 0)
        }
    };

    let mut ctx = req.context_mut()?;
    ctx.log
        .insert("scope".to_string(), Value::from(input.scope));
    ctx.log.insert("path".to_string(), Value::from(input.path));
    ctx.log.insert("id".to_string(), Value::from(input.id));
    ctx.log.insert("count".to_string(), Value::from(rt.0));
    ctx.log.insert("limited".to_string(), Value::from(rt.1 > 0));

    respond_result(rt)
}

pub async fn get_redlist(
    req: HttpRequest,
    rules: web::Data<RedRules>,
) -> Result<HttpResponse, Error> {
    let ts = req.context()?.unix_ms;
    let rt = rules.redlist(ts).await;
    respond_result(rt)
}

pub async fn post_redlist(
    pool: web::Data<RedisPool>,
    input: web::Json<HashMap<String, u64>>,
) -> Result<HttpResponse, Error> {
    if let Err(err) = redlimit::redlist_add(pool, input.into_inner()).await {
        log::error!("redlist_add error: {}", err);
        return respond_error(500, err.to_string());
    }

    respond_result("ok")
}

pub async fn get_redrules(
    req: HttpRequest,
    rules: web::Data<RedRules>,
) -> Result<HttpResponse, Error> {
    let ts = req.context()?.unix_ms;
    let rt = rules.redrules(ts).await;
    respond_result(rt)
}

#[derive(Deserialize)]
pub struct RedRulesRequest {
    scope: String,
    rules: HashMap<String, (u64, u64)>,
}

pub async fn post_redrules(
    pool: web::Data<RedisPool>,
    input: web::Json<RedRulesRequest>,
) -> Result<HttpResponse, Error> {
    let input = input.into_inner();
    if let Err(err) = redlimit::redrules_add(pool, &input.scope, &input.rules).await {
        log::error!("redlist_add error: {}", err);
        return respond_error(500, err.to_string());
    }

    respond_result("ok")
}

fn respond_result(result: impl serde::ser::Serialize) -> Result<HttpResponse, Error> {
    match to_value(result) {
        Ok(result) => Ok(HttpResponse::Ok()
            .content_type("application/json")
            .json(json!({ "result": result }))),
        Err(err) => respond_error(500, err.to_string()),
    }
}

fn respond_error(code: u16, err_msg: String) -> Result<HttpResponse, Error> {
    let err_json = json!({ "error": {"code": code, "message": err_msg }});
    Ok(HttpResponse::build(StatusCode::from_u16(code).unwrap())
        .content_type("application/json")
        .json(err_json))
}
