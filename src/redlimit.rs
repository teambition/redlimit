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

use super::{conf::Rule, redis::RedisPool};

pub struct RedRulesData {
    floor: Vec<u64>,
    defaut: Rule,
    rules: HashMap<String, Rule>,
    dyn_rules: HashMap<String, (u64, u64)>,
    dyn_blacklist: HashMap<String, u64>,
    dyn_blacklist_cursor: u64,
}

pub struct RedRules {
    inner: RwLock<RedRulesData>,
}

impl RedRules {
    pub fn new(rules: &HashMap<String, Rule>) -> Self {
        let mut rr = RedRulesData {
            floor: vec![3, 10000, 1, 1000],
            defaut: Rule {
                limit: vec![10, 5000, 3, 1000],
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
            rr.dyn_blacklist.insert(k, v);
        }

        rr.dyn_rules.retain(|_, v| v.1 > now);
        for (k, v) in dyn_rules {
            rr.dyn_rules.insert(k, v);
        }
    }
}

// (quantity, max count per period, period with millisecond, max burst, burst
// period with millisecond)
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
    let cmd = resp::cmd("FUNCTION").arg("LOAD").arg(REDLIMIT);
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

static REDLIMIT: &str = r#"#!lua name=redlimit

local function now_ms()
  local now = redis.call('TIME')
  return tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
end

-- keys: <an identifier to rate limit against>
-- args (should be well formed): <quantity> <max count per period> <period with millisecond> [<max burst> <burst period with millisecond>]
-- return: [<count in period> or 0, <wait duration with millisecond> or 0]
local function limiting(keys, args)
  local quantity = tonumber(args[1]) or 1
  local max_count = tonumber(args[2]) or 0
  local period = tonumber(args[3]) or 0
  local max_burst  = tonumber(args[4]) or 0
  local burst_period  = tonumber(args[5]) or 1000

  local result = {quantity, 0}
  if quantity > max_count then
    result[2] = 1
    return result
  end

  local burst = 0
  local burst_at = 0
  local limit = redis.call('HMGET', keys[1], 'c', 'b', 't')
  -- field:c(count in period)
  -- field:b(burst in burst period)
  -- field:t(burst start time, millisecond)

  if limit[1] then
    result[1] = tonumber(limit[1]) + quantity

    if max_burst > 0 then
      local ts = now_ms()
      burst = tonumber(limit[2]) + quantity
      burst_at = tonumber(limit[3])
      if burst_at + burst_period <= ts  then
        burst = quantity
        burst_at = ts
      elseif burst > max_burst then
        result[1] = result[1] - quantity
        result[2] = burst_at + burst_period - ts
        return result
      end
    end

    if result[1] > max_count then
      result[1] = result[1] - quantity
      result[2] = redis.call('PTTL', keys[1])

      if result[2] <= 0 then
        result[2] = 1
        redis.call('DEL', keys[1])
      end
    elseif max_burst > 0 then
      redis.call('HSET', keys[1], 'c', result[1], 'b', burst, 't', burst_at)
    else
      redis.call('HSET', keys[1], 'c', result[1])
    end

  else
    if max_burst > 0 then
      burst = quantity
      burst_at = now_ms()
    end

    redis.call('HSET', keys[1], 'c', quantity, 'b', burst, 't', burst_at)
    redis.call('PEXPIRE', keys[1], period)
  end

  return result
end

-- keys: <redlist key>
-- args: <member> <expire duration with millisecond> [<member> <expire duration with millisecond> ...]
-- return: integer or error
local function redlist_add(keys, args)
  local key_add = '_RLA_' .. keys[1]
  local key_exp = '_RLE_' .. keys[1]
  local ts = now_ms()
  local members = redis.call('ZRANGE', key_exp, '-inf', '(' .. ts, 'BYSCORE')
  if #members > 0 then
    redis.call('ZREM', key_add, unpack(members))
    redis.call('ZREM', key_exp, unpack(members))
  end

  if #args == 0 then
    return 0
  end

  local members_add = {}
  local members_exp = {}
  for i = 1, #args, 2 do
    members_add[i] = ts + i
    members_add[i + 1] = args[i]
    members_exp[i] = ts + (tonumber(args[i + 1]) or 1000)
    members_exp[i + 1] = args[i]
  end

  redis.call('ZADD', key_exp, unpack(members_exp))
  return redis.call('ZADD', key_add, unpack(members_add))
end

-- keys: <redlist key>
-- args: <cursor>
-- return: [<cursor>, <member>, <ttl with millisecond>, <member>, <ttl with millisecond> ...] or error
local function redlist_scan(keys, args)
  local key_add = '_RLA_' .. keys[1]
  local key_exp = '_RLE_' .. keys[1]
  local cursor = tonumber(args[2]) or 0

  local res = {}
  local members = redis.call('ZRANGE', key_add, cursor, 'inf', 'BYSCORE', 'LIMIT', 0, 10000)
  if #members > 0 then
    local ttls = redis.call('ZMSCORE', key_exp, unpack(members))
    table.insert(res, redis.call('ZSCORE', key_add, members[#members]))
    for i = 1, #members, 1 do
      table.insert(res, members[i])
      table.insert(res, ttls[i] or '0')
    end
  end
  return res
end

-- keys: <redrule key>
-- args: <scope> <path> <quantity> <expire duration with millisecond>
-- return: integer or error
local function redrules_add(keys, args)
  local key = '_RR_' .. keys[1]
  local ts = now_ms()
  local members = redis.call('ZRANGE', key, '-inf', '(' .. ts, 'BYSCORE')
  if #members > 0 then
    redis.call('HDEL', key, unpack(members))
  end

  if #args == 0 then
    return 0
  end

  local id = args[1] .. args[2]
  local quantity = tonumber(args[3]) or 1
  local ttl = ts + (tonumber(args[4]) or 1000)
  redis.call('ZADD', key, ttl, id)
  return redis.call('HSET', key, id, cjson.encode({args[1], args[2], quantity,  ttl}))
end

-- keys: <redrules key>
-- return: array or error
local function redrules_all(keys, args)
  local key = '_RR_' .. keys[1]
  return redis.call('HVALS', key)
end

redis.register_function('limiting', limiting)
redis.register_function('redlist_add', redlist_add)
redis.register_function('redlist_scan', redlist_scan)
redis.register_function('redrules_add', redrules_add)
redis.register_function('redrules_all', redrules_all)

"#;
