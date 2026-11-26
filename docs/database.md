# PostgreSQL 与迁移

web 拥有 `app`（业务）和 `blog`（博客）两个 PostgreSQL schema。初始化迁移建立 `app.schema_info` 及一条服务版本记录。sqlx 把已经执行的迁移、校验值和执行结果记录在 `_sqlx_migrations`。新增迁移采用 UTC 时间戳命名，已经应用的文件保持原样。

从仓库根运行 `web migrate`，以 `DATABASE_URL` 指定目标库。部署时由运行环境注入连接串并限制配置文件权限。运行角色需读取 `app.schema_info`，并具有集成和通知表所需的 SELECT、INSERT、UPDATE 权限及审计序列的使用权限。迁移角色另需建 schema、表、索引及约束的权限。自动迁移默认启用；使用独立迁移角色时先 `web migrate`，再设置 `WEB_AUTO_MIGRATE=false`。

服务启动时先监听 HTTP。后台从连接池获取连接，检查 `app.schema_info` 的 web 版本、`blog` schema、`app.integration_configs`、`app.notification_health` 和 `app.notification_outbox.replaces_id`。缺少迁移、标记不匹配或连接中断时 live 仍为 200，ready 为 503。探测失败从 250ms 退避至最多 4s，恢复后每秒检查一次。`acquire_timeout_ms` 限制池内等待时间，`connect_timeout_ms` 限制获取可用连接（包括新建和重连）与执行探测语句各自的时间。`/api/v1/system/status` 的 `database.ok` 和 `latency_ms` 为最近一次探测结果，失败时延迟为空。数据库连接串不写入 HTTP 响应。

数据库就绪后，后台轮询同时刷新通知健康快照。读取失败时，快照中的通知状态为 `unavailable`，数据库就绪继续按连接和迁移探测判定。`/api/v1/system/status` 直接读取后台发布的快照，避免请求等待数据库连接或查询。

## 集成配置与审计

| 表 | 内容 |
| --- | --- |
| `app.integration_configs` | Connector ID、修改 revision 和最新配置版本 |
| `app.integration_versions` | 标的配置、状态和同一集成的可选凭据引用。部分唯一索引保证每个集成最多一个活动版本 |
| `app.integration_credentials` | 密文文件 UUID、固定掩码、格式、版本、轮换和吊销时间 |
| `app.integration_tests` | 候选版本、开始和结束时间、有限状态及可空类型化证据。每个集成最多一个待定测试 |
| `app.integration_audit` | 管理员、版本、操作、测试引用和时间。各状态变更与审计同事务提交 |

每个修改事务锁住集成行并校验 revision。候选测试启动、状态结算也更新 revision。启动新测试、修改、吊销或停用可以使旧测试失效，迟到结果无法覆盖。数据库中保存配置和凭据元数据，凭据密文与 PostgreSQL 分开持久化。

W4 迁移 `20261127020000` 追加测试证据、激活时间和回滚来源。曾启用且凭据仍有效的历史版本可以复制到新版本并原子启用。迁移根据 W1 成功审计回填激活时间。SMTP 限一个集成配置，候选测试与 outbox 通过外键关联并同事务创建。notifier 在终态事务中写入投递结果及测试证据，测试失败不改变活动槽的投递健康。

迁移 `20261127030000` 增加 SMTP 快照 UUID。`app.notification_outbox.smtp_snapshot_id` 仅可写入带 `integration_test_id` 的集成候选测试行。notifier 在首次发送候选测试前保存并绑定快照，后续测试重试沿用该引用。通过的测试把同一引用交给活动配置，回滚复制被选历史版本的引用。数据库仅保存快照 UUID，加密内容和密钥由 notifier 的本机目录持久化。已有未绑定的 SMTP 历史需重新测试后启用。

