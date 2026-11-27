# API 集成测试

`cargo test --locked` 运行 Rust 单元测试与 PostgreSQL 集成测试。以普通用户运行，将用于 `initdb`、`pg_ctl`、`psql` 的 PostgreSQL bin 目录加入 PATH。数据库测试启动独占的临时 Unix socket 实例，退出时停库，覆盖空库和已初始化 schema、断开时的 live / ready、同一 web 进程恢复就绪，以及集成配置启用、失败回退、并发、审计与脱敏。

短期会话、Cookie、Origin/CSRF、鉴权顺序及日志脱敏可以离线运行 `cargo test --locked --lib w1_http_tests`。凭据封装测试使用临时生成的 RSA 密钥，可通过 `cargo test --locked --lib credentials::tests` 单独运行。

告警集成测试使用独立 PostgreSQL Unix socket、固定 UTC 时间和合成不透明 UUID，覆盖事件/outbox 原子性、重放去重、管理员 API、发送结果与策略。发送端用进程内替身。SMTP 参数及超时测试用临时进程替身，覆盖传输结果归类、稳定 Message-ID、凭据文件和固定模板，执行 `cargo test --locked --lib notifications` 可在不启动 PostgreSQL 的情况下运行。

执行 `cargo test --locked --test api notifications_worker::silence` 验证主动静默。测试现场构造规则、告警和投递，使用相对于数据库测试当天的截止时间、固定的主题 UUID 和进程内 SMTP 替身。覆盖规则静默期间的记录与去重、实例作用范围、时限边界、重叠作用范围、汇总重试、管理员鉴权和静默审计。OpenAPI 路径和权限检查可用 `cargo test --locked --test api openapi_registers_private_silence_operations` 单独运行。

W4 的 SMTP 集成测试使用进程内主、备槽位和临时 PostgreSQL 验证候选失败、显式启用与回滚后选用的投递槽位。行情探测使用固定标的、生成的 UUID、测试时钟和随机回环端口，验证只读证据、禁用 Binance 的零请求和超时后只查原测试。备份与采集缺口测试以合成的 T2b 状态及 F3 计数写入临时目录。文件适配和 rclone 合成往返命令见[本机探测文档](../../docs/local-integration-probes.md#本地验证)。

默认 Rust 测试也执行 rclone 本机加密往返，使用 `/usr/bin/rclone` 或由 `WEB_TEST_RCLONE` 指定的绝对路径。完整行情 HTTP 测试核对落库与读回的候选证据，随后显式启用。质量发布测试运行 Web CLI，使用固定种子的 F3 替身发布合成快照，再通过进程内管理员请求验证查询。

`cargo test --locked --offline --lib quality_gaps_response_matches_exported_watermarks_schema` 从导出的 200 响应 schema 解析缺口覆盖、水位和源观测的引用，校验进程内请求返回的实际字段、空值分支和精确计数。使用固定合成快照，无需启动 PostgreSQL 或监听 socket。

W2a 的路由测试使用合成标的、临时管理员凭据和内存路由，检查匿名拒绝、禁止缓存、跨源、focus 设置与清除、共享订阅额度和超大正文。自选测试用临时 PostgreSQL 检查并发 revision、排序和持久性。WAL 测试用固定十进制、时钟与序号验证展示许可、盘口连续性、精度、接续和慢端重置。SSE 测试用内存事件与临时会话验证重连、授权丢失、游标和空窗口告警突发。

`cargo test --lib --locked websocket_` 的端到端用例使用两个随机回环端口运行 WebSocket 客户端、web 路由和合成 finance 替身，验证 101 升级、许可拒绝、注销后重连、额度占满、释放和慢端重置。fixture 用固定 F1 记录和临时管理员会话，finance 替身检查浏览器认证头和 Cookie 留在 web。`websocket_wire_handshake_rejects_unauthenticated_and_cross_origin_clients` 还通过进程内双向流发送真实握手请求，验证匿名和跨源请求在升级前分别收到 401 和 403，无需监听 socket。

执行 `cargo test --lib --locked research::tests` 检查 W2b。测试从固定 YAML、UTC 日期、版本、十进制文本和临时随机管理员凭据构造合成请求与报告。回环 finance 替身验证上传与版本选择的计划一致、行列错误、幂等键恢复、旧深链、因子精度和身份边界。路由限额、深链、生成模型及 OpenAPI 引用也有无需 socket 的测试。

从仓库根目录构建并运行三个本地 HTTP 验证脚本。WEB_BIN 指向本次构建产物，PATH 包含 PostgreSQL 工具。

    cargo build --locked --bin web
    export WEB_BIN="$PWD/target/debug/web"
    python3 -B tests/api/core_http.py
    python3 -B tests/api/finance_resilience.py
    python3 -B tests/api/finance_writes.py
    python3 -B -m unittest discover -s tests/api -p test_http_isolation.py -v

三个脚本使用随机回环端口上的合成 finance 桩、临时静态目录和显式构造的 web 子进程环境。测试客户端直连回环地址，禁用系统代理和 HTTP 重定向。状态与优雅退出用例自行初始化仅监听临时 Unix socket 的 PostgreSQL，迁移后验证状态与退化、请求 ID、深链接、缓存、错误路由及 SIGTERM 在途请求完成，退出时停止实例。另外两个脚本用临时目录内不存在的 Unix socket 路径测试无数据库场景。写权限脚本只使用临时生成的令牌文件，其他 HTTP 脚本将令牌文件指向临时目录内的缺失路径。运行这些脚本只需提供构建产物和工具路径，DATABASE_URL、代理和部署配置都由脚本隔离。
