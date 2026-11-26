# web 架构

web 是 fertile 的后端网关，负责网络入口、frontend 静态托管、面向浏览器的 API、finance HTTP 客户端及博客。技术栈为 Rust axum + sqlx + PostgreSQL，借鉴 Zero To Production 的分层配置、startup、telemetry、idempotency 与 tests/api，不引入完整 loco。

```text
浏览器 → web（axum）
           ├─ static：frontend Vite 产物
           ├─ routes → auth / domain / idempotency → finance_client → HTTP → finance api
           └─ blog / 业务持久化 → PostgreSQL（app / blog）
```

main.rs 组装进程，lib.rs 管理路由、请求边界与健康检查。backtests.rs 转发回测请求，market.rs 转发行情并汇总新鲜度。认证层从本地 0600 文件载入管理员 Bearer，可建立短期同源会话。管理写请求先鉴权并应用共享滑动窗口限额，再读请求体。回测提交检查 Idempotency-Key，并向 finance 发送一次，由 finance 保存幂等结果。系统、回测和行情状态保留公开读取，按键查询和集成配置限管理员读取。生成的 finance-client crate 独立于手写适配层，金融数值按契约保留精度。目录入口见 [src](../src/README.md)。

web 故障不影响 finance 交易核心，finance 不依赖 Tunnel。规划中的 Webhook 需先落盘再幂等处理。回测写请求超时后的查询与重试见 [运行手册](../RUNBOOK.md)。

frontend 不独立运行，由配置指定的静态目录提供资源，按 fertile tag 验证来源、切换与回滚；缓存和 SPA 回退见 [static](../src/static/README.md)。所有路径配置化并默认 /home/box 下，配置层分 profile，运行模式与 profile 独立。

契约从代码导出，见 [api.md](api.md)；数据库 schema 与迁移见 [database.md](database.md)。部署与发版唯一入口是 fertile deploy/ 和 docs/release.md，web service.yaml 记录组件接口与配置。

## 集成控制面

catalog 确定 connector、可用字段和标的，创建、修改和凭据轮换均形成草稿版本。连接测试只接收登记的 connector、候选版本与不透明凭据引用。成功结果在事务中提升候选并退役原活动版本。W1 使用显式不可用的连接适配器，实际工作由 W2/F 系列承接。集成事务记录单管理员审计，通知和投递将由 W3 管理。

web 用部署接收方的公钥封装凭据，持久化后才提交 PostgreSQL 引用。将来由授权消费者依据活动配置或对应待定候选取得引用并在各自边界内解封。接收路径、权限、轮换和故障恢复见[运行手册](../RUNBOOK.md)。

## finance HTTP 调用

生成客户端持有一个复用连接的 reqwest Client。web 的 Finance 适配层为 GET 和写请求提供独立的调用入口。对连接失败、超时、无效响应和 5xx，GET 按 profile 的次数进行有限重试；每次延迟在指数增长的上限的一半与上限之间随机选取。客户端关闭自动重定向和底层自动重试。业务 4xx 保留状态和统一错误结构。写请求只发送一次。

所有路由共享熔断器。最终调用连续失败达到阈值后进入 open，即时拒绝请求。冷却后只放行一个 half_open 试探，其余请求立即返回 503。试探成功或收到有效业务 4xx 恢复 closed；失败或试探请求被取消则重新冷却。请求按进入时的状态代际登记，旧并发响应不能覆盖新的熔断状态。健康探测只用一次独立的短时限，系统状态上的 finance.circuit 与 JSON 日志都记录熔断状态。
