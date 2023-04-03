use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use actix_web::web;
use anyhow::{Error, Result};
use rustis::{client::Client, resp};
use serde::{Deserialize, Serialize};
use tokio::{sync::RwLock, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;

use super::{conf::Rule, context::unix_ms, redis::RedisPool, redlimit_lua};

pub struct RedRules {
    pub ns: NS,
    floor: Vec<u64>,
    defaut: Rule,
    rules: HashMap<String, Rule>,
    dyn_rules: RwLock<DynRedRules>,
}

pub struct NS(String);

impl NS {
    pub fn new(namespace: String) -> Self {
        NS(namespace)
    }

    pub fn redlist_key(id: &str) -> &str {
        id
    }

    pub fn redrules_key(scope: &str, path: &str) -> String {
        format!("{}:{}", scope, path)
    }

    pub fn limiting_key(&self, scope: &str, id: &str) -> String {
        format!("{}:{}:{}", self.0, scope, id)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

pub struct DynRedRules {
    redrules: HashMap<String, (u64, u64)>, // ns:scope:path -> (quantity, ttl)
    redlist: HashMap<String, u64>,         // ns:id -> ttl
    redlist_cursor: u64,
}

impl RedRules {
    pub fn new(namespace: &str, rules: &HashMap<String, Rule>) -> Self {
        let mut rr = RedRules {
            ns: NS::new(namespace.to_string()),
            floor: vec![2, 10000, 1, 1000],
            defaut: Rule {
                limit: vec![5, 5000, 2, 1000],
                quantity: 1,
                path: HashMap::new(),
            },
            rules: HashMap::new(),
            dyn_rules: RwLock::new(DynRedRules {
                redrules: HashMap::new(),
                redlist: HashMap::new(),
                redlist_cursor: 0,
            }),
        };

        for (scope, rule) in rules {
            match scope.as_str() {
                "*" => rr.defaut = rule.clone(),
                "-" => rr.floor = rule.limit.clone(),
                _ => {
                    rr.rules.insert(scope.clone(), rule.clone());
                }
            }
        }
        rr
    }

    pub async fn redlist(&self, now: u64) -> HashMap<String, u64> {
        let dr = self.dyn_rules.read().await;
        let mut redlist = HashMap::new();
        for (k, v) in &dr.redlist {
            if *v >= now {
                redlist.insert(k.clone(), *v);
            }
        }
        redlist
    }

    pub async fn redrules(&self, now: u64) -> HashMap<String, (u64, u64)> {
        let dr = self.dyn_rules.read().await;
        let mut redrules = HashMap::new();
        for (k, v) in &dr.redrules {
            if v.1 >= now {
                redrules.insert(k.clone(), *v);
            }
        }
        redrules
    }

    pub async fn limit_args(&self, now: u64, scope: &str, path: &str, id: &str) -> LimitArgs {
        if id.is_empty() {
            return LimitArgs::new(0, &vec![]);
        }

        let dr = self.dyn_rules.read().await;
        if let Some(ttl) = dr.redlist.get(NS::redlist_key(id)) {
            if *ttl >= now {
                return LimitArgs::new(1, &self.floor);
            }
        }

        let rule = self.rules.get(scope).unwrap_or(&self.defaut);
        if let Some((quantity, ttl)) = dr.redrules.get(&NS::redrules_key(scope, path)) {
            if *ttl >= now {
                return LimitArgs::new(*quantity, &rule.limit);
            }
        }

        let quantity = *rule.path.get(path).unwrap_or(&rule.quantity);
        let quantity = if quantity > 0 { quantity } else { 1 };
        LimitArgs::new(quantity, &rule.limit)
    }

    pub async fn dyn_update(
        &self,
        now: u64,
        redlist_cursor: u64,
        redlist: HashMap<String, u64>,
        redrules: HashMap<String, (u64, u64)>,
    ) {
        let mut dr = self.dyn_rules.write().await;
        if redlist_cursor > dr.redlist_cursor {
            dr.redlist_cursor = redlist_cursor;
        }

        dr.redlist.retain(|_, v| *v > now);
        for (k, v) in redlist {
            if v > now {
                dr.redlist.insert(k, v);
            }
        }

        dr.redrules.retain(|_, v| v.1 > now);
        for (k, v) in redrules {
            if v.1 > now {
                dr.redrules.insert(k, v);
            }
        }
    }
}

// (quantity, max count per period, period with millisecond, max burst, burst
// period with millisecond)
#[derive(PartialEq, Debug)]
pub struct LimitArgs(pub u64, pub u64, pub u64, pub u64, pub u64);

impl LimitArgs {
    pub fn new(quantity: u64, others: &Vec<u64>) -> Self {
        let mut args = LimitArgs(quantity, 0, 0, 0, 0);
        match others.len() {
            2 => {
                args.1 = others[0];
                args.2 = others[1];
            }
            3 => {
                args.1 = others[0];
                args.2 = others[1];
                args.3 = others[2];
            }
            4 => {
                args.1 = others[0];
                args.2 = others[1];
                args.3 = others[2];
                args.4 = others[3];
            }
            _ => {}
        }
        args
    }

    pub fn is_valid(&self) -> bool {
        self.0 > 0
            && self.0 <= self.1
            && self.2 > 0
            && self.2 <= 60 * 1000
            && (self.3 == 0 || self.0 <= self.3)
            && (self.4 == 0 || self.4 <= self.2)
    }
}

#[derive(Serialize, PartialEq, Debug)]
// LimitResult.0: request count;
// LimitResult.1: 0: not limited, > 0: limited, milliseconds to wait;
pub struct LimitResult(pub u64, pub u64);

pub async fn limiting(
    pool: web::Data<RedisPool>,
    limiting_key: &str,
    args: LimitArgs,
) -> Result<LimitResult> {
    if !args.is_valid() {
        return Ok(LimitResult(0, 0));
    }

    let mut cmd = resp::cmd("FCALL")
        .arg("limiting")
        .arg(1)
        .arg(limiting_key)
        .arg(args.0)
        .arg(args.1)
        .arg(args.2);
    if args.3 > 0 {
        cmd = cmd.arg(args.3);
    }
    if args.4 > 0 {
        cmd = cmd.arg(args.4);
    }

    let data = pool.get().await?.send(cmd, None).await?;
    if let Ok(rt) = data.to::<(u64, u64)>() {
        return Ok(LimitResult(rt.0, rt.1));
    }

    Ok(LimitResult(0, 0))
}

pub async fn redrules_add(
    pool: web::Data<RedisPool>,
    ns: &str,
    scope: &str,
    rules: &HashMap<String, (u64, u64)>,
) -> Result<()> {
    if !rules.is_empty() {
        let cli = pool.get().await?;
        for (k, v) in rules {
            let cmd = resp::cmd("FCALL")
                .arg("redrules_add")
                .arg(1)
                .arg(ns)
                .arg(scope)
                .arg(k)
                .arg(v.0)
                .arg(v.1);
            cli.send(cmd, None).await?;
        }
    }
    Ok(())
}

pub async fn redlist_add(
    pool: web::Data<RedisPool>,
    ns: &str,
    list: &HashMap<String, u64>,
) -> Result<()> {
    if !list.is_empty() {
        let cli = pool.get().await?;
        let mut cmd = resp::cmd("FCALL").arg("redlist_add").arg(1).arg(ns);

        for (k, v) in list {
            cmd = cmd.arg(k).arg(*v);
        }

        cli.send(cmd, None).await?;
    }
    Ok(())
}

pub async fn init_redlimit_fn(pool: web::Data<RedisPool>) -> anyhow::Result<()> {
    let cmd = resp::cmd("FUNCTION")
        .arg("LOAD")
        .arg(redlimit_lua::REDLIMIT);

    let data = pool.get().await?.send(cmd, None).await?;
    if data.is_error() {
        let err = data.to_string();
        if !err.contains("already exists") {
            return Err(Error::msg(err));
        }
    }
    Ok(())
}

pub fn init_redlimit_sync(
    pool: web::Data<RedisPool>,
    redrules: web::Data<RedRules>,
    interval_secs: u64,
) -> (JoinHandle<()>, CancellationToken) {
    let cancel_redrules_sync = CancellationToken::new();
    (
        tokio::spawn(spawn_redlimit_sync(
            pool,
            redrules,
            cancel_redrules_sync.clone(),
            interval_secs,
        )),
        cancel_redrules_sync,
    )
}

async fn spawn_redlimit_sync(
    pool: web::Data<RedisPool>,
    redrules: web::Data<RedRules>,
    stop_signal: CancellationToken,
    interval_secs: u64,
) {
    loop {
        tokio::select! {
            _ = stop_signal.cancelled() => {
                log::info!("gracefully shutting down redlimit sync job");
                break;
            }
            _ = sleep(Duration::from_secs(interval_secs)) => {}
        };

        let rt = redlimit_sync_job(pool.clone(), redrules.clone()).await;
        if rt.is_err() {
            log::error!("redlimit_sync_job error: {:?}", rt);

            // auto load function
            if rt.unwrap_err().to_string().contains("Function not found") {
                match init_redlimit_fn(pool.clone()).await {
                    Ok(_) => {
                        log::warn!("init_redlimit_fn success");
                    }
                    Err(e) => {
                        log::error!("init_redlimit_fn error: {:?}", e);
                    }
                }
            }
        }
    }
}

async fn redlimit_sync_job(
    pool: web::Data<RedisPool>,
    redrules: web::Data<RedRules>,
) -> anyhow::Result<()> {
    let redis = pool.get().await?;
    let cursor = redrules.dyn_rules.read().await.redlist_cursor;
    let inow = Instant::now();
    let now = unix_ms();

    let dyn_rules = redrules_load(redis.clone(), redrules.ns.as_str(), now).await?;

    let dyn_list = redlist_load(redis.clone(), redrules.ns.as_str(), now, cursor).await?;

    let cursor = dyn_list.0;
    let rules_len = dyn_rules.len();
    let list_len = dyn_list.1.len();
    if !dyn_rules.is_empty() || !dyn_list.1.is_empty() {
        redrules
            .dyn_update(now, cursor, dyn_list.1, dyn_rules)
            .await;
    }

    log::info!(target: "sync",
        cursor = cursor,
        redrules = rules_len,
        redlist = list_len,
        elapsed = inow.elapsed().as_millis() as u64;
        "ok",
    );

    Ok(())
}

#[derive(Deserialize)]
struct RedRuleEntry(String, String, u64, u64);

async fn redrules_load(
    redis: Client,
    ns: &str,
    now: u64,
) -> anyhow::Result<HashMap<String, (u64, u64)>> {
    let redrules_cmd = resp::cmd("FCALL").arg("redrules_all").arg(1).arg(ns);

    let data = redis.send(redrules_cmd, None).await?.to::<Vec<String>>()?;
    let mut rt: HashMap<String, (u64, u64)> = HashMap::new();
    let mut has_stale = false;
    for s in data {
        if let Ok(v) = serde_json::from_str::<RedRuleEntry>(&s) {
            if v.3 > now {
                rt.insert(NS::redrules_key(&v.0, &v.1), (v.2, v.3));
            } else {
                has_stale = true
            }
        }
    }

    if has_stale {
        let sweep_cmd = resp::cmd("FCALL").arg("redrules_add").arg(1).arg(ns);
        redis.send(sweep_cmd, None).await?;
    }

    Ok(rt)
}

const REDLIST_SCAN_COUNT: usize = 10000;
async fn redlist_load(
    redis: Client,
    ns: &str,
    now: u64,
    cursor: u64,
) -> anyhow::Result<(u64, HashMap<String, u64>)> {
    let mut cursor = cursor;
    let mut has_stale = false;
    let mut rt: HashMap<String, u64> = HashMap::new();

    'next_cursor: loop {
        let blacklist_cmd = resp::cmd("FCALL")
            .arg("redlist_scan")
            .arg(1)
            .arg(ns)
            .arg(cursor);

        let data = redis.send(blacklist_cmd, None).await?.to::<Vec<String>>()?;
        let has_next = data.len() >= REDLIST_SCAN_COUNT;

        let mut iter = data.into_iter();
        match iter.next() {
            Some(c) => {
                let new_cursor = c.parse::<u64>()?;
                if cursor == new_cursor {
                    cursor += 1;
                } else {
                    cursor = new_cursor;
                }
            }
            None => {
                break;
            }
        }

        loop {
            if let Some(id) = iter.next() {
                match iter.next() {
                    Some(ttl) => {
                        let ttl = ttl.parse::<u64>()?;
                        if ttl > now {
                            rt.insert(id, ttl);
                        } else {
                            has_stale = true;
                        }
                        continue;
                    }
                    None => {
                        break 'next_cursor;
                    }
                }
            }
            break;
        }

        if !has_next {
            break;
        }
    }

    if has_stale {
        let sweep_cmd = resp::cmd("FCALL").arg("redlist_add").arg(1).arg(ns);
        redis.send(sweep_cmd, None).await?;
    }

    Ok((cursor, rt))
}

#[cfg(test)]
mod tests {

    use actix_web::web;

    use super::{
        super::{conf, redis},
        *,
    };

    #[actix_web::test]
    async fn limit_args_works() -> anyhow::Result<()> {
        assert_eq!(LimitArgs(1, 0, 0, 0, 0), LimitArgs::new(1, &vec![]));
        assert_eq!(LimitArgs(2, 0, 0, 0, 0), LimitArgs::new(2, &vec![]));
        assert_eq!(LimitArgs(2, 0, 0, 0, 0), LimitArgs::new(2, &vec![100]));

        assert_eq!(
            LimitArgs(3, 100, 10000, 0, 0),
            LimitArgs::new(3, &vec![100, 10000])
        );

        assert_eq!(
            LimitArgs(3, 100, 10000, 10, 0),
            LimitArgs::new(3, &vec![100, 10000, 10])
        );

        assert_eq!(
            LimitArgs(1, 100, 10000, 50, 2000),
            LimitArgs::new(1, &vec![100, 10000, 50, 2000])
        );

        assert_eq!(
            LimitArgs(1, 0, 0, 0, 0),
            LimitArgs::new(1, &vec![100, 10000, 50, 2000, 1])
        );

        Ok(())
    }

    #[actix_web::test]
    async fn red_rules_works() -> anyhow::Result<()> {
        let cfg = conf::Conf::new()?;
        let redrules = RedRules::new(&cfg.namespace, &cfg.rules);

        {
            assert_eq!(vec![3, 10000, 1, 1000], redrules.floor);

            assert_eq!(vec![10, 10000, 3, 1000], redrules.defaut.limit);
            assert!(redrules.defaut.path.is_empty());

            assert_eq!(0, redrules.dyn_rules.read().await.redlist_cursor);

            let core_rules = redrules
                .rules
                .get("core")
                .ok_or(anyhow::Error::msg("'core' not exists"))?;
            assert_eq!(vec![100, 10000, 50, 2000], core_rules.limit);
            assert_eq!(
                5,
                core_rules.path.get("GET /v1/file/list").unwrap().to_owned()
            );

            assert!(redrules.rules.get("core2").is_none());
        }

        {
            assert!(redrules.redlist(0).await.is_empty());
            assert!(redrules.redrules(0).await.is_empty());

            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user1")
                    .await
            );
            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user2")
                    .await,
                "any user"
            );

            assert_eq!(
                LimitArgs(1, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v2/file/list", "user1")
                    .await,
                "path not exists"
            );

            assert_eq!(
                LimitArgs(1, 10, 10000, 3, 1000),
                redrules
                    .limit_args(0, "core2", "GET /v1/file/list", "user1")
                    .await,
                "scope not exists"
            );

            assert_eq!(
                LimitArgs(1, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "biz", "GET /v1/app/info", "user1")
                    .await
            );
            assert_eq!(
                LimitArgs(3, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "biz", "GET /v2/app/info", "user1")
                    .await
            );
            assert_eq!(
                LimitArgs(10, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "biz", "GET /v3/app/info", "user1")
                    .await,
                "any user"
            );
        }

        let ts = unix_ms();
        {
            let mut dyn_blacklist = HashMap::new();
            dyn_blacklist.insert("user1".to_owned(), ts + 1000);
            redrules
                .dyn_update(ts, 1, dyn_blacklist, HashMap::new())
                .await;

            {
                let dr = redrules.dyn_rules.read().await;
                assert_eq!(1, dr.redlist_cursor);
            }

            assert_eq!(1, redrules.redlist(0).await.len());
            assert_eq!(1, redrules.redlist(ts + 1000).await.len());
            assert!(redrules.redlist(ts + 1001).await.is_empty());
            assert!(redrules.redrules(0).await.is_empty());

            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(ts, "core", "GET /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "GET /v1/file/list", "user1")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
        }

        {
            let mut dyn_rules = HashMap::new();
            dyn_rules.insert("core:GET /v1/file/list".to_owned(), (3, ts + 1000));
            dyn_rules.insert("core:GET /v2/file/list".to_owned(), (5, ts + 1000));
            redrules.dyn_update(ts, 2, HashMap::new(), dyn_rules).await;

            {
                let dr = redrules.dyn_rules.read().await;
                assert_eq!(2, dr.redlist_cursor);
            }

            assert_eq!(1, redrules.redlist(0).await.len());
            assert_eq!(2, redrules.redrules(0).await.len());
            assert_eq!(2, redrules.redrules(ts + 1000).await.len());
            assert!(redrules.redrules(ts + 1001).await.is_empty());

            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(3, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user2")
                    .await,
                "limited by dyn_rules"
            );
            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(0, "core", "GET /v2/file/list", "user2")
                    .await,
                "limited by dyn_rules"
            );

            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "GET /v1/file/list", "user1")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
            assert_eq!(
                LimitArgs(5, 100, 10000, 50, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "GET /v1/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
            assert_eq!(
                LimitArgs(1, 100, 10000, 50, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "GET /v2/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
        }

        {
            redrules
                .dyn_update(ts + 1001, ts, HashMap::new(), HashMap::new())
                .await;

            {
                let dr = redrules.dyn_rules.read().await;
                assert_eq!(ts, dr.redlist_cursor);
            }

            assert!(
                redrules.redlist(0).await.is_empty(),
                "auto sweep stale rules"
            );
            assert!(
                redrules.redrules(0).await.is_empty(),
                "auto sweep stale rules"
            );

            let mut dyn_rules = HashMap::new();
            dyn_rules.insert("core:GET /v1/file/list".to_owned(), (3, ts + 1000)); // stale rules
            dyn_rules.insert("core:GET /v1/file/list".to_owned(), (5, ts + 1002));

            redrules
                .dyn_update(ts + 1001, ts + 1, HashMap::new(), dyn_rules)
                .await;

            {
                let dr = redrules.dyn_rules.read().await;
                assert_eq!(ts + 1, dr.redlist_cursor);
            }

            assert!(redrules.redlist(0).await.is_empty());
            assert_eq!(
                1,
                redrules.redrules(0).await.len(),
                "stale rules should not be added"
            );
        }

        Ok(())
    }

    #[actix_web::test]
    async fn init_redlimit_fn_works() -> anyhow::Result<()> {
        let cfg = conf::Conf::new()?;
        let pool = web::Data::new(redis::new(cfg.redis.clone()).await?);

        assert!(init_redlimit_fn(pool.clone()).await.is_ok());
        assert!(init_redlimit_fn(pool.clone()).await.is_ok());

        Ok(())
    }

    #[actix_web::test]
    async fn limiting_works() -> anyhow::Result<()> {
        let cfg = conf::Conf::new()?;
        let pool = web::Data::new(redis::new(cfg.redis.clone()).await?);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(1, 8, 1000, 5, 300)).await?;
        assert_eq!(LimitResult(1, 0), res);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(3, 8, 1000, 5, 300)).await?;
        assert_eq!(LimitResult(4, 0), res);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(3, 8, 1000, 5, 300)).await?;
        assert_eq!(4, res.0);
        assert!(res.1 > 0);

        sleep(Duration::from_millis(res.1 + 1)).await;
        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(3, 8, 1000, 5, 300)).await?;
        assert_eq!(LimitResult(7, 0), res);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(2, 8, 1000, 5, 300)).await?;
        assert_eq!(7, res.0);
        assert!(res.1 > 0);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(1, 8, 1000, 5, 300)).await?;
        assert_eq!(LimitResult(8, 0), res);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(1, 8, 1000, 5, 300)).await?;
        assert_eq!(8, res.0);
        assert!(res.1 > 0);

        sleep(Duration::from_millis(res.1 + 1)).await;
        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(1, 8, 1000, 5, 300)).await?;
        assert_eq!(LimitResult(1, 0), res);

        let res = limiting(pool.clone(), "TT:core:user1", LimitArgs(1, 1, 1000, 5, 300)).await?;
        assert_eq!(1, res.0);
        assert!(res.1 > 0, "with new max count");

        Ok(())
    }

    #[actix_web::test]
    async fn redrules_add_load_works() -> anyhow::Result<()> {
        let ns = "redrules_add_load_works";
        let cfg = conf::Conf::new()?;
        let pool = web::Data::new(redis::new(cfg.redis.clone()).await?);
        let ts = unix_ms();

        let cli = pool.get().await?;

        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert!(dyn_redrules.is_empty());

        let mut rules = HashMap::new();
        redrules_add(pool.clone(), ns, "core", &rules).await?;
        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert!(dyn_redrules.is_empty());

        rules.insert("path1".to_owned(), (2, 100));
        redrules_add(pool.clone(), ns, "core", &rules).await?;
        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert_eq!(1, dyn_redrules.len());

        redrules_add(pool.clone(), ns, "core2", &rules).await?;
        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert_eq!(2, dyn_redrules.len());

        let rt = dyn_redrules
            .get("core:path1")
            .ok_or(anyhow::Error::msg("'core:path1' not exists"))?
            .to_owned();
        assert_eq!(2, rt.0);
        assert!(rt.1 > ts);

        let rt = dyn_redrules
            .get("core2:path1")
            .ok_or(anyhow::Error::msg("'core2:path1' not exists"))?
            .to_owned();
        assert_eq!(2, rt.0);
        assert!(rt.1 > ts);

        let dyn_redrules = redrules_load(cli.clone(), ns, ts + 210).await?;
        assert_eq!(0, dyn_redrules.len());

        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert_eq!(2, dyn_redrules.len());

        sleep(Duration::from_millis(210)).await;
        let dyn_redrules = redrules_load(cli.clone(), ns, ts + 210).await?;
        assert_eq!(0, dyn_redrules.len(), "will sweep stale rules");
        let dyn_redrules = redrules_load(cli.clone(), ns, ts).await?;
        assert_eq!(0, dyn_redrules.len(), "should sweeped stale rules");

        Ok(())
    }

    #[actix_web::test]
    async fn redlist_add_load_works() -> anyhow::Result<()> {
        let ns = "redlist_add_load_works";
        let cfg = conf::Conf::new()?;
        let pool = web::Data::new(redis::new(cfg.redis.clone()).await?);
        let ts = unix_ms();
        let cli = pool.get().await?;

        let dyn_redlist = redlist_load(cli.clone(), ns, ts, 0).await?;
        assert!(dyn_redlist.1.is_empty());

        let mut rules: HashMap<String, u64> = HashMap::new();
        redlist_add(pool.clone(), ns, &rules).await?;
        let dyn_redlist = redlist_load(cli.clone(), ns, ts, 0).await?;
        assert!(dyn_redlist.1.is_empty());

        rules.insert("user1".to_owned(), 100);
        redlist_add(pool.clone(), ns, &rules).await?;
        let dyn_redlist = redlist_load(cli.clone(), ns, ts, 0).await?;
        assert!(dyn_redlist.0 > ts - 1000);
        assert_eq!(1, dyn_redlist.1.len());

        redlist_add(pool.clone(), ns, &rules).await?;
        let dyn_redlist = redlist_load(cli.clone(), ns, ts, dyn_redlist.0).await?;
        assert!(dyn_redlist.0 > ts);
        assert_eq!(1, dyn_redlist.1.len());

        let rt = dyn_redlist
            .1
            .get("user1")
            .ok_or(anyhow::Error::msg("'user1' not exists"))?
            .to_owned();
        assert!(rt > ts);

        let dyn_redlist = redlist_load(cli.clone(), ns, ts + 210, 0).await?;
        assert_eq!(0, dyn_redlist.1.len());
        let dyn_redlist = redlist_load(cli.clone(), ns, ts, 0).await?;
        assert_eq!(1, dyn_redlist.1.len());

        sleep(Duration::from_millis(210)).await;
        let dyn_redlist = redlist_load(cli.clone(), ns, ts + 210, 0).await?;
        assert_eq!(0, dyn_redlist.1.len(), "will sweep stale rules");
        let dyn_redlist = redlist_load(cli.clone(), ns, ts, 0).await?;
        assert_eq!(0, dyn_redlist.1.len(), "should sweeped stale rules");

        Ok(())
    }
}
