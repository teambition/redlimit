<div align="center">
  <h1>RedLimit</h1>
  <p>
    <strong>RedLimit is a redis-based distributed rate limit HTTP service, implemented with Rust.</strong>
  </p>
</div>

[![CI](https://github.com/teambition/redlimit/actions/workflows/ci.yml/badge.svg)](https://github.com/teambition/redlimit/actions/workflows/ci.yml)
[![Build](https://github.com/teambition/redlimit/actions/workflows/build.yml/badge.svg)](https://github.com/teambition/redlimit/actions/workflows/build.yml)
[![License](http://img.shields.io/badge/license-mit-blue.svg?style=flat-square)](https://raw.githubusercontent.com/teambition/redlimit/main/LICENSE)

## 介绍

RedLimit 是基于 Redis 7 最新 FUNCTION 特性和 IO 多线程能力实现的分布式 API 限速 HTTP 服务，使用 Rust 语言开发。特征如下：
1. 高性能，RedLimit 服务本身无状态，可水平扩展，限速状态保存在 Redis 实例上。Redis 实例是高负载的主要瓶颈，本服务的设计原则之一是尽量降低 Redis 的 CPU 开销。
2. 无需 Redis 持久化存储，允许状态数据丢失，允许切换 Redis 实例。一方面是因为服务会自动加载 FUNCTION 脚本，另一方面限速状态值也无需持久保存。
3. 自动降级，当 Redis 服务不可用时或者负载高延迟过大（100ms）时，RedLimit 服务会自动降级为不限速，不会影响业务；当 Redis 服务恢复时，RedLimit 服务会自动恢复限速能力。
4. 灵活的限速策略，支持爆发性限速控制，支持临时限速权重调整，支持临时限速名单，详见下文。

生产环境实际开销：用 k8s 部署的 RedLimit 服务，Redis 7 实例为 8 核 arm64 CPU，开启了多线程支持，25000 QPS 时，RedLimit 服务 8 个 pod 消耗 CPU 总计为 3，Redis 实例消耗 CPU 为 1.2，内存消耗很少，可忽略。
## 限速策略

限速策略分为静态限速策略和动态限速策略两部分。

### 静态限速策略
静态限速策略在 config https://github.com/teambition/redlimit/blob/main/config/default.toml 文件中配置，每次更新需要重启 RedLimit 服务（基于 k8s Deployment 的 `RollingUpdate` 重启不会影响业务）。

以默认配置为例来了解一下静态限速策略：
```toml
[rules.core]
limit = [100, 10000, 50, 2000]

[rules.core.path]
"GET /v1/file/list" = 5
```

这是一个 `scope` 为 "core" 的策略，其中：
* `limit = [100, 10000, 50, 2000]` 是 "core" 的限速策略值，前两个值定义常规限速值，此示例表示 10000 毫秒内最多消耗 100 个 token。后两个值定义 burst 爆发性或并发性限速值，此示例表示 2000 毫秒内最多消耗 50 个 token。
* `"GET /v1/file/list" = 5` 是 "core" 下的一个自定义 token 权重的限速路径，表示 `GET /v1/file/list` 这个路径一次请求要消耗 5 个 token，而默认只消耗 1 个 token，所以这个路径并发超过 10 个请求会触发爆发性限速，10 秒内逐步发出超过 20 个请求也会触发常规限速。

一个限速请求如下：
```
POST http://localhost:8080/limiting
Content-Type: application/json
```
请求数据如下：
```json
{
  "scope": "core",
  "path": "GET /v1/file/list",
  "id": "user123"
}
```
其中：
* `scope` 是限速作用域，对应了 config 中的某个限速策略，没找到则使用 "*" 默认限速策略，示例中即表明使用 `[100, 10000, 50, 2000]` 这组策略值，可以为空。
* `path` 是限速路径，对应了 config 中的 `scope` 下限速路径定义的一次请求 token 消耗数量，默认为 1 token。其字面含义由业务自行定义，可以为空。
* `id` 是限速主体标记，可以是用户 ID、设备 ID、IP 等。

响应结果如下：
```json
{
  "result": {
    "limit": 100,
    "remaining": 95,
    "reset": 0,
    "retry": 0
  }
}
```
其中：
* `limit` 对应 `x-ratelimit-limit`，表示当前周期（10000 毫秒）内有 100 个 token。
* `remaining` 对应 `x-ratelimit-remaining`，表示当前周期（10000 毫秒）内还剩 95 个 token。
* `reset` 对应 `x-ratelimit-reset`，表示限速计数状态重置的时间点，UNIX EPOCH 秒数，由于精度低，为 0 也可能处于被限速状态。
* `retry` 对应 `retry-after`，但其精度单位为毫秒，为 0 一定表示未被限速，n >= 1 表示被限速，n 毫秒后可以重试。

同时，本次 HTTP 请求会生成一条 JSON 请求日志，类似这样：
```
{"timestamp":"2023-03-22T09:50:02.209734Z","level":"INFO","message":"ok","target":"api","module":"redlimit::context","method":"POST","path":"/limiting","xid":"","status":200,"start":1679478602195,"elapsed":14,"kv":"{\"id\":\"user123\",\"scope\":\"core\",\"bursted\":false,\"path\":\"GETs /v1/file/list\",\"count\":5,\"limited\":false}"}
```
其中：
* `path` 为本次请求的 API 路径。
* `xid` 为本次请求的 `x-request-id`，请求未携带则为空。
* `status` 为本次请求响应状态，正常请求都将响应 200，包括 Redis 处于异常状态时的请求。
* `elapsed` 为本次请求所消耗的时间，单位为毫秒，一般为 0，最大约 100ms 左右。
* `kv.scope`, `kv.path`, `kv.id` 为本次请求的参数。
* `kv.count` 为本次请求后在当前周期内累积消耗的 token 数，正常请求都应该 >= 1，为 0 表示本次请求时 Redis 异常或超时，自动降级为不限速。
* `kv.limited` 为 true 时表示本次请求被限速。
* `kv.bursted` 为 true 时表示本次请求突破了 burst 爆发值，被限速，此时 `limited` 也一定为 true。

### 动态限速策略
动态限速策略包括 redlist 和 redrules 两种，具有生命周期，超过生命周期则失效，详见下文。
动态限速策略通过 HTTP API 动态添加或更新到 Redis 中，并同步给各个 RedLimit 服务运行实例。

## 使用
### 启动服务
RedLimit 服务依赖 Redis 7，请先启动 Redis 服务并在 config 中配置好。

本地开发环境运行：
```bash
cargo run
```

或通过 `CONFIG_FILE_PATH` 环境变量指定 config 文件运行：
```bash
CONFIG_FILE_PATH=/my/config.toml cargo run
```

RedLimit 也提供了 docker 镜像，可以通过 docker 或 k8s 运行（请自行定义配置），
见：https://github.com/teambition/redlimit/pkgs/container/redlimit

## API

### 检查限速状态：`POST /limiting`
详情见上文。
```bash
POST http://localhost:8080/limiting
Content-Type: application/json
```
请求数据如下：
```json
{
  "scope": "core",
  "path": "POST /v1/file/list",
  "id": "user123"
}
```
响应结果如下：
```json
{
  "result": {
    "limit": 100,
    "remaining": 95,
    "reset": 0,
    "retry": 0
  }
}
```

### 查看服务状态：`GET /version`
该 API 可用于健康检测。
```bash
GET http://localhost:8080/version
```

响应结果如下：
```json
{
  "result": {
    "name": "redlimit",
    "version": "0.2.4"
  }
}
```

同时该 API 会产生如下访问日志：
```
{"timestamp":"2023-03-23T01:47:22.789366Z","level":"INFO","message":"ok","target":"api","module":"redlimit::context","method":"GET","path":"/version","xid":"","status":200,"start":1679536042789,"elapsed":0,"kv":"{\"idle_connections\":6,\"connections\":6}"}
```
其中 `kv.idle_connections`, `kv.connections` 为当前服务中 redis pool 状态，`kv.connections` 为 0 表示 redis 服务异常。

### 创建或更新限速名单：`POST /redlist`
RedLimit 支持动态添加限速红名单，名单中的 `id` 都将使用 config 中的 `rules."-"` 规则。
```bash
POST http://localhost:8080/redlist
Content-Type: application/json
```
请求数据如下：
```json
{
  "user1": 50000,
  "user2": 120000,
  "ip3": 120000
}
```
其中，key 为限速主体标记 `id`，value 为规则有效期，单位为毫秒。如果 `id` 不存在，则创建；如果 `id` 存在，则更新其有效期。
示例中，"user1"、"user2"、"ip3" 三个 ID 都将使用 config 中的 `rules."-"` 规则，即 `[3, 10000, 1, 1000]`。
对 "user1" 的限制将在 50 秒后失效，对 "user2" 和 "ip3" 的限制将在 120 秒后失效。

响应结果如下：
```json
{
  "result": "ok",
}
```

### 查看所有有效动态限速名单：`GET /redlist`
该 API 一次性返回所有有效期内的动态限速名单，不支持分页，所以限速名单不应该太多，最好不要超过 10 万个。
```bash
GET http://localhost:8080/redlist
```

响应结果如下：
```json
{
  "result": {
    "ip3": 1679536722731,
    "user1": 1679536652731,
    "user2": 1679536722731
  }
}
```
其中，key 为限速主体标记 `id`，value 为该 `id` 将失效的 UNIX EPOCH 时间点，单位为毫秒，已失效的限速主体不会返回。

### 创建或更新限速策略的限速路径权重：`POST /redrules`
RedLimit 支持动态调整限速策略下限速路径的 token 权重。
```bash
POST http://localhost:8080/redrules
Content-Type: application/json
```
请求数据如下：
```json
{
  "scope": "core",
  "rules": {
    "GET /v1/file/list": [10, 10000],
    "GET /v2/file/list": [8, 20000]
  }
}
```
其中，`scope` 为目标限速策略，此示例为 "core"，`rules` 中的 key 为限速路径 `path`，value[0] 为该路径一次请求 token 消耗数量，value[1] 为该路径规则有效期，单位为毫秒。如果 `path` 不存在，则创建；如果 `path` 存在，则更新其 token 权重和有效期。
示例中，"GET /v1/file/list" 路径的 token 权重为 10，有效期为 10 秒，"GET /v2/file/list" 路径的 token 权重为 8，有效期为 20 秒。

响应结果如下：
```json
{
  "result": "ok",
}
```

### 查看所有有效动态限速策略：`GET /redrules`
该 API 一次性返回所有有效期内的动态限速策略，不支持分页，所以动态限速策略不应该太多，最好不要超过 1 万个。
```bash
GET http://localhost:8080/redrules
```

响应结果如下：
```json
{
  "result": {
    "core:GET /v1/file/list": [10, 1679536684628],
    "core:GET /v2/file/list": [8, 1679536694627]
  }
}
```
其中，key 为限速作用域 `scope` 和限速路径 `path` 的组合，value[0] 为该路径一次请求 token 消耗数量，value[1] 为该路径策略将失效的 UNIX EPOCH 时间点，单位为毫秒，已失效的限速策略不会返回。

## License
Copyright © 2023 [teambition](https://github.com/teambition).

teambition/redlimit is licensed under the MIT License.  See [LICENSE](./LICENSE) for the full license text.
