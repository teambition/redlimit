use std::{
    cell::{Ref, RefMut},
    collections::HashMap,
    time::Instant,
};

use actix_utils::future::{ready, Ready};
use actix_web::{
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    error::ErrorInternalServerError,
    Error, HttpMessage, HttpRequest,
};
use futures_core::future::LocalBoxFuture;
use serde_json::Value;

pub use structured_logger::unix_ms;

pub struct ContextTransform;

pub struct Context {
    pub unix_ms: u64,
    pub start: Instant,
    pub log: HashMap<String, Value>,
}

impl Context {
    pub fn new() -> Self {
        Context {
            unix_ms: unix_ms(),
            start: Instant::now(),
            log: HashMap::new(),
        }
    }
}

pub trait ContextExt {
    fn context(&self) -> Result<Ref<'_, Context>, Error>;
    fn context_mut(&self) -> Result<RefMut<'_, Context>, Error>;
}

impl ContextExt for HttpRequest {
    fn context(&self) -> Result<Ref<'_, Context>, Error> {
        if self.extensions().get::<Context>().is_none() {
            return Err(ErrorInternalServerError(
                "no context in http request extensions",
            ));
        }

        Ok(Ref::map(self.extensions(), |ext| ext.get().unwrap()))
    }

    fn context_mut(&self) -> Result<RefMut<'_, Context>, Error> {
        if self.extensions().get::<Context>().is_none() {
            return Err(ErrorInternalServerError(
                "no context in http request extensions",
            ));
        }

        Ok(RefMut::map(self.extensions_mut(), |ext| {
            ext.get_mut().unwrap()
        }))
    }
}

impl<S, B> Transform<S, ServiceRequest> for ContextTransform
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = ContextMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ContextMiddleware { service }))
    }
}

pub struct ContextMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for ContextMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let log_method = req.method().to_string();
        let log_path = req.path().to_string();
        let log_xid = req
            .headers()
            .get("x-request-id")
            .map_or("", |h| h.to_str().unwrap())
            .to_string();

        let ctx = Context::new();
        req.request().extensions_mut().insert(ctx);
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            {
                let ctx = res.request().context_mut().unwrap();
                log::info!(target: "api",
                    method = log_method,
                    path = log_path,
                    xid = log_xid,
                    status = res.response().status().as_u16(),
                    start = ctx.unix_ms,
                    elapsed = ctx.start.elapsed().as_millis() as u64,
                    kv = log::as_serde!(&ctx.log);
                    "",
                );
            }
            Ok(res)
        })
    }
}
