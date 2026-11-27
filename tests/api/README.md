# API 集成测试

`cargo test --locked` 运行 Rust 单元测试与 PostgreSQL 集成测试。以普通用户运行，将用于 `initdb`、`pg_ctl`、`psql` 的 PostgreSQL bin 目录加入 PATH。数据库测试启动独占的临时 Unix socket 实例，退出时停库，覆盖空库和已初始化 schema、断开时的 live / ready、同一 web 进程恢复就绪，以及集成配置启用、失败回退、并发、审计与脱敏。

短期会话、Cookie、Origin/CSRF、鉴权顺序及日志脱敏可以离线运行 `cargo test --locked --lib w1_http_tests`。凭据封装测试使用临时生成的 RSA 密钥，可通过 `cargo test --locked --lib credentials::tests` 单独运行。

告警集成测试使用独立 PostgreSQL Unix socket、固定 UTC 时间和合成不透明 UUID，覆盖事件/outbox 原子性、重放去重、管理员 API、发送结果与策略。发送端用进程内替身。SMTP 参数及超时测试用临时进程替身，覆盖传输结果归类、稳定 Message-ID、凭据文件和固定模板，执行 `cargo test --locked --lib notifications` 可在不启动 PostgreSQL 的情况下运行。

执行 `cargo test --locked --offline --test api notifier_startup` 验证邮件进程的安全空转。测试运行实际二进制，在私有 PostgreSQL Unix socket 中用固定 UUID 和相对数据库时钟构造待处理与遗留发送任务，比较启动前后的 outbox、尝试、延迟和健康记录。回归用例覆盖无活动 W4 版本、主槽缺收件人、备选槽与快照存储可用时的队列保留。合成 SMTP 凭据由测试现场生成的 UUID 构造，回环监听器只检测连接。测试还检查固定日志、无合成私密值回显及 SIGTERM、SIGINT 的退出码。`notifier_idles_before_reading_database_or_worker_policy` 和 `notifier_with_only_secondary_smtp_idles_before_opening_snapshot_storage` 无需 PostgreSQL 即可验证启动判定。

活动 W4 测试用 notifier 的快照代码在临时目录生成绑定，再在临时数据库标记为活动版本。移除快照目录设置与删除绑定文件分别使用独立的数据库和子进程。测试先核对只读预检的固定空转原因，再验证进程日志、退出码和队列保留。需查询数据库的用例为进程启动和只读预检各预留五秒。移除主槽后，实际二进制应连接原绑定的回环监听器，更新后的备选槽监听器应保持零连接。可单独运行 `active_smtp_route_requires_and_loads_the_bound_snapshot` 离线验证传输选择和解密，用 `cargo test --locked --offline --bin web-notifier` 验证异步预检返回空转后发布日志并等待关停。

`notifier_startup::queue` 使用同一套合成任务和两个回环中继。启动前移走候选绑定文件后，测试等待独立 W3 投递和下一条 W3 额度延迟落库，再核对候选完整记录。恢复原文件并重启后检查原中继收到第二次尝试。另一用例用注入时钟验证缺快照目录时遗留候选在第 69、70 秒的处理。`smtp_disable_committed_before_reservation_preserves_the_ordinary_job` 通过 PostgreSQL 锁确认 worker 已到达领取前的额度检查，再由另一个连接提交实际的 SMTP 停用事务。

`cargo test --locked --offline --test api runtime_loss` 覆盖运行中已经绑定及刚绑定的候选。用固定 outbox UUID、数据库时钟和 UUID 生成的 SMTP 合成凭据，在数据库确认绑定后、传输读取前移走加密文件。测试核对待定状态、尝试审计、脱敏日志和退避边界，恢复文件后观察原回环中继连接。中继只接受并关闭连接。

执行 `cargo test --locked --test api notifications_worker::silence` 验证主动静默。测试现场构造规则、告警和投递，使用相对于数据库测试当天的截止时间、固定的主题 UUID 和进程内 SMTP 替身。覆盖规则静默期间的记录与去重、实例作用范围、时限边界、重叠作用范围、汇总重试、管理员鉴权和静默审计。OpenAPI 路径和权限检查可用 `cargo test --locked --test api openapi_registers_private_silence_operations` 单独运行。

