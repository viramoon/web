# Web HTTP API

默认地址为 http://127.0.0.1:8090。每个响应都有 X-Request-Id。调用者传入合法 UUID 时保留，否则服务生成新 UUID。tracing 的 JSON 日志记录请求 ID、HTTP 方法、匹配的路由模板、状态码与耗时。

请求日志使用匹配的模板，未匹配时记录 <unmatched>，自定义方法统一记录 OTHER。请求头、查询参数和正文都留在请求处理边界内。上游错误的 HTTP 响应按下文既有转发规则返回，日志只记录状态和固定错误类别。请求处理 panic 返回 500 与 error.code 为 internal_error 的通用错误。参见[日志、样本与 CI 防护](data-guard.md)。

| GET | 状态与返回值 |
| --- | --- |
| /healthz/live | 200，进程可提供 HTTP，status 为 ok |
| /healthz/ready | 检查 database、static_index 与 finance 并返回 checks（name、ok、required）。数据库不可用或静态目录缺 index.html 为 503 / not_ready；finance 不可达默认返回 200 / degraded，设置 WEB_REQUIRE_FINANCE=true 后返回 503。全部就绪为 200 / ready |
| /api/v1/system/status | 200，含 web.version、web.commit、web.started_at、database.ok / latency_ms、finance.ok / latency_ms。下游不可用时仍能读取各依赖状态 |

OpenAPI 由 handler 注解和输出类型生成。执行下列命令可在不启动服务或数据库的情况下导出 JSON。

    cargo run --locked -q -- export-openapi > openapi/web.openapi.json

CI 对同一提交导出两次，并与仓库中的[快照](../openapi/web.openapi.json)逐字节比对。正式集成时将本提交的生成物交给 fertile/contracts/web.openapi.json，再由 frontend 使用 Hey API 生成客户端。生成物只能经导出流程更新。

本仓 CI 在临时目录校验导出内容，校验结果只显示状态。web 导出使用排序键和换行，去除行首缩进以满足 WG 的 256 KiB 文件上限。OpenAPI 中的 example、examples 和生成客户端的样例须登记合成来源。生成快照与客户端使用各自的导出和再生成流程。

## finance 契约版本

W6 将 finance 契约从 `617ed83` 升到 `fc5e153e9727181f5748659c0610b2fbc2b49769`。以下是 W6 的 HTTP 路径与 schema 差异。

| 变化 | 路径或 schema | 含义 |
| --- | --- | --- |
| 新增 GET | `/api/v1/market/bars` | F6 主图时钟、来源、覆盖、精确 OHLCV 与逐行指标。 |
| 新增 GET | `/api/v1/indicators` | F6 版本化指标、参数规则、输出与口径目录。 |
| 新增 GET | `/api/v1/indicators/values` | F6 保留 bar 时钟、覆盖与预热状态的指标值。 |
| 新增 schema | `BarProvenance`、`BarSeriesKey`、`CalendarId`、`ChartBar`、`ChartBars`、`ChartContext`、`ChartCoverage`、`ChartValue`、`ChartValues`、`DelayTier`、`DisplayPolicy`、`IndicatorCatalog`、`IndicatorDefinition`、`IndicatorParameter`、`IndicatorRequest`、`ParameterRule`、`ProjectedBar`、`ValueRow`、`ValueStatus` | F6 新路径的响应及关联类型。价格、数量和指标值保留十进制文本。 |
| 新增 schema | `FundingRate`、`OpenInterest` | F4 将费率、可空的下次资金费率时间及持仓量建模为独立 payload。 |
| 更新 schema | `Message` | 新增 `funding_rate`、`open_interest` 两个分支。 |
| 更新 schema | `Event` | 2.0 永续事件的 `revision` 为 `"0"`。允许既有 `perpetual_context`、两个新增分支，以及 `gap`、`reset`、`heartbeat`、`status`。1.0 保留原消息集合。 |

