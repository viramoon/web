# web 架构

web 是 fertile 的后端网关，负责网络入口、frontend 静态托管、面向浏览器的 API、finance HTTP 客户端及博客。技术栈为 Rust axum + sqlx + PostgreSQL，借鉴 Zero To Production 的分层配置、startup、telemetry、idempotency 与 tests/api，不引入完整 loco。

```text
浏览器 → web（axum）
           ├─ static：frontend Vite 产物
           ├─ routes → auth / domain / idempotency → finance_client → HTTP → finance api
           └─ blog / 业务持久化 → PostgreSQL（app / blog）
                                └─ 通知 outbox → web-notifier → SMTP
```

main.rs 组装进程，lib.rs 管理路由、请求边界与健康检查。backtests.rs 转发回测请求，market.rs 转发行情并汇总新鲜度。认证层从本地 0600 文件载入管理员 Bearer，可建立短期同源会话。管理写请求先鉴权并应用共享滑动窗口限额，再读请求体。回测提交检查 Idempotency-Key，并向 finance 发送一次，由 finance 保存幂等结果。系统、回测和行情状态保留公开读取，按键查询和集成配置限管理员读取。生成的 finance-client crate 独立于手写适配层，金融数值按契约保留精度。目录入口见 [src](../src/README.md)。

web 故障不影响 finance 交易核心，finance 不依赖 Tunnel。规划中的 Webhook 需先落盘再幂等处理。回测写请求超时后的查询与重试见 [运行手册](../RUNBOOK.md)。

frontend 不独立运行，由配置指定的静态目录提供资源，按 fertile tag 验证来源、切换与回滚；缓存和 SPA 回退见 [static](../src/static/README.md)。所有路径配置化并默认 /home/box 下，配置层分 profile，运行模式与 profile 独立。

契约从代码导出，见 [api.md](api.md)；数据库 schema 与迁移见 [database.md](database.md)。部署与发版唯一入口是 fertile deploy/ 和 docs/release.md，web service.yaml 记录组件接口与配置。

## W2 契约来源

后续 W2 使用与 [finance 来源提交](../openapi/finance.commit)一致的以下契约。阶段 0 仅记录来源和版本。

- F1 行情协议 `1.0`。标的、事件与能力的 JSON Schema 及样本 `examples.json` 来自 [`protocol/v1/`](https://github.com/viramoon/finance/tree/b21f24150dae968a147b57eefbe4ffde0f3b4809/protocol/v1)。
- F2 StrategyBundle v1，`apiVersion` 为 `fertile.finance/strategy/v1`。Bundle schema `strategy-bundle-v1.schema.json` 与组件注册表 `component-registry-v1.json` 来自 [`strategies/schema/`](https://github.com/viramoon/finance/tree/b21f24150dae968a147b57eefbe4ffde0f3b4809/strategies/schema)。

## 集成控制面

catalog 确定 connector、可用字段及允许的标的或部署配置槽。创建、修改和凭据轮换均形成草稿版本。连接测试绑定候选版本并保存类型化证据，通过后由管理员显式启用。启用与回滚在 PostgreSQL 事务内切换活动版本并记录审计。行情适配经本机 finance 行情所有者观察，只发送固定标的和测试标识。SMTP 候选使用 notifier 的既有队列，正式投递在领取时读取活动槽。文件适配和 Drive 本机加密预检在 web 发起。见[行情探测契约](market-probes.md)及[本机文件与 Drive 测试](local-integration-probes.md)。

web 用部署接收方的公钥封装凭据，持久化后才提交 PostgreSQL 引用。将来由授权消费者依据活动配置或对应待定候选取得引用并在各自边界内解封。接收路径、权限、轮换和故障恢复见[运行手册](../RUNBOOK.md)。

管理员运维端点消费 T2b 的备份状态与授权的 F3 缺口快照。两种生产者各自发布状态，web 读取并在响应前裁剪至类型化字段。备份观察时间、已验证截点与隔离恢复时间分别导出。见[接口文档](api.md#备份与采集质量)。

## finance HTTP 调用

生成客户端持有一个复用连接的 reqwest Client。web 的 Finance 适配层为 GET 和写请求提供独立的调用入口。对连接失败、超时、无效响应和 5xx，GET 按 profile 的次数进行有限重试；每次延迟在指数增长的上限的一半与上限之间随机选取。客户端关闭自动重定向和底层自动重试。业务 4xx 保留状态和统一错误结构。写请求只发送一次。

所有路由共享熔断器。最终调用连续失败达到阈值后进入 open，即时拒绝请求。冷却后只放行一个 half_open 试探，其余请求立即返回 503。试探成功或收到有效业务 4xx 恢复 closed；失败或试探请求被取消则重新冷却。请求按进入时的状态代际登记，旧并发响应不能覆盖新的熔断状态。健康探测只用一次独立的短时限，系统状态上的 finance.circuit 与 JSON 日志都记录熔断状态。

## 告警与 SMTP

内部生产者通过 [record_signal](../src/notifications/store/ingest.rs) 写入条件、阶段、事件时间和不透明业务键。写入在同一个 PostgreSQL 事务中更新事故状态、保存事件并入队。当前由 notifier 生成 SMTP 健康事件。采集器、运维和行情异常的生产者在对应集成中接入此窄接口。

`web-notifier` 以 PostgreSQL 会话锁协调单个发送者，在 SMTP I/O 前持久化发送尝试，并在 SMTP 返回后独立结算。静默、冷却、滚动额度和恢复汇总使用同一队列和审计。渠道边界提供 `validate`、`test`、`send`，v1 注册邮件。模板根据类型化条件生成固定说明和私有 `/alerts` 入口，汇总只含计数，付费行情正文和凭据没有进入模板或告警持久化的字段。
