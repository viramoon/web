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
