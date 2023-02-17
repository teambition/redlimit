use actix_web::web;
use rustis::{client::PooledClientManager, resp};
use serde::Serialize;

pub type RedisPool = rustis::bb8::Pool<PooledClientManager>;

#[derive(Serialize)]
// LimitResult.0: request count;
// LimitResult.1: 0: not limited, > 0: limited, milliseconds to wait;
pub struct LimitResult(pub u64, pub u64);

pub async fn limiting(
    pool: web::Data<RedisPool>,
    id: String,
    args: Vec<u64>,
) -> Result<LimitResult, String> {
    if id.is_empty() || args.len() < 2 {
        return Ok(LimitResult(0, 0));
    }

    if args[0] == 0 || args[1] > 60 * 1000 {
        return Ok(LimitResult(0, 0));
    }

    // args: <max count per period> <period ms> [<quantity> <max burst> <burst
    // period ms>]
    let mut cmd = resp::cmd("FCALL").arg("rl_limiting").arg(1).arg(id);
    match args.len() {
        2 => {
            cmd = cmd.arg(args[0]).arg(args[1]);
        }
        3 => {
            cmd = cmd.arg(args[0]).arg(args[1]).arg(args[2]);
        }
        4 => {
            cmd = cmd.arg(args[0]).arg(args[1]).arg(args[2]).arg(args[3]);
        }
        5 => {
            cmd = cmd
                .arg(args[0])
                .arg(args[1])
                .arg(args[2])
                .arg(args[3])
                .arg(args[4]);
        }
        _ => return Ok(LimitResult(0, 0)),
    };

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

const LIMITER: &str = r#"#!lua name=redlimit

-- keys: an identifier to rate limit against
-- args: <max count per period> <period ms> [<quantity> <max burst> <burst period ms>]

-- HASH: keys[1]
--   field:c(count in a period)
--   field:b(burst in a seceond)
--   field:t(burst start time, ms)

local function now()
  local now = redis.call('TIME')
  return tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
end

local function limiting(keys, args)
  local max_count = tonumber(args[1]) or 0
  local period = tonumber(args[2]) or 0
  local quantity = tonumber(args[3]) or 1
  local max_burst  = tonumber(args[4]) or 0
  local burst_period  = tonumber(args[5]) or 1000

  local result = {quantity, 0}
  if (period < 1) or (quantity < 1) or (quantity > max_count) or (max_burst > max_count) or (max_burst > 0 and quantity > max_burst) then
    result[2] = 1
    return result
  end

  local burst = 0
  local burst_at = 0
  local limit = redis.call('HMGET', keys[1], 'c', 'b', 't')
  if limit[1] then
    result[1] = tonumber(limit[1]) + quantity

    if max_burst > 0 then
      local ms = now()
      burst = tonumber(limit[2]) + quantity
      burst_at = tonumber(limit[3])
      if burst_at + burst_period <= ms  then
        burst = quantity
        burst_at = ms
      elseif burst > max_burst then
        result[1] = result[1] - quantity
        result[2] = burst_at + burst_period - ms
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
      burst_at = now()
    end

    redis.call('HSET', keys[1], 'c', quantity, 'b', burst, 't', burst_at)
    redis.call('PEXPIRE', keys[1], period)
  end

  return result
end

redis.register_function('rl_limiting', limiting)
"#;
