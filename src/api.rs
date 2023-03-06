use std::collections::HashMap;

use actix_web::{http::StatusCode, web, Error, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, to_value, Value};
use tokio::time::{timeout, Duration};

use crate::{context::ContextExt, redis::RedisPool, redlimit, redlimit::RedRules};

#[derive(Serialize, Deserialize)]
pub struct AppInfo {
    pub version: String,
}

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

#[derive(Serialize)]
pub struct LimitResponse {
    limit: u64,     // x-ratelimit-limit
    remaining: u64, // x-ratelimit-remaining
    reset: u64,     // x-ratelimit-reset
    retry: u64,     // retry-after delay-milliseconds
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
    let limit = args.1;

    let rt = match timeout(
        Duration::from_millis(100),
        redlimit::limiting(pool, &rules.ns.limiting_key(&input.scope, &input.id), args),
    )
    .await
    {
        Ok(rt) => rt,
        Err(_) => Err(anyhow::Error::msg("limiting timeout".to_string())),
    };

    let rt = match rt {
        Ok(rt) => rt,
        Err(err) => {
            log::warn!("post_limiting error: {}", err);
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

    respond_result(LimitResponse {
        limit,
        remaining: if limit > rt.0 { limit - rt.0 } else { 0 },
        reset: if rt.1 > 0 { (ts + rt.1) / 1000 } else { 0 },
        retry: rt.1,
    })
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
    rules: web::Data<RedRules>,
    input: web::Json<HashMap<String, u64>>,
) -> Result<HttpResponse, Error> {
    if let Err(err) = redlimit::redlist_add(pool, rules.ns.as_str(), &input.into_inner()).await {
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
    rules: web::Data<RedRules>,
    input: web::Json<RedRulesRequest>,
) -> Result<HttpResponse, Error> {
    let input = input.into_inner();
    if let Err(err) =
        redlimit::redrules_add(pool, rules.ns.as_str(), &input.scope, &input.rules).await
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::header::ContentType, test, App};

    #[actix_web::test]
    async fn get_version_works() -> anyhow::Result<()> {
        let cfg = super::super::conf::Conf::new()?;
        let pool = web::Data::new(super::super::redis::new(cfg.redis.clone()).await?);
        let info = web::Data::new(AppInfo {
            version: String::from("v0.1.0"),
        });

        let app = test::init_service(
            App::new()
                .app_data(pool.clone())
                .app_data(info.clone())
                .wrap(super::super::context::ContextTransform {})
                .route("/", web::get().to(version)),
        )
        .await;
        let req = test::TestRequest::default()
            .insert_header(ContentType::json())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        Ok(())
    }
}