W4 的 SMTP 集成测试使用进程内主、备槽位和临时 PostgreSQL 验证候选失败、显式启用与回滚后选用的投递槽位。行情探测使用固定标的、生成的 UUID、测试时钟和随机回环端口，验证只读证据、禁用 Binance 的零请求和超时后只查原测试。备份与采集缺口测试以合成的 T2b 状态及 F3 计数写入临时目录。文件适配和 rclone 合成往返命令见[本机探测文档](../../docs/local-integration-probes.md#本地验证)。

默认 Rust 测试也执行 rclone 本机加密往返，使用 `/usr/bin/rclone` 或由 `WEB_TEST_RCLONE` 指定的绝对路径。完整行情 HTTP 测试核对落库与读回的候选证据，随后显式启用。质量发布测试运行 Web CLI，使用固定种子的 F3 替身发布合成快照，再通过进程内管理员请求验证查询。

`cargo test --locked --offline --lib quality_gaps_response_matches_exported_watermarks_schema` 从导出的 200 响应 schema 解析缺口覆盖、水位和源观测的引用，校验进程内请求返回的实际字段、空值分支和精确计数。使用固定合成快照，无需启动 PostgreSQL 或监听 socket。

W2a 的路由测试使用合成标的、临时管理员凭据和内存路由，检查匿名拒绝、禁止缓存、跨源、focus 设置与清除、共享订阅额度和超大正文。自选测试用临时 PostgreSQL 检查并发 revision、排序和持久性。WAL 测试用固定十进制、时钟与序号验证展示许可、盘口连续性、精度、接续和慢端重置。SSE 测试用内存事件与临时会话验证重连、授权丢失、游标和空窗口告警突发。

`cargo test --locked --offline --lib f10c_` 用固定时钟、虚构标的和合成十进制数构造期权、深度、敞口、历史、预算及覆盖响应。回环 finance 替身核对查询转发、禁止转发身份头、显示许可和上游错误。进程内用例覆盖匿名、共享节流、边界参数、旧链的 pending、敞口未知情景的本地 503、非法情景的 400 和请求 schema，以及 3.0 事件的时间戳校验。回环用例覆盖混合版本 WAL 和投影的 502 拒绝，以及 5xx 响应的完整错误结构。完整替身测试使用随机本地端口。

`cargo test --locked --offline --lib f10d_` 用固定时钟、合成合约 ID 和精确字符串验证期权流。随机回环端口上的 finance 替身核对两条专用路径和旧入口的窗口、分页、错误及响应原样转发。`f10d_verification_fixtures_match_contract_and_pagination` 在进程内用生成的 F10d 模型校验核验样本，覆盖条件解释对象、正常续页及超大偏移返回空页。其他进程内用例覆盖共享授权与节流、兼容错误、产品许可和 OpenAPI。所有数据来自测试代码中的确定性字面量。

另执行 `cargo test --locked --offline --lib public_openapi_preserves_f1_composition_and_declares_restricted_reads`，核对旧 WAL 和投影的响应引用，以及 `Event`、`Message`、`SchemaVersion` 与基线 OpenAPI 的哈希。

`cargo test --lib --locked websocket_` 的端到端用例使用两个随机回环端口运行 WebSocket 客户端、web 路由和合成 finance 替身，验证 101 升级、许可拒绝、注销后重连、额度占满、释放和慢端重置。fixture 用固定 F1 记录和临时管理员会话，finance 替身检查浏览器认证头和 Cookie 留在 web。`websocket_wire_handshake_rejects_unauthenticated_and_cross_origin_clients` 还通过进程内双向流发送真实握手请求，验证匿名和跨源请求在升级前分别收到 401 和 403，无需监听 socket。

从仓库根目录构建并运行三个本地 HTTP 验证脚本。WEB_BIN 指向本次构建产物，PATH 包含 PostgreSQL 工具。

    cargo build --locked --bin web
    export WEB_BIN="$PWD/target/debug/web"
    python3 -B tests/api/core_http.py
    python3 -B tests/api/finance_resilience.py
    python3 -B tests/api/finance_writes.py
    python3 -B -m unittest discover -s tests/api -p test_http_isolation.py -v

三个脚本使用随机回环端口上的合成 finance 桩、临时静态目录和显式构造的 web 子进程环境。测试客户端直连回环地址，禁用系统代理和 HTTP 重定向。状态与优雅退出用例自行初始化仅监听临时 Unix socket 的 PostgreSQL，迁移后验证状态与退化、请求 ID、深链接、缓存、错误路由及 SIGTERM 在途请求完成，退出时停止实例。另外两个脚本用临时目录内不存在的 Unix socket 路径测试无数据库场景。写权限脚本只使用临时生成的令牌文件，其他 HTTP 脚本将令牌文件指向临时目录内的缺失路径。运行这些脚本只需提供构建产物和工具路径，DATABASE_URL、代理和部署配置都由脚本隔离。
