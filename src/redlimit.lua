#!lua name=redlimit

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
