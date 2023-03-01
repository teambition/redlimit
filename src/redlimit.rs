use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use actix_web::web;
use anyhow::{Error, Result};
use rustis::resp;
use serde::{Deserialize, Serialize};
use tokio::{sync::RwLock, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;

use super::{conf::Rule, redis::RedisPool, redlimit_lua};

pub struct RedRulesData {
    floor: Vec<u64>,
    defaut: Rule,
    rules: HashMap<String, Rule>,
    dyn_rules: HashMap<String, (u64, u64)>, // scope:path -> (quantity, ttl)
    dyn_blacklist: HashMap<String, u64>,    // id -> ttl
    dyn_blacklist_cursor: u64,
}

pub struct RedRules {
    inner: RwLock<RedRulesData>,
}

impl RedRules {
    pub fn new(rules: &HashMap<String, Rule>) -> Self {
        let mut rr = RedRulesData {
            floor: vec![2, 10000, 1, 1000],
            defaut: Rule {
                limit: vec![5, 5000, 2, 1000],
                path: HashMap::new(),
            },
            rules: HashMap::new(),
            dyn_rules: HashMap::new(),
            dyn_blacklist: HashMap::new(),
            dyn_blacklist_cursor: 0,
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
        RedRules {
            inner: RwLock::new(rr),
        }
    }

    pub async fn redlist(&self, now: u64) -> HashMap<String, u64> {
        let rr = self.inner.read().await;
        let mut redlist = HashMap::new();
        for (k, v) in &rr.dyn_blacklist {
            if *v >= now {
                redlist.insert(k.clone(), *v);
            }
        }
        redlist
    }

    pub async fn redrules(&self, now: u64) -> HashMap<String, (u64, u64)> {
        let rr = self.inner.read().await;
        let mut redrules = HashMap::new();
        for (k, v) in &rr.dyn_rules {
            if v.1 >= now {
                redrules.insert(k.clone(), *v);
            }
        }
        redrules
    }

    pub async fn limit_args(&self, now: u64, scope: &str, path: &str, id: &str) -> LimitArgs {
        let rr = self.inner.read().await;
        if let Some(ttl) = rr.dyn_blacklist.get(id) {
            if *ttl >= now {
                return LimitArgs::new(1, &rr.floor);
            }
        }

        let rule = rr.rules.get(scope).unwrap_or(&rr.defaut);
        if let Some((quantity, ttl)) = rr.dyn_rules.get(&scoped_string(scope, path)) {
            if *ttl >= now {
                return LimitArgs::new(*quantity, &rule.limit);
            }
        }

        let quantity = rule.path.get(path).unwrap_or(&1);
        LimitArgs::new(*quantity, &rule.limit)
    }

    pub async fn dyn_update(
        &self,
        now: u64,
        dyn_blacklist_cursor: u64,
        dyn_blacklist: HashMap<String, u64>,
        dyn_rules: HashMap<String, (u64, u64)>,
    ) {
        let mut rr = self.inner.write().await;
        rr.dyn_blacklist_cursor = dyn_blacklist_cursor;

        rr.dyn_blacklist.retain(|_, v| *v > now);
        for (k, v) in dyn_blacklist {
            if v > now {
                rr.dyn_blacklist.insert(k, v);
            }
        }

        rr.dyn_rules.retain(|_, v| v.1 > now);
        for (k, v) in dyn_rules {
            if v.1 > now {
                rr.dyn_rules.insert(k, v);
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
}

#[derive(Serialize)]
// LimitResult.0: request count;
// LimitResult.1: 0: not limited, > 0: limited, milliseconds to wait;
pub struct LimitResult(pub u64, pub u64);

pub async fn limiting(
    pool: web::Data<RedisPool>,
    scope: &str,
    id: &str,
    args: LimitArgs,
) -> Result<LimitResult> {
    if id.is_empty()
        || args.0 == 0 // 0 quantity
        || args.0 > args.1 // quantity > max count per period
        || args.2 > 60 * 1000 // period > 60s
        || (args.3 > 0 && args.0 > args.3)
    // max burst > 0 && quantity > max burst
    {
        return Ok(LimitResult(0, 0));
    }

    let mut cmd = resp::cmd("FCALL")
        .arg("limiting")
        .arg(1)
        .arg(scoped_string(scope, id))
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

pub async fn redlist_add(pool: web::Data<RedisPool>, args: HashMap<String, u64>) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }

    let mut cmd = resp::cmd("FCALL")
        .arg("redlist_add")
        .arg(1)
        .arg("dyn_blacklist");

    for (k, v) in args {
        cmd = cmd.arg(k).arg(v);
    }

    pool.get().await?.send(cmd, None).await?;
    Ok(())
}

pub async fn redrules_add(
    pool: web::Data<RedisPool>,
    scope: &str,
    rules: &HashMap<String, (u64, u64)>,
) -> Result<()> {
    if !rules.is_empty() {
        let mut cli = pool.get().await?;
        for (k, v) in rules {
            let cmd = resp::cmd("FCALL")
                .arg("redrules_add")
                .arg(1)
                .arg("dyn_rules")
                .arg(scope.to_string())
                .arg(k.clone())
                .arg(v.0)
                .arg(v.1);
            cli.send(cmd, None).await?;
        }
    }
    Ok(())
}

pub async fn load_fn(pool: web::Data<RedisPool>) -> Result<()> {
    let cmd = resp::cmd("FUNCTION")
        .arg("LOAD")
        .arg(redlimit_lua::REDLIMIT);
    let data = pool.get().await?.send(cmd, None).await?;
    if data.is_error() {
        let err = data.to_string();
        if !err.contains("already exists") {
            return Err(Error::msg(data));
        }
    }
    Ok(())
}

pub fn init_redrules_sync(
    pool: web::Data<RedisPool>,
    redrules: web::Data<RedRules>,
) -> (JoinHandle<()>, CancellationToken) {
    let cancel_redrules_sync = CancellationToken::new();
    (
        tokio::spawn(spawn_redrules_sync(
            pool,
            redrules,
            cancel_redrules_sync.clone(),
        )),
        cancel_redrules_sync,
    )
}

async fn spawn_redrules_sync(
    pool: web::Data<RedisPool>,
    redrules: web::Data<RedRules>,
    stop_signal: CancellationToken,
) {
    loop {
        log::info!("starting redrules sync job");

        let cursor = redrules.inner.read().await.dyn_blacklist_cursor;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_millis() as u64;

        let dyn_rules = load_redrules(pool.clone(), now)
            .await
            .map_err(|e| log::error!("load_redrules error: {}", e))
            .unwrap_or(HashMap::new());

        let dyn_blacklist = load_blacklist(pool.clone(), now, cursor)
            .await
            .map_err(|e| log::error!("load_blacklist error: {}", e))
            .unwrap_or((cursor, HashMap::new()));

        if !dyn_rules.is_empty() || !dyn_blacklist.1.is_empty() {
            redrules
                .dyn_update(now, dyn_blacklist.0, dyn_blacklist.1, dyn_rules)
                .await;
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => {
                continue;
            }

            _ = stop_signal.cancelled() => {
                log::info!("gracefully shutting down redrules sync job");
                break;
            }
        };
    }
}

#[derive(Deserialize)]
struct RedRuleEntry(String, String, u64, u64);

async fn load_redrules(
    pool: web::Data<RedisPool>,
    now: u64,
) -> anyhow::Result<HashMap<String, (u64, u64)>> {
    let mut cli = pool.get().await?;
    let redrules_cmd = resp::cmd("FCALL")
        .arg("redrules_all")
        .arg(1)
        .arg("dyn_rules");

    let data = cli.send(redrules_cmd, None).await?.to::<Vec<String>>()?;
    let mut rt: HashMap<String, (u64, u64)> = HashMap::new();
    let mut has_stale = false;
    for s in data {
        if let Ok(v) = serde_json::from_str::<RedRuleEntry>(&s) {
            if v.3 > now {
                rt.insert(scoped_string(&v.0, &v.1), (v.2, v.3));
            } else {
                has_stale = true
            }
        }
    }

    if has_stale {
        let sweep_cmd = resp::cmd("FCALL")
            .arg("redrules_add")
            .arg(1)
            .arg("dyn_rules");
        cli.send(sweep_cmd, None).await?;
    }

    Ok(rt)
}

const REDLIST_SCAN_COUNT: usize = 10000;
async fn load_blacklist(
    pool: web::Data<RedisPool>,
    now: u64,
    cursor: u64,
) -> anyhow::Result<(u64, HashMap<String, u64>)> {
    let mut cli = pool.get().await?;
    let mut cursor = cursor;
    let mut has_stale = false;
    let mut rt: HashMap<String, u64> = HashMap::new();

    loop {
        let blacklist_cmd = resp::cmd("FCALL")
            .arg("redlist_scan")
            .arg(1)
            .arg("dyn_blacklist")
            .arg(cursor);

        let data = cli.send(blacklist_cmd, None).await?.to::<Vec<String>>()?;
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
                match iter.nth(1) {
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
                        break;
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
        let sweep_cmd = resp::cmd("FCALL")
            .arg("redlist_add")
            .arg(1)
            .arg("dyn_blacklist");
        cli.send(sweep_cmd, None).await?;
    }

    Ok((cursor, rt))
}

fn scoped_string(a: &str, b: &str) -> String {
    let mut s = String::from(a);
    s.push(':');
    s.push_str(b);
    s
}

#[cfg(test)]
mod tests {

    use super::{super::conf, *};

    #[actix_rt::test]
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
            LimitArgs(1, 200, 10000, 10, 2000),
            LimitArgs::new(1, &vec![200, 10000, 10, 2000])
        );

        assert_eq!(
            LimitArgs(1, 0, 0, 0, 0),
            LimitArgs::new(1, &vec![200, 10000, 10, 2000, 1])
        );

        Ok(())
    }

    #[actix_rt::test]
    async fn red_rules_works() -> anyhow::Result<()> {
        std::env::set_var("CONFIG_FILE_PATH", "./config/test.toml");

        let cfg = conf::Conf::new()?;
        let redrules = RedRules::new(&cfg.rules);

        {
            let rr = redrules.inner.read().await;
            assert_eq!(vec![3, 10000, 1, 1000], rr.floor);

            assert_eq!(vec![10, 10000, 3, 1000], rr.defaut.limit);
            assert!(rr.defaut.path.is_empty());

            assert_eq!(0, rr.dyn_blacklist_cursor);

            let core_rules = rr
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

            assert!(rr.rules.get("core2").is_none());
        }

        {
            assert!(redrules.redlist(0).await.is_empty());
            assert!(redrules.redrules(0).await.is_empty());

            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user1")
                    .await
            );
            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user2")
                    .await,
                "any user"
            );

            assert_eq!(
                LimitArgs(1, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "POST /v2/file/list", "user1")
                    .await,
                "path not exists"
            );

            assert_eq!(
                LimitArgs(1, 10, 10000, 3, 1000),
                redrules
                    .limit_args(0, "core2", "POST /v1/file/list", "user1")
                    .await,
                "scope not exists"
            );
        }

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_millis() as u64;
        {
            let mut dyn_blacklist = HashMap::new();
            dyn_blacklist.insert("user1".to_owned(), ts + 1000);
            redrules
                .dyn_update(ts, 1, dyn_blacklist, HashMap::new())
                .await;

            {
                let rr = redrules.inner.read().await;
                assert_eq!(1, rr.dyn_blacklist_cursor);
            }

            assert_eq!(1, redrules.redlist(0).await.len());
            assert_eq!(1, redrules.redlist(ts + 1000).await.len());
            assert!(redrules.redlist(ts + 1001).await.is_empty());
            assert!(redrules.redrules(0).await.is_empty());

            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(ts, "core", "POST /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "POST /v1/file/list", "user1")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
        }

        {
            let mut dyn_rules = HashMap::new();
            dyn_rules.insert("core:POST /v1/file/list".to_owned(), (3, ts + 1000));
            dyn_rules.insert("core:GET /v1/file/list".to_owned(), (5, ts + 1000));
            redrules.dyn_update(ts, 2, HashMap::new(), dyn_rules).await;

            {
                let rr = redrules.inner.read().await;
                assert_eq!(2, rr.dyn_blacklist_cursor);
            }

            assert_eq!(1, redrules.redlist(0).await.len());
            assert_eq!(2, redrules.redrules(0).await.len());
            assert_eq!(2, redrules.redrules(ts + 1000).await.len());
            assert!(redrules.redrules(ts + 1001).await.is_empty());

            assert_eq!(
                LimitArgs(1, 3, 10000, 1, 1000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user1")
                    .await,
                "limited by dyn_blacklist"
            );
            assert_eq!(
                LimitArgs(3, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "POST /v1/file/list", "user2")
                    .await,
                "limited by dyn_rules"
            );
            assert_eq!(
                LimitArgs(5, 200, 10000, 10, 2000),
                redrules
                    .limit_args(0, "core", "GET /v1/file/list", "user2")
                    .await,
                "limited by dyn_rules"
            );

            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "POST /v1/file/list", "user1")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
            assert_eq!(
                LimitArgs(2, 200, 10000, 10, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "POST /v1/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
            assert_eq!(
                LimitArgs(1, 200, 10000, 10, 2000),
                redrules
                    .limit_args(ts + 1001, "core", "GET /v1/file/list", "user2")
                    .await,
                "not limited by dyn_blacklist after ttl"
            );
        }

        {
            redrules
                .dyn_update(ts + 1001, ts, HashMap::new(), HashMap::new())
                .await;

            {
                let rr = redrules.inner.read().await;
                assert_eq!(ts, rr.dyn_blacklist_cursor);
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
            dyn_rules.insert("core:POST /v1/file/list".to_owned(), (3, ts + 1000)); // stale rules
            dyn_rules.insert("core:GET /v1/file/list".to_owned(), (5, ts + 1002));

            redrules
                .dyn_update(ts + 1001, ts + 1, HashMap::new(), dyn_rules)
                .await;

            {
                let rr = redrules.inner.read().await;
                assert_eq!(ts + 1, rr.dyn_blacklist_cursor);
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
}
