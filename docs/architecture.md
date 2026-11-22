# web 架构

web 是 fertile 的后端网关，负责网络入口、frontend 静态托管、面向浏览器的 API、finance HTTP 客户端及博客。技术栈为 Rust axum + sqlx + PostgreSQL，借鉴 Zero To Production 的分层配置、startup、telemetry、idempotency 与 tests/api，不引入完整 loco。

```text
浏览器 → web（axum）
           ├─ static：frontend Vite 产物
           ├─ routes → auth / domain / idempotency → finance_client → HTTP → finance api
           └─ blog / 业务持久化 → PostgreSQL（app / blog）
```

routes 按 trading / backtest / dashboard / blog / health 资源划分，startup 负责组装，telemetry 负责观测。领域层保持精确数值；生成 finance_client 不混入手写业务代码。目录入口见 [src](../src/README.md)。

web 故障不影响 finance 交易核心，finance 不依赖 Tunnel。Webhook 落盘与幂等去重，写操作先查后试；web 的超时不能被理解为订单未执行。

frontend 不独立运行，由配置指定的静态目录提供资源，按 fertile tag 验证来源、切换与回滚；缓存和 SPA 回退见 [static](../src/static/README.md)。所有路径配置化并默认 /home/box 下，配置层分 profile，运行模式与 profile 独立。

契约从代码导出，见 [api.md](api.md)；数据库 schema 与迁移见 [database.md](database.md)。部署与发版唯一入口是 fertile deploy/ 和 docs/release.md，web service.yaml 仅提供组件设计字段。

## finance HTTP 调用

生成客户端持有一个复用连接的 reqwest Client。web 的 Finance 适配层为 GET 和写请求提供独立的调用入口。对连接失败、超时、无效响应和 5xx，GET 按 profile 的次数进行有限重试；每次延迟在指数增长的上限的一半与上限之间随机选取。客户端关闭自动重定向和底层自动重试。业务 4xx 保留状态和统一错误结构。写请求只发送一次。

所有路由共享熔断器。最终调用连续失败达到阈值后进入 open，即时拒绝请求。冷却后只放行一个 half_open 试探，其余请求立即返回 503。试探成功或收到有效业务 4xx 恢复 closed；失败或试探请求被取消则重新冷却。请求按进入时的状态代际登记，旧并发响应不能覆盖新的熔断状态。健康探测只用一次独立的短时限，系统状态上的 finance.circuit 与 JSON 日志都记录熔断状态。
