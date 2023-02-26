#!lua name=redlimit

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

  if #args == 0 then
    return 0
  end

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
  local count = tonumber(args[1]) or 100
  local cursor = tonumber(args[2]) or 0
  if cursor == 0 then
    cursor = now_ms()
  end

  local res = redis.call('ZRANGE', key, cursor, -1, 'BYSCORE', 'LIMIT', count, 'WITHSCORES')
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
