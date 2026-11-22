# Web HTTP API

默认地址为 http://127.0.0.1:8080。每个响应都有 X-Request-Id。调用者传入合法 UUID 时保留；否则服务生成新 UUID。tracing 的 JSON 日志记录请求 ID、HTTP 方法、路径、状态码与耗时。

| GET | 状态与返回值 |
| --- | --- |
| /healthz/live | 200，进程可提供 HTTP，status 为 ok |
| /healthz/ready | 检查 database、static_index 与 finance 并返回 checks（name、ok、required）。数据库不可用或静态目录缺 index.html 为 503 / not_ready；finance 不可达默认返回 200 / degraded，设置 WEB_REQUIRE_FINANCE=true 后返回 503。全部就绪为 200 / ready |
| /api/v1/system/status | 200，含 web.version、web.commit、web.started_at、database.ok / latency_ms、finance.ok / latency_ms。下游不可用时仍能读取各依赖状态 |

OpenAPI 由 handler 注解和输出类型生成。执行下列命令可在不启动服务或数据库的情况下导出 JSON。

    cargo run --locked -q -- export-openapi > /tmp/web.openapi.json

CI 对同一提交导出两次并逐字节比对。正式集成时将本提交的生成物交给 fertile/contracts/web.openapi.json，再由 frontend 使用 Hey API 生成客户端。生成物只能经生成器更新。

未知 /api 路径返回 JSON 404，错误结构为 error.code、error.message，日志由 X-Request-Id 关联。页面 GET 的前端路由回落到 index.html；/healthz、/api、/assets 保留各自的错误语义。

GET /api/v1/backtests/sample 使用锁定版本的 finance 客户端转发回测摘要。pnl 始终是十进制字符串，原始精度与尾零保留。已有示例返回 200，缺失示例保留 finance 的 404 与错误结构。finance 连接失败或熔断打开时返回 503 / finance_unavailable，超时返回 504 / finance_timeout，无效响应或其他上游故障返回 502。所有错误均使用 error.code、error.message。

system/status 的 finance.circuit 为 closed、open 或 half_open。finance 不可达时可继续读取 web 与数据库状态。健康端点单次探测，不重试，并遵循相同的熔断状态。

## 作业与行情

| 方法与路径 | 请求及响应 |
| --- | --- |
| POST /api/v1/backtests | Bearer 管理员鉴权。Idempotency-Key 必填，1 至 128 个可见 ASCII 字符。JSON 请求遵循 finance 的 JobRequest。返回 202 与 id。每次最多向 finance 发送一次 |
| GET /api/v1/backtests/{id} | 公开读取作业，返回状态、请求、结果摘要及错误。不存在时返回 404 |
| GET /api/v1/backtests | 按 id 递增分页。limit 默认 20，范围 1 至 100，cursor 为上页的 next_cursor。返回 items、next_cursor |
| GET /api/v1/market/status | 公开读取行情来源、状态、快照时间、最大年龄及逐合约的 stale 与 age_seconds |

POST 写入关闭返回 403，鉴权失败返回 401，超过 token 的滑动窗口限额返回 429 并提供 Retry-After。幂等键冲突保留 409，参数错误返回 422。504 的提交结果未知，同一请求可用原始键重交，已知 id 可直接查询。GET、POST 仅调用内部 finance，不将管理员 token 传给 finance。

system/status 的 market 字段包含 state、fresh、source、updated_at、instrument_count 与 stale_count。仅在状态为 ready、存在合约且均未过期时 fresh 才为 true。finance 可回复但尚未 ready 时，web 仍可读取行情以展示过期原因。应用层不可达时，state 为 unavailable，fresh 为 false。
