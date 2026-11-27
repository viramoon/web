# 行情集成的本机测试契约

Web 将候选 UUID、版本和标的交给 finance 的行情所有者，由该所有者完成只读观察。部署通过数字回环地址 `FINANCE_BASE_URL` 配置适配连接。探测与常规 finance 请求共享连接池，客户端关闭代理、重定向和自动重试。探测期限独立于常规 finance 请求的熔断器。锁定的 finance 快照缺少这组路由，探测结果为 `adapter_unavailable`。Web 的启用和回滚记录活动版本。部署端的真实 paper 观察、各源验证及活动版本应用依赖 F10 后续批次。[finance #43](https://github.com/viramoon/finance/issues/43)

| `connector` | `symbol` |
| --- | --- |
| `ibkr_paper` | `SPY`、`QQQ` |
| `hyperliquid_perp` | `BTC`、`ETH` |
| `binance_spot`、`binance_usd_m` | `BTCUSDT`、`ETHUSDT` |

`WEB_BINANCE_PROBES_ENABLED` 默认为 `false`。此时两个 Binance 连接器在发出本机 finance 请求前结束，结果为 `disabled`。配置及测试请求只选择表内固定产品和标的。

## 请求和结果

`POST /integrations/{id}/tests` 带 JSON 字段 `test_id`（UUID）、`config_version`（整数）、`connector`、`symbol`。finance 用同一个 `test_id` 标识此次观察，并校验待测产品及请求标的。无后续查询的即时结果返回 HTTP 200。接受异步观察时返回 HTTP 202。Web 用 `GET /integrations/{id}/tests/{testId}` 查询原 ID。POST 超时也只查询原 ID。一次 Web 探测最多执行两次 GET，且探测总时限为 12 秒。

HTTP 200 使用以下字段。选填字段可省略或返回 `null`。Web 对终态响应执行逐字段类型、标识和值域校验，HTTP 诊断以及字段之外的内容不会进入测试记录。

| 字段 | 内容 |
| --- | --- |
| `integration_id`、`test_id`、`config_version`、`connector`、`symbol` | 与同次请求完全一致 |
| `status` | `verified`、`market_closed`、`delayed`、`no_permission`、`lines_full` 或 `source_unavailable` |
| `observation` | 对象，含 `origin`、`observed_at` 和 `details`。验证通过时必填 |
| `observation.origin` | `live` 或 `replay`。回放记录保留来源并落为 `replay_only` |
| `observation.observed_at` | RFC 3339 时间，实时观察须属于本次测试，容许 5 秒时钟偏差，最大年龄 120 秒 |
| `observation.details` | 含 `kind` 和 `data` 的对象，见下文 |

## IBKR paper

`details.kind` 为 `ibkr_paper`。行情所有者先确认 paper 监听端点、paper 账户和只读设置，复用同一个带唯一 client ID 的行情所有者。通过实际回调记录股票综合 L1、具体期权合约 L1、OI、Greeks 和 Nasdaq 深度。操作范围限行情请求和相应的订阅释放。

| `details.data` 字段 | 实时验证通过所需值 |
| --- | --- |
| `paper_endpoint`、`paper_account`、`read_only`、`unique_market_data_client` | 布尔值，均为 `true` |
| `stock_l1_market_data_type`、`option_l1_market_data_type` | 实际回调代码，均为 `1`。代码 `2` 是冻结，`3` 是延迟，`4` 是延迟冻结 |
| `option_contract_id` | 本次观察过的期权合约整数标识，大于零 |
| `option_open_interest`、`option_greeks`、`nasdaq_depth_callback` | 布尔值，均为 `true` |
| `remaining_l1_lines`、`remaining_depth_lines` | 本次观察的剩余额度，无符号整数，允许零 |

仅在通过上述检查且时间合格时，`verified` 提供启用所需的测试证据。休市、延迟、权限不足、额度已满与源不可用分别返回独立结果。记录只含观察类型和状态，账户号、client ID、报价、数量及回调原文留在行情所有者边界内。

## 加密源

`details.kind` 为 `crypto_market`。验证通过要求 `details.data` 的 `metadata`、`snapshot` 均为 `true`，`price_precision` 为 0 至 38 的整数小数位数，`event_at` 为新事件的 RFC 3339 时间。该事件须在本次探测期间出现，使用与观察时间相同的时钟偏差和年龄上限。价格和数量无需传入此接口。

本机测试 fixture 以测试时钟、固定计数以及生成的 UUID 构造响应。HTTP 集成测试发起候选探测，核对 PostgreSQL 持久化与查询读回的同一份证据，然后显式启用。paper 会话和已启用的公开行情源在部署端完成独立观察。[本机行情 fixture](../src/integrations/market_probe/tests.rs) · [HTTP 集成测试](../tests/api/integrations.rs)
