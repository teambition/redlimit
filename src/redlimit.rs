use std::collections::HashMap;

use actix_web::web;
use rustis::resp;
use serde::Serialize;

use super::{conf::Rule, redis::RedisPool};

pub struct RedRules {
    floor: Vec<u64>,
    defaut: Rule,
    rules: HashMap<String, Rule>,
    dyn_rules: HashMap<String, (u16, u64)>,
    dyn_blacklist: HashMap<String, u64>,
}

impl RedRules {
    pub fn new(rules: &HashMap<String, Rule>) -> Self {
        let mut rr = RedRules {
            floor: vec![3, 10000, 1, 1000],
            defaut: Rule {
                limit: vec![10, 5000, 3, 1000],
                path: HashMap::new(),
            },
            rules: HashMap::new(),
            dyn_rules: HashMap::new(),
            dyn_blacklist: HashMap::new(),
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

    pub fn limit_args(&self, scope: &str, path: &str, id: &str, now: u64) -> LimitArgs {
        if let Some(ttl) = self.dyn_blacklist.get(id) {
            if *ttl >= now {
                return LimitArgs::new(1, &self.floor);
            }
        }

        let rule = self.rules.get(scope).unwrap_or(&self.defaut);
        if let Some((quantity, ttl)) = self.dyn_rules.get(&prefix_string(scope, path)) {
            if *ttl >= now {
                return LimitArgs::new(*quantity, &rule.limit);
            }
        }

        let quantity = rule.path.get(path).unwrap_or(&1);
        LimitArgs::new(*quantity, &rule.limit)
    }
}

// (quantity, max count per period, period with millisecond, max burst, burst
// period with millisecond)
pub struct LimitArgs(pub u64, pub u64, pub u64, pub u64, pub u64);

impl LimitArgs {
    pub fn new(quantity: u16, others: &Vec<u64>) -> Self {
        let mut args = LimitArgs(quantity as u64, 0, 0, 0, 0);
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
) -> Result<LimitResult, String> {
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
        .arg(prefix_string(scope, id))
        .arg(args.0)
        .arg(args.1)
        .arg(args.2);

    if args.3 > 0 {
        cmd = cmd.arg(args.3);
    }
    if args.4 > 0 {
        cmd = cmd.arg(args.4);
    }

    match pool.get().await {
        Ok(mut cli) => match cli.send(cmd, None).await {
            Ok(data) => {
                if let Ok(rt) = data.to::<(u64, u64)>() {
                    return Ok(LimitResult(rt.0, rt.1));
                }
            }
            Err(e) => return Err(e.to_string()),
        },
        Err(e) => return Err(e.to_string()),
    }

    Ok(LimitResult(0, 0))
}

pub async fn load_fn(pool: RedisPool) -> Result<(), String> {
    match pool.get().await {
        Ok(mut cli) => {
            match cli
                .send(resp::cmd("FUNCTION").arg("LOAD").arg(LIMITER), None)
                .await
            {
                Ok(res) => {
                    if res.is_error() {
                        let err = res.to_string();
                        if !err.contains("already exists") {
                            return Err(err);
                        }
                    }
                    Ok(())
                }
                Err(err) => Err(err.to_string()),
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

fn prefix_string(a: &str, b: &str) -> String {
    let mut s = String::from(a);
    s.push(':');
    s.push_str(b);
    s
}

static LIMITER: &str = r#"#!lua name=redlimit

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
  local key = '_RL_' .. keys[1]
  local ts = now_ms()
  redis.call('ZREMRANGEBYSCORE', key, '-inf', '(' .. ts)

  local members = {}
  for i = 1, #args, 2 do
    members[i] = ts + (tonumber(args[i + 1]) or 1000)
    members[i + 1] = args[i]
  end

  return redis.call('ZADD', key, unpack(members))
end

-- keys: <redlist key>
-- args: <limit count> <cursor>
-- return: [<member>, <ttl with millisecond> ...] or error
local function redlist_scan(keys, args)
  local key = '_RL_' .. keys[1]
  local ts = now_ms()
  local count = tonumber(args[1]) or 100
  local cursor = tonumber(args[2]) or ts

  local res = redis.call('ZRANGE', key, cursor, -1, 'BYSCORE', 'LIMIT', count, 'WITHSCORES')
end

-- keys: <redrule key>
-- args: <scope> <path> <quantity> <expire duration with millisecond>
-- return: integer or error
local function redrules_add(keys, args)
  local key = '_RR_' .. keys[1]
  local id = args[1] .. args[2]
  local ts = now_ms()
  local members = redis.call('ZRANGE', key, '-inf', '(' .. ts, 'BYSCORE')
  if #members > 0 then
    redis.call('HDEL', key, unpack(members))
  end

  local quantity = tonumber(args[3]) or 1
  local ttl = ts + (tonumber(args[4]) or 1000)
  redis.call('ZADD', key, ttl, id)
  redis.call('HSET', key, id, cjson.encode({args[1], args[2], quantity,  ttl}))
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