普通投递及独立通知渠道测试在每次领取时读取当时活动版本的快照引用。启用或回滚后，同一投递的下一次重试可能使用新的活动中继与收件地址。初次启用 W4 SMTP 前，未停用的邮件渠道沿用 W3 主槽。[备份与轮换说明](../RUNBOOK.md#告警与邮件进程)

单独配置 notifier 数据库角色时，授予集成配置、版本、测试和审计的读取权，配置及测试的更新权，以及审计的插入权和序列使用权。

## 通知规则、事务 outbox 和审计

W3 迁移 `20261127000000` 在 `app` 下追加以下表。后续迁移 `20261127010000` 在 outbox 中追加可空的 `replaces_id` 唯一引用。web API 与独立 notifier 共用此库。运维备份需保留这些表，以便恢复告警历史、幂等证据和投递队列。

| 表 | 内容 |
| --- | --- |
| `app.notification_rules` | revision、类型化条件、级别、邮件开关和冷却/提醒配置 |
| `app.notification_incidents` | 按规则、条件和不透明主题 UUID 保存最新观察、命中计数与事故状态 |
| `app.alert_events` | 观察、触发和恢复记录、确认时间、重复次数。规则、条件、主题、窗口起点和阶段组成去重键 |
| `app.notification_outbox` | 出队状态、计划时间、稳定投递 UUID、优先级、汇总关联、替代投递引用和测试幂等键 |
| `app.notification_attempts` | 每次尝试的开始、完成时间及有限结果。`interrupted` 表示结算丢失后的投递结果未知 |
| `app.notification_deferrals` | 静默、冷却和额度延迟，保留每次决定的时间及下一次可尝试时间 |
| `app.notification_health` | 邮件渠道状态、故障区间、尝试与接受时间，用于系统状态和恢复汇总 |

业务事件、事故转移与对应 outbox 同事务写入。同业务键重放累加重复计数，保留原事件 ID。过时观察仍被记录，事故状态按时间向前更新。停用规则或修改条件会抑制待发送任务。事故恢复会抑制旧提醒，已开始的 SMTP 尝试仍记录实际结果。发送事务先占用滚动额度并写尝试记录，再调用 SMTP。进程退出后由下一发送者按超时窗口处理悬挂记录。数据库仅保存投递元数据，邮件原文、收发地址、凭据和行情正文留在各自的来源边界。

等待重试的汇总若有主任务或成员被抑制，notifier 会为仍符合规则的告警分别创建替代投递，包括规则仍有效的主任务。原汇总记为 `suppressed`，保留投递 UUID、成员关系和尝试记录。抑制原因记录在受规则变更影响的投递上。替代投递使用新的 UUID 和 `Message-ID`，继承汇总的重试等待，并继续接受静默、冷却和额度检查。成员停用后再启用，其原有投递的抑制记录继续生效。已领取的尝试按实际 SMTP 结果结算，返回 `accepted` 的原汇总视为已完成投递。

成员行的 `summary_id` 指向汇总 outbox 行。读取投递审计时，API 从汇总行的 `event_id` 取得 `summary_alert_id`，并按汇总投递 UUID 读取 `summary_attempts` 和 `summary_deferrals`。替代投递沿用原告警的 `event_id`，并以 `replaces_id` 引用原投递，所以同一告警可以追踪原汇总和替代链。 [接口文档](api.md#告警规则与邮件通知)

## 自选

W2a 迁移 `20261127040000` 创建 `app.market_watchlists`。v1 的单一管理员拥有自选，表只保存自选 ID、正整数 revision 和有序 F1 标的 ID 列表。新自选以 revision 0 条件创建，既有自选以行内 revision 比较更新，两个并发请求最多一个成功。更新超时先重读该行。自选随 PostgreSQL 备份及恢复，行情正文和流游标不保存在该表。

## 校验与恢复

```sh
web migrate
psql "$DATABASE_URL" -X -c 'SELECT component, version FROM app.schema_info;'
cargo sqlx prepare --workspace
cargo sqlx prepare --check --workspace
SQLX_OFFLINE=true cargo test --locked
```

`.sqlx/` 由 SQLx CLI 0.8.6 对已迁移的隔离库生成。修改查询或迁移后重新生成。数据库恢复测试用 initdb、pg_ctl、createdb 和 psql 创建一个带专用 Unix socket 的一次性集群，通过同一 web 进程执行未迁移 → 迁移 → 停库 → 恢复的 HTTP 验证。将目标 PostgreSQL 的 bin 目录加入 PATH 后运行 `cargo test --test api postgres_ -- --nocapture`，同时验证启动时断库后能自动迁移。

正式部署前先备份和演练恢复，再执行前向迁移。W1 追加迁移可在空库及已有初始化迁移的库执行，保留原 schema 版本标记和数据。回退到 W1 前的 web 二进制时保留数据库及已执行迁移，并设置 `WEB_AUTO_MIGRATE=false`。持久化目录随 fertile 设置，默认 `/home/box/fertile/data/postgres`。