F9c 的场所深度、F9b 的行情线预算、F12 的历史期权与费用估算、F13 的回测 worker 分离在此版本都没有独立的 HTTP 路径或 schema 差异。F4 更新上游 `protocol/v2/event.schema.json` 与 `protocol/v2/capability.schema.json`，分别加入独立上下文与 2.0 控制事件、独立上下文产品能力。F6 更新上游 `strategies/schema/strategy-bundle-v1.schema.json` 与 `strategies/schema/component-registry-v1.json`，增加指标注册和参数规则。这些上游文件由 finance 维护。[锁定版本的行情协议](https://github.com/viramoon/finance/tree/fc5e153e9727181f5748659c0610b2fbc2b49769/protocol)

同一提交的 1.0 读流产物保存在 web，内容与 `617ed83` 的导出一致。[市场 WS schema](../openapi/finance.market-ws.schema.json) · [行情状态 SSE schema](../openapi/finance.market-status-sse.schema.json) · [合成回放样本](../openapi/finance.read-stream-examples.json)

web OpenAPI 的 `Event`、`Message`、`FundingRate`、`OpenInterest` 对应新版 WAL 与投影响应。浏览器按每条事件的 `schema_version` 和 `kind` 解码，保留十进制和大整数文本。web 已有的 bars 和指标端点仍使用 W2a 的 WAL 视图。市场 WebSocket 继续发 1.0 数据帧，遇到 2.0 事件发出 `unsupported_version` 重置。web 运维 SSE 仍按下文的 `/events` 契约运行。运维缺口响应的水位组件见[备份与采集质量](#备份与采集质量)。

W2b 从包含 F10b 的开发提交 `f834478f87dce2e91a560c071334ef9cba4e8f19` 导出 HTTP 和读流产物，三份读流产物与 W6 一致。F10b 的 finance 主线合并提交 `356818db` 在编译 `finance-collector` 时缺少 F9e 模块和类型的导出，主线导出待上游修复。当前内部 HTTP 新增策略校验、版本列表及详情、单份因子评价、作业取消、worker 状态和决策信号。浏览器使用策略与因子读取，以及现有回测代理。取消、worker 状态和决策信号由 finance 内部契约保留。[finance 来源提交](../openapi/finance.commit) · [finance OpenAPI](../openapi/finance.openapi.json)

协调者按 finance 主线 → web → frontend → fertile 更新契约链。frontend 按已合并的 web SHA 更新 `openapi/web.commit`、快照及生成客户端。fertile 更新对应 gitlinks 并运行 T3 契约比对和隔离复测。[生成及校验流程](../src/finance_client/README.md)

## 请求与静态资源

路由未命中时，解码后首段为 `api` 或 `healthz` 的路径返回 JSON 404，判断时忽略空段和 `.` 段。错误结构为 `error.code`、`error.message`，日志由 `X-Request-Id` 关联。

静态文件以 `WEB_STATIC_ROOT` 为根提供，许可证和声明的 URL 为 `/third-party/lightweight-charts/LICENSE` 和 `/third-party/lightweight-charts/NOTICE`。解码并忽略空段和 `.` 段后，恰为 `/NOTICE` 的 GET 返回同一声明文件的原始正文，声明文件缺失时返回 404。对静态资源和页面，解码后含 `..` 段的 GET、HEAD 在读取文件前返回 404。缺失的 `/assets`、`/static`、`/third-party` 及其子路径返回 404。有效的 `/strategies/{id}` 允许策略 ID 含点，按页面处理。其他末段含 `.` 或恰为 `LICENSE`、`NOTICE` 的缺失路径返回 404。页面 GET 在未命中时回退到 `index.html`，包括 `/backtests?job=<id>`。[静态托管](../src/static/README.md)

GET /api/v1/backtests/sample 使用锁定版本的 finance 客户端转发回测摘要。pnl 始终是十进制字符串，原始精度与尾零保留。已有示例返回 200，缺失示例保留 finance 的 404 与错误结构。finance 连接失败或熔断打开时返回 503 / finance_unavailable，超时返回 504 / finance_timeout，无效响应或其他上游故障返回 502。所有错误均使用 error.code、error.message。

system/status 的 finance.circuit 为 closed、open 或 half_open。finance 不可达时可继续读取 web 与数据库状态。健康端点单次探测，不重试，并遵循相同的熔断状态。

## 作业与行情

| 方法与路径 | 请求及响应 |
| --- | --- |
| POST /api/v1/backtests | 管理员 Bearer 或同源会话。Idempotency-Key 必填，1 至 128 个可见 ASCII 字符。JSON 请求接受原 `JobRequest` 或 `BundleSubmission`。返回 202 与 id。每次最多向 finance 发送一次 |
| GET /api/v1/backtests/{id} | 返回状态、请求、结果摘要及错误。原有作业可公开读取，StrategyBundle 作业仅管理员可见。找不到或不可见时返回 404 |
| GET /api/v1/backtests | 按 id 递增分页。limit 默认 20，范围 1 至 100，cursor 为上页的 next_cursor。匿名页的 items 只含公开作业。只要 next_cursor 有值即可继续翻页，items 可能为空 |
| GET /api/v1/market/status | 公开读取行情来源、状态、快照时间、最大年龄及逐合约的 stale 与 age_seconds |

POST 写入关闭返回 403，鉴权失败返回 401，超过 token 的滑动窗口限额返回 429 并提供 Retry-After。幂等键冲突保留 409，参数错误返回 422。504 的提交结果未知，同一请求可用原始键重交，已知 id 可直接查询。GET、POST 仅调用内部 finance，不将管理员 token 传给 finance。

作业列表、详情、提交和按键查询返回 `Cache-Control: no-store`。

system/status 的 market 字段包含 state、fresh、source、updated_at、instrument_count 与 stale_count。仅在状态为 ready、存在合约且均未过期时 fresh 才为 true。finance 可回复但尚未 ready 时，web 仍可读取行情以展示过期原因。应用层不可达时，state 为 unavailable，fresh 为 false。

## 历史回测

GET /api/v1/backtests/datasets 返回标的日历、来源和首尾日期。SmaCrossover 参数和详情的 research 字段复用 finance 的生成契约。GET /api/v1/backtests/by-key 接收管理员 Bearer 或同源会话，以及原请求头 Idempotency-Key，返回已知 ID 或 404。它独立于写入限流，返回 `Cache-Control: no-store`。网页只有查不到原提交后才允许重试同一键与参数。finance 上游只收到幂等键。

`research.windows` 透传 finance 的 `50y`、`10y`、`5y` 和 `longest_sample` 窗口。`Window` 仅要求 `coverage`。窗口标识、标签、日历边界和诊断、覆盖原因、请求年限及会话计数等字段可省略或为 `null`，web 保留上游写法。`coverage: insufficient` 时，上游缺少有效指标，响应中的 `metrics: null` 表示不可用。

`Window.metrics` 引用 `WindowMetrics`。收益率和最大回撤是精确十进制字符串，年化收益率、夏普比率、波动率和换手率也使用十进制字符串，并允许省略或为 `null`。报告级 `research.metrics` 仍使用 `Metrics`。fertile 更新 web 契约快照后，frontend 应重新生成客户端，处理窗口和指标字段的省略及 `null`，并将不可用指标显示为「不可用」。

## 策略与研究

下表路径使用 `/api/v1` 前缀。所有请求使用 W1 管理员身份，响应带 `Cache-Control: no-store`。会话 POST 需要同源 Origin 和 CSRF token，脚本可使用 Bearer。POST 共用 W1 写入限额，每个请求向 finance 发送一次。正文由 web 直接转发给 finance 编译或注册。

| 方法与路径 | 请求及响应 |
| --- | --- |
| `POST /strategies/validate` | 接受 `StrategyDocument`，即 `{format: "yaml" 或 "json", content: "<UTF-8 文件正文>"}`。200 返回 `StrategyPreview`，包含策略 ID、版本、内容哈希和 `CompiledPlan` |
| `POST /strategies` | 接受同一文档，200 返回 `RegisteredStrategy`。同一 ID 和版本可重交同一规范化内容，异内容为 409 |
| `GET /strategies` | 可按 `id` 筛选，`limit` 为 1 至 100，默认 20，`after` 使用上页的 `next_cursor`。返回 `StrategyVersionPage` |
| `GET /strategies/{id}/versions/{version}` | 返回 `StrategyPreview`，按该版本固定的输入元数据编译。未登记时返回 404 |
| `GET /factors` | 读取 `FactorReadPage`。可按 `factor_id`、`version` 筛选，使用 `limit` 和 `after` 翻页 |
| `GET /factors/{id}/evaluations` | 读取 `FactorEvaluationPage`。可按 `version` 筛选，使用 `limit` 和 `after` 翻页 |
| `GET /factors/{id}/evaluations/{evaluation_id}` | 必填查询参数 `version`，返回 `FactorEvaluationDetail`。未发布该版本评价时返回 404 |

文件正文上限为解码后的 256 KiB，以 UTF-8 字节数计。整个 JSON 请求另受 `server.max_body_bytes` 限制，默认 1 MiB。超过任一上限返回 413 `body_too_large`。finance 编译器的 422 响应携带诊断码、消息、JSON Pointer `error.path` 和从 1 起算的 `error.location.line`、`error.location.column`。带定位的注册冲突也保留诊断。校验只生成预览，注册绑定版本，finance 按固定版本和输入元数据读取计划。同一文件的预览与已登记版本在相同输入下返回相同的计划和内容哈希。

`BundleSubmission` 以 `strategy: "StrategyBundle"` 标识。用 `source: {kind: "inline", format, content}` 发送本次文件，或用 `source: {kind: "registered", id, version}` 选择版本。`start` 与 `end` 为 UTC 午夜的半开区间。数据集、成分和口径来自计划中的版本引用。finance 统一编译两种来源并固定回放，同一幂等键、计划内容及时间窗返回同一作业，不同内容或时间窗返回 409。作业详情中的 `BundleRequestView` 返回登记引用和内容哈希。

因子页的 `limit` 为 1 至 100，默认 50，`after` 使用上页的 `next_cursor`。发布快照提供 `as_of`、版本、覆盖、许可、成分范围和可用时间规则。没有已发布快照时，列表返回空 `items` 与 `as_of: null`。报告的成本、收益、IC 等精确数值保留十进制字符串，缺失指标保留 `null`。目录按原值返回 `state`、`tradable` 和 `selectable_for_backtest`，`research_only` 只用于研究展示。[web OpenAPI](../openapi/web.openapi.json)

## 管理会话与产品权限

| 方法与路径 | 请求及响应 |
| --- | --- |
| POST /api/v1/admin/session | 发送现有管理 Bearer 和部署配置的 HTTPS Origin。201 返回 `csrf_token`、`expires_in`（秒），并设置 `__Host-web_admin_session` Cookie |
| DELETE /api/v1/admin/session | 发送会话 Cookie、Origin 与 `X-CSRF-Token`。204 撤销服务端会话并清除 Cookie |
| GET /api/v1/me/capabilities | 200 返回 `subject`（`anonymous` 或 `admin`）及可见的 `products`。有效会话额外返回 `csrf_token`，便于页面重载恢复 |

Cookie 使用 `HttpOnly; Secure; SameSite=Strict; Path=/`，作用域为当前主机。所有 Cookie 管理写入都需 Origin 精确匹配，并发送会话的 CSRF token；新集成写接口收到显式 Bearer 和 Origin 时也校验 Origin。脚本可仅发送 Bearer。权限失败为 401 或 403，管理员登录及写入共享限流。会话、能力及集成接口返回 `Cache-Control: no-store`。

匿名可见 `system`、`backtests`、`market_status`。管理员额外可见 `backtest_submission`、`integrations`、`restricted_market_data`、`notifications` 和 `strategy_research`。回测按原键查询、策略与因子、集成读取、通知读取和 W2a 行情网关使用 W1 管理员身份。

## 行情、自选与推送

以下路径使用 `/api/v1` 前缀。W2a 读取与流只允许管理员会话或 bearer，响应含 `Cache-Control: no-store`。浏览器会话的跨源请求被拒绝，WebSocket 会话握手还要求明确的同源 `Origin`。REST 行情和自选读取每分钟共享 120 次额度，超额返回 429 和 `Retry-After`。自选和 focus 写入沿用 W1 写额度，浏览器会话必须提供同源 `Origin` 和 `X-CSRF-Token`。

| 路径 | F10a 读取语义 |
| --- | --- |
| `GET /market/products` | 分页查询可记录产品和许可。来源可筛选。 |
| `GET /market/wal` | 以 `source` 和 `product` 读取已提交 WAL。使用 `after` 排除已读位置，使用 `through` 固定翻页上界。 |
| `GET /market/projection/latest` | 按 `source`、`product`、`channel`、`instrument_id`、`epoch` 和 `seq` 精确读取投影事件或修订。 |
| `GET /market/watermarks` | 按源和产品读取 WAL 与投影水位。 |
| `GET /quality/finance-gaps` | finance F3 授权产品的缺口、覆盖和分页。按源、产品及可选频道、标的和窗口筛选。F10a 接受 UTC Unix 纳秒的半开窗口 `[from_ts, to_ts)`。 |
| `GET /market/instruments` | 从 finance 行情状态分页读取已观测的标的。可按来源筛选。 |
| `GET /market/bars`、`/market/trades`、`/market/derivatives` | 分别读取已提交的 bar、成交和永续上下文。保留 F1 数据与更正信封。 |
| `GET /market/book`、`/market/quotes` | 对指定来源、产品、标的和频道按快照与连续增量还原盘口，可由同一盘口取得最佳买卖档。 |
| `GET /indicators`、`/indicators/values` | 列出 WAL bar 支持的 `close` 和 `volume` 数列，按 `indicator` 读取对应十进制文本。 |

直接查询保留 finance 的许可标签、源和产品、`as_of`、部分覆盖、可空字段、精确十进制值及大整数文本。WAL 和投影事件需通过本机记录与显示许可检查。既有 `GET /quality/gaps` 从 web 的获准质量快照读取。

WAL 视图使用 `source`、`product` 和 F1 `instrument_id`。盘口和报价还须指定 `channel`。视图每页最多扫描 128 条 WAL 记录，响应给出实际覆盖和继续游标。历史列表可用带时区的半开区间 `[from, until)` 筛选 bar 的开盘时间或事件的交易所时间，`as_of` 按首次可用时间筛选。盘口状态为 `observed`、`snapshot_required` 或 `unknown`，失同步后盘口及最佳档为空。报价为所选频道的观察。它的时效以观测时间与 WAL 覆盖为准。

自选用 `GET /watchlists/{id}` 读取，用 `PUT /watchlists/{id}` 以 `{revision, instrument_ids}` 替换有序列表。未保存的自选返回 revision 0 和空列表。每次成功提交增加 revision，陈旧 revision 返回 409。接受最多 64 个不重复的 F1 标的 ID，ID 校验限规范语法。按完整注册表验证标的和向 finance 提交订阅需求需后续契约。

`GET /market/depth-focus` 读取当前标的。`PUT /market/depth-focus` 接受 `action: set` 加 F1 标的 ID，或 `action: clear`，返回保存后的 `instrument_id`。盘口请求省略标的时使用该选择，清除后同一请求返回 `depth_focus_required`。自选和 focus 的 PUT 超过正文上限时返回 413 `body_too_large`。

`GET /market/subscriptions` 返回进程内单管理员额度 `web.limit`、`web.active` 与 `web.available`。浏览器会话与 bearer 共用 8 个 web 订阅槽。`finance_lines` 为 `finance_contract_pending`，当前券商线路使用量待 finance 提供。期权链、flow 和 exposure 的三个查询返回 503 `finance_contract_pending`。

`GET /events` 返回 `text/event-stream`。事件数据为含 `schema_version: "1.0"` 和 `kind` 的 JSON，事件名称为 `job`、`alert`、`backup`、`permissions`、`reset`。作业、告警和备份来自现有只读来源，流只给最小状态字段。首次订阅收到权限状态与 `snapshot_required`；客户端通过 REST 取得初始快照。使用 `Last-Event-ID` 接续最近 128 个保留事件，ID 格式为 `<UUID>:<十进制序号>`。游标超出保留窗、进程代际变化、慢端掉队时返回 `cursor_unavailable`，告警窗口溢出返回 `alert_window_overrun`。收到重置后重取快照。流每秒检查空闲会话且在发送数据前重新鉴权，最多同时 16 条连接。

`GET /stream/market` 对有效的 WebSocket 升级返回 101。无效升级返回 400，身份或 Origin 校验失败返回 401 或 403，16 条连接已满时返回 429，finance 不可达时返回 503 `finance_unavailable`。finance 的就绪端点实际返回 HTTP 503 且正文符合健康检查契约时，web 将其视为可达但未就绪，仍允许客户端尝试订阅。升级完成后用文本帧发送 `subscribe`，选择源、产品、频道和标的，或显式设置 `use_depth_focus`。每条连接可持有一个订阅。八个共享槽位占满时，后续订阅收到 `subscription_quota_exceeded`。退订、断开、重置或授权失效均释放槽位。

数据帧沿用 F10a 的 F1 v1.0 事件格式。web 控制帧使用 `type` 区分 `checkpoint`、`reset`、`error` 和 `unsubscribed`。仅对通过当前展示许可的 WAL 发送数据。重连重新鉴权，提交最后确认的同分区 WAL `after` 游标。缺少可验证的盘口基线时收到 `snapshot_required`，消费者掉队时收到 `consumer_lagged`。重置后清空旧盘口，再建立订阅。连接内的上游、订阅及许可错误通过控制帧报告。

## 集成配置

所有集成接口需要管理员权限，路径参数 ID 采用 UUID。

| 方法与路径 | 请求及响应 |
| --- | --- |
| GET /api/v1/integrations/catalog | 200 返回 connector、产品、授权类型、允许字段、标的或配置槽、文件类型、测试方式与可用状态、费用类别、自动回补能力 |
| GET /api/v1/integrations | 200 返回配置列表，含 `revision`、`latest`、可空的 `active`、`last_test_id` 和可回滚的 `rollback_versions` |
| GET /api/v1/integrations/{id} | 200 返回单个配置及 ETag。重试前可用此接口读取最新版本和测试 ID |
| POST /api/v1/integrations | 提交 `{"connector":"binance_spot","settings":{"symbol":"BTCUSDT"}}`，201 建立草稿 |
| PATCH /api/v1/integrations/{id} | 提交 `{"action":"configure","settings":{"symbol":"ETHUSDT"}}` 新建草稿，或提交 `{"action":"disable"}` 停用集成。返回 200 |
| PUT /api/v1/integrations/{id}/credential | 仅对要求凭据的 catalog 条目提交 `{"api_token":"…"}`，200 返回新草稿及固定掩码、版本和轮换时间 |
| DELETE /api/v1/integrations/{id}/credential | 请求体为空。200 返回无凭据的草稿，并吊销此集成已有的凭据，停用依赖这些凭据的活动版本 |
| POST /api/v1/integrations/{id}/tests | 请求体为空且无查询参数。将草稿提为候选并记录连接测试，返回 202、测试记录及查询路径 `Location` |
| GET /api/v1/integrations/{id}/tests/{testId} | 200 返回此集成的测试、`config_version`、起止时间、状态和可空的类型化 `evidence` |
| GET /api/v1/integration-tests/{id} | W1 的兼容查询路径。响应与上行一致，路径参数为测试 UUID |
| POST /api/v1/integrations/{id}/activate | 提交 `{"test_id":"<uuid>"}`，凭当前候选的最近一次通过测试原子切换活动版本。返回 200 和新 ETag |
| POST /api/v1/integrations/{id}/rollback | 提交 `{"version":1}`，复制可回滚的配置到新的活动版本。返回 200 和新 ETag |

更新、吊销、发起测试、启用与回滚需要 `If-Match: "1"` 形式的当前配置 revision。缺失返回 428，格式错误返回 422，旧版本或状态冲突返回 409。HTTP 请求达到时限时返回 504 和 `error.code=request_timeout`。发起测试超时后先读取配置的 revision 和最新测试 ID，再查询该测试。启用或回滚超时后先核对 revision 和活动版本。创建超时先读取集成列表，其他修改超时先检查配置 revision。替换候选或再次开始测试时，原待定测试会被标记为 `superseded`。回滚时同样使待定测试失效，并保留历史版本与审计。已吊销凭据关联的版本无法回滚。

行情卡以 `symbol` 选择 catalog 中的固定标的，覆盖 IBKR Paper、Binance Spot、Binance USDⓈ-M、Hyperliquid Perp 和授权期权占位。策略文件、研究文件、SMTP 和 Google Drive 使用 `{"profile":"primary"}` 或 `{"profile":"secondary"}` 选择部署端配置槽。两个 Binance 探测默认关闭。`configure` 只允许 `action` 和 `settings`，`disable` 只允许 `action`。多余字段返回 422。

`test_available` 根据数据库就绪和配置前置条件计算。各槽位、行情生产者和 SMTP notifier 的实际可用性以测试证据为准。

凭据 token 必须为 16 至 4096 个 ASCII 字符。首字符是英文字母或数字，其余字符可另外使用 `-`、`.`、`_`、`~`、`+`，末尾可有最多两个 `=`。服务器路径、任意测试目标、`.env` 内容和 PEM 内容均不接受。密文存储未配置或存储失败时，凭据录入的 PUT 返回 503。凭据吊销的 DELETE 只需数据库可用并通过鉴权和请求校验。响应、日志和错误不含凭据原文，请求正文不写入日志。

测试状态包括 `pending`、`passed`、`failed`、`unavailable` 和 `superseded`。测试与版本绑定，完成后保留活动版本，且只有最新的通过测试允许启用。`evidence.kind` 为 `market`、`local`、`smtp` 或 `adapter`。`details` 提供固定结果码、观测时间及对应的结构化验证摘要。行情证据区分休市、延迟、权限不足和行满。SMTP 候选在 outbox 排队，通过表示中继已接收。启用版本和回滚继承相应测试的加密 SMTP 配置引用，响应仍只提供有限结果码。未绑定快照的旧 SMTP 历史版本不可直接启用或回滚。Drive 候选返回本机加密往返结果。[行情生产者契约](market-probes.md) · [运行手册](../RUNBOOK.md#集成配置及凭据)

## 备份与采集质量

`GET /api/v1/backup/status` 和 `GET /api/v1/quality/gaps` 需要管理员权限，返回 `Cache-Control: no-store`。两个查询从部署提供的本机文件读取，独立于公开系统状态和就绪探测。

- 备份响应包含 `state`、`observed_at`、`last_recoverable_point`、`last_recoverable_at`、`last_full_at`、`last_hourly_at`、`age_seconds`、`alert`、`degraded` 及 `last_isolated_restore`。未配置时状态为 `disabled`。缺失或不合格的文件显示 `unavailable`。有效状态的 `observed_at` 取自 T2b 文件发布时间，`last_recoverable_at` 取自已验证恢复点的数据截点。降级观测可以保留原截点。备份已启用时，水位缺失、时钟倒置、点龄超过 3600 秒或生产者降级均使 `alert` 为真。隔离恢复凭据包括同一远端实际恢复的 `point`、`recoverable_at` 和 `restored_at`，可为空。
- 缺口响应包含 `state`、`as_of`、`coverage`、`items` 和 `next_offset`。`unavailable` 表示暂无有效观测，`available` 可返回空结果。支持 `source`、`product`、`from`、`to` 筛选，与记录的 UTC 半开区间相交即匹配。列表按起始时间倒序，`limit` 范围 1–100，默认 20，`offset` 默认为 0。翻页比较 `as_of`，快照变化后从首屏重新读取。每段含源、产品、标的、频道、原因、恢复类别和可选计数。恢复类别为 `tick_backfilled`、`bar_backfilled`、`book_restored_from_snapshot`、`history_unrecoverable`，`null` 表示待恢复。覆盖项包含许可和可选的源最新、本机持久、投影和远端已验证水位。WAL 位置、纳秒时间和计数使用精确十进制字符串。

在 web OpenAPI 中，`QualityCoverage.watermarks` 为 `null` 或 `QualityWatermarks`，后者直接包含 `local_durable`、`projected`、`remote_verified` 和 `source_latest`。非空 `source_latest` 使用 `QualitySourceObservation`。行情端点 `GET /api/v1/market/watermarks` 使用 finance 的 `MarketWatermarks` 包装和 `SourceObservation`。缺口接口的响应字段与运行时行为保持原状。

质量文件上限 1 MiB。`web publish-quality INPUTS` 对获准且可持久化的产品读取 finance F3 `finance-market-wal quality` 与 `watermarks`，校验后原子发布。根字段为 `version`（`1`）、`observed_at`（RFC 3339 UTC 时间）及 `streams`（最多 64 项）。每项含 F1 `source`、`product`、`license`、可空的 F3 `watermarks` 对象及 F3 `reports` 数组。web 将 F3 闭区间的 `to_ts` 增加一纳秒后导出半开区间。生产者未提供远端水位时 `remote_verified` 为空。有效快照发布前接口返回 `unavailable`。[发布配置](quality-publishing.md) · [运行手册](../RUNBOOK.md#备份与采集缺口)

## 告警规则与邮件通知

这些接口要求管理员 `notifications` 产品权限，返回 `Cache-Control: no-store`。管理员的同源 Cookie 写请求需要匹配的 Origin 和 `X-CSRF-Token`。支持脚本携带管理 Bearer。写请求共享管理员限流。

| 方法与路径 | 请求及响应 |
| --- | --- |
| `GET /api/v1/notification-rules` | 200，按 UUID 升序返回 `items`、`next_cursor`。接受 `limit`（默认 20，范围 1–100）与上一页的 UUID `cursor` |
| `GET /api/v1/notification-rules/{id}` | 200，返回规则和当前 `ETag`。未知 UUID 返回 404 |
| `PUT /api/v1/notification-rules/{id}` | 客户端分配 UUID，带 `If-Match: "0"` 创建返回 201。带当前引号包住的 revision 更新返回 200，成功时返回新 `ETag`。缺少前置条件返回 428，revision 冲突返回 409 |
| `GET /api/v1/alerts` | 200，按序号降序返回 `items`、`next_cursor`。接受 `limit`（默认 20，范围 1–100）、十进制字符串 `cursor`、`unacknowledged=true`。后者只返回待确认的有效告警 |
| `GET /api/v1/alerts/{id}` | 200，按 UUID 读取告警及投递审计。找不到告警返回 404 |
| `POST /api/v1/alerts/{id}/ack` | 空请求，200 返回含 `acknowledged_at` 的告警，重复请求沿用第一次确认时间 |
| `POST /api/v1/notification-channels/email/tests` | 空请求，`Idempotency-Key` 为客户端分配的 UUID。202 返回测试记录及 `Location`，同一个键始终引用同一次测试 |
| `GET /api/v1/notification-channels/email/tests/{testId}` | 200，读取以原 `Idempotency-Key` 为 ID 的测试记录，包括状态与投递尝试。找不到测试返回 404 |

规则字段包括 `condition`、`severity`（`informational`、`important` 或 `urgent`）、`enabled`、`email_enabled`、可空的 `cooldown_seconds` 和 `reminder_seconds`，以及 `min_occurrences`（默认 1，范围 1–100）。规则匹配类型化条件，`smtp_unavailable` 由 notifier 自行记录。当前私有事件入口是 Rust `record_signal`，由 web 生产者传入规则 UUID、类型化条件、阶段、观察时间、窗口起点和不透明主题 UUID。重复窗口在告警 `duplicate_count` 中可追踪，过时观察记为 `ignored`，尚未达到触发阈值的观察记为 `observed`。处于有效事故且已有邮件的紧急规则可在收到后续观察时按提醒间隔再通知。恢复记录独立于确认操作。

告警投递包含 `pending`、`sending`、`accepted`、`failed`、`summarized` 或 `suppressed` 状态。告警列表、详情和确认响应中的 `deliveries` 使用同一审计结构。

| 投递字段 | 含义 |
| --- | --- |
| `attempts`、`deferrals` | 此投递自身的 SMTP 尝试和静默、冷却或限流延迟记录 |
| `summary_id`、`summary_status` | 所属汇总的投递 UUID 和状态。无所属汇总时均为 `null` |
| `summary_alert_id` | 所属汇总的主告警 UUID，可用于 `GET /api/v1/alerts/{id}`。无所属汇总时为 `null` |
| `summary_attempts`、`summary_deferrals` | 所属汇总自身的尝试和延迟记录。无所属汇总或无对应记录时返回空数组 |
| `replaces_id` | 被此替代投递取代的投递 UUID，可在同一告警的 `deliveries` 中查找。原始投递的此字段为 `null` |
| `suppression_reason` | 此投递因规则变更或事故恢复被抑制时记录的原因 |

等待重试的汇总若有主任务或成员被抑制，notifier 会停止原汇总的后续发送，并为仍符合规则的告警重新排队。可在原成员投递的 `summary_attempts` 中查看汇总的超时或拒绝记录，并在替代投递的 `attempts` 中查看其自身尝试。已领取的尝试按实际 SMTP 结果结算，`accepted` 表示中继已接收。SMTP 配置由部署提供，测试请求无正文或查询参数，测试邮件发往配置中的本人邮箱。

管理写入返回 504 时，按 `id` 读取规则 revision 或告警确认状态，或用原 `Idempotency-Key` 作为 `testId` 查询测试。查明结果后才使用原请求重试。`GET /api/v1/system/status` 的 `notifications` 包含 `state`（`idle`、`healthy`、`degraded`、`unavailable`）、失败起始时间和最近尝试、接受时间及结果。HTTP 请求读取最近一次后台数据库轮询的快照。启动时或后台无法读取投递状态时，`state` 为 `unavailable`。SMTP 投递状态独立于 HTTP 就绪探测。

### 主动静默

以下两个资源都支持 GET、PUT、DELETE。每次请求使用对应规则或告警的非零 UUID。

| 路径 | 作用范围 |
| --- | --- |
| `/api/v1/notification-rules/{id}/silence` | 此规则的全部待投递通知，包括现有队列和静默期间新产生的告警、提醒及恢复通知 |
| `/api/v1/alerts/{id}/silence` | 此告警 UUID 所关联的投递。其他告警 UUID 各自使用其所属规则和实例的静默状态 |

GET 返回 200，含 `active` 和 `silence`。`silence` 为此目标最近的一条记录，覆盖有效、已解除和已到期状态。没有记录时为 `null`。记录含 `reason`、`started_by`、`started_at`、`until`，以及可空的 `ended_by`、`ended_at` 和 `end_action`。手动结束动作为 `released` 或 `replaced`，自然到期以 `until` 判断。浏览器在告警页可用 `rule_id` 单独查询所属规则的静默，规则和实例任一生效即暂停该告警投递。告警列表、详情、规则和确认沿用已有响应结构。

PUT 提交含 `until` 与 `reason` 的 JSON。截止时间使用带时区的 RFC 3339，按 PostgreSQL 微秒精度保存，须晚于数据库当前时间。原因去除首尾空白后须为 1–256 个 Unicode 字符，使用单行简短运维说明。PUT 创建或替换此目标的静默，重复提交相同的有效截止时间和原因沿用原记录。DELETE 请求体为空，仅解除此目标当前有效的静默。再次解除已解除或已到期的资源会返回原记录。PUT 和 DELETE 均返回变更后的 200 状态，并在同一事务保留原原因、截止时间和执行操作的管理员身份 `admin`。静默操作独立于规则配置 revision。

服务端保存去除首尾空白后的原因，OpenAPI 在字段描述中说明计数规则。客户端若预校验长度，应先按相同规则去除首尾空白。例如一个普通空格加 256 个字母可提交，响应中的原因保留这 256 个字母。

全部静默请求使用管理员鉴权并返回 `Cache-Control: no-store`。Cookie 写请求需匹配的 Origin 和 CSRF token。未知目标返回 404，格式错误、非法期限或 PUT/DELETE 带查询参数返回 422，权限失败返回 401 或 403，超额返回 429。写入遇到 504 时先 GET 同一资源并核对截止时间、原因及结束动作，再决定是否重试。

静默期间照常保存信号、去重结果和 outbox，适用的邮件保持待发送。静默在截止瞬间失效，到期由投递资格实时判断。手动静默覆盖紧急业务邮件，`WEB_NOTIFIER_URGENT_BYPASS_QUIET` 只针对定时静默。独立邮件测试和 SMTP 候选测试继续使用其既有策略。已经领取的尝试按实际结果结算，后续重试重新检查静默。静默结束后可将尚未尝试的积压告警汇总，并继续执行正常的投递策略。
