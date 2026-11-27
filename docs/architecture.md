# web 架构

web 是 fertile 的后端网关，负责网络入口、frontend 静态托管、面向浏览器的 API、finance HTTP 客户端及博客。技术栈为 Rust axum + sqlx + PostgreSQL，借鉴 Zero To Production 的分层配置、startup、telemetry、idempotency 与 tests/api，不引入完整 loco。

```text
浏览器 → web（axum）
           ├─ static：frontend Vite 产物
           ├─ routes → auth / domain / idempotency → finance_client → HTTP → finance api
           └─ blog / 业务持久化 → PostgreSQL（app / blog）
                                └─ 通知 outbox → web-notifier → SMTP
```

main.rs 组装进程，lib.rs 管理路由、请求边界与健康检查。backtests.rs 转发回测请求，market.rs 转发行情并汇总新鲜度。认证层从本地 0600 文件载入管理员 Bearer，可建立短期同源会话。管理写请求先鉴权并应用共享滑动窗口限额，再读请求体。回测提交检查 Idempotency-Key，并向 finance 发送一次，由 finance 保存幂等结果。系统状态、原有回测作业和行情状态允许公开读取。StrategyBundle 作业、按键查询和集成配置限管理员读取。生成的 finance-client crate 独立于手写适配层，金融数值按契约保留精度。目录入口见 [src](../src/README.md)。

web 故障不影响 finance 交易核心，finance 不依赖 Tunnel。规划中的 Webhook 需先落盘再幂等处理。回测写请求超时后的查询与重试见 [运行手册](../RUNBOOK.md)。

frontend 不独立运行，由配置指定的静态目录提供资源，按 fertile tag 验证来源、切换与回滚；缓存和 SPA 回退见 [static](../src/static/README.md)。所有路径配置化并默认 /home/box 下，配置层分 profile，运行模式与 profile 独立。

契约从代码导出，见 [api.md](api.md)；数据库 schema 与迁移见 [database.md](database.md)。部署与发版唯一入口是 fertile deploy/ 和 docs/release.md，web service.yaml 记录组件接口与配置。

## W2 契约来源

W2a 使用从锁定的 finance 提交生成的客户端，浏览器可访问管理员专用的 WAL、投影、产品和质量读取。W6 增加 2.0 永续事件支持。W2b 接入 F10b 的策略文档校验、版本注册与读取、因子目录和报告。策略源文、版本绑定、因子快照及幂等队列由 finance 持有。[finance 来源提交](../openapi/finance.commit) · [契约差异](api.md#finance-契约版本)

- F1 行情协议 `1.0`。标的、事件与能力的 JSON Schema 及样本 `examples.json` 来自 [`protocol/v1/`](https://github.com/viramoon/finance/tree/b21f24150dae968a147b57eefbe4ffde0f3b4809/protocol/v1)。
- F10a 的市场 WS 数据帧使用 finance 导出的 [1.0 schema](../openapi/finance.market-ws.schema.json)。浏览器订阅命令和 web 控制帧由网关契约定义。
- F2 StrategyBundle v1，`apiVersion` 为 `fertile.finance/strategy/v1`。F6 在 Bundle schema 与组件注册表中补充了指标类型及参数规则。[锁定版本的策略 schema 和注册表](https://github.com/viramoon/finance/tree/fc5e153e9727181f5748659c0610b2fbc2b49769/strategies/schema)

SSE 按需轮询已有 finance 作业、PostgreSQL 告警及备份状态，仅保留这三类的有限状态数据。局部回放在内存中，使用进程代际和事件序号。重连、回放和单帧发送均校验 W1 身份。自选使用 PostgreSQL revision 条件写入，focus 为进程内盘口选择。WAL 视图保留单页覆盖，盘口要求同一分区存在可用快照和连续增量。客户端经 Axum WebSocket 进入市场流会话，按单管理员分配订阅额度，再按 finance 产品许可回放已提交 WAL。finance 实时显示流和券商线路数据等待后续契约。[W2a 接口](api.md#行情自选与推送)

## 集成控制面

catalog 确定 connector、可用字段及允许的标的或部署配置槽。创建、修改和凭据轮换均形成草稿版本。连接测试绑定候选版本并保存类型化证据，通过后由管理员显式启用。启用与回滚在 PostgreSQL 事务内切换活动版本并记录审计。行情适配经本机 finance 行情所有者观察，只发送固定标的和测试标识。SMTP 候选使用 notifier 的既有队列，并在首次发送前保存加密配置快照。正式投递读取活动版本的快照引用。文件适配和 Drive 本机加密预检在 web 发起。[行情探测契约](market-probes.md) · [本机文件与 Drive 测试](local-integration-probes.md)

web 用部署接收方的公钥封装凭据，持久化后才提交 PostgreSQL 引用。将来由授权消费者依据活动配置或对应待定候选取得引用并在各自边界内解封。接收路径、权限、轮换和故障恢复见[运行手册](../RUNBOOK.md)。

管理员运维端点消费 T2b 的备份状态与授权的 F3 缺口快照。部署可用 `web publish-quality` 采样 F3 并发布其快照。web 读取文件并在响应前裁剪至类型化字段。备份观察时间、已验证截点与隔离恢复时间分别导出。[接口文档](api.md#备份与采集质量) · [F3 发布配置](quality-publishing.md)

## finance HTTP 调用

生成客户端持有一个复用连接的 reqwest Client。web 的 Finance 适配层为 GET 和写请求提供独立的调用入口。对连接失败、超时、无效响应和 5xx，GET 按 profile 的次数进行有限重试；每次延迟在指数增长的上限的一半与上限之间随机选取。客户端关闭自动重定向和底层自动重试。finance 地址为数字 IPv4 或 IPv6 回环时，普通调用与集成探测直接连接 finance。业务 4xx 保留状态和统一错误结构。写请求只发送一次。

匿名回测分页使用一次正常的 finance 首读，再在固定页数和时间预算内查找公开作业。续读结束时以已确认的公开作业生成响应，只有找到下一条公开作业时才返回本页最后一条公开作业的 ID 作为游标。[回测接口](api.md#作业与行情)

常规 finance 查询、写入和健康探测共享熔断器。最终调用连续失败达到阈值后进入 open，即时拒绝请求。冷却后只放行一个 half_open 试探，其余请求立即返回 503。试探成功或收到有效业务 4xx 恢复 closed。失败或试探请求被取消后重新冷却。请求按进入时的状态代际登记，旧并发响应不能覆盖新的熔断状态。健康探测只用一次独立的短时限，系统状态上的 finance.circuit 与 JSON 日志都记录熔断状态。集成行情测试克隆同一个直连数字回环的 reqwest 客户端，与常规请求共享底层连接池。探测流程独立执行，使用固定探测期限和有界结果查询。[行情探测契约](market-probes.md)

## 告警与 SMTP

内部生产者通过 [record_signal](../src/notifications/store/ingest.rs) 写入条件、阶段、事件时间和不透明业务键。写入在同一个 PostgreSQL 事务中更新事故状态、保存事件并入队。当前由 notifier 生成 SMTP 健康事件。采集器、运维和行情异常的生产者在对应集成中接入此窄接口。

`web-notifier` 以 PostgreSQL 会话锁协调单个发送者，在 SMTP I/O 前持久化发送尝试，并在 SMTP 返回后独立结算。静默、冷却、滚动额度和恢复汇总使用同一队列和审计。渠道边界提供 `validate`、`test`、`send`，v1 注册邮件。模板根据类型化条件生成固定说明和私有 `/alerts` 入口，汇总只含计数，付费行情正文和凭据没有进入模板或告警持久化的字段。
