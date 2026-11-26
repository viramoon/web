# PostgreSQL 与迁移

web 拥有 `app`（业务）和 `blog`（博客）两个 PostgreSQL schema。初始化迁移建立 `app.schema_info` 及一条服务版本记录。sqlx 把已经执行的迁移、校验值和执行结果记录在 `_sqlx_migrations`。新增迁移采用 UTC 时间戳命名，已经应用的文件保持原样。

从仓库根运行 `web migrate`，以 `DATABASE_URL` 指定目标库。部署时由运行环境注入连接串并限制配置文件权限。运行角色需读取 `app.schema_info`，并具有集成表所需的 SELECT、INSERT、UPDATE 权限及审计序列的使用权限。迁移角色另需建 schema、表、索引及约束的权限。自动迁移默认启用；使用独立迁移角色时先 `web migrate`，再设置 `WEB_AUTO_MIGRATE=false`。

服务启动时先监听 HTTP。后台从连接池获取连接，检查 `app.schema_info` 的 web 版本、blog schema 和 `app.integration_configs`。缺少迁移、标记不匹配或连接中断时 live 仍为 200，ready 为 503。探测失败从 250ms 退避至最多 4s，恢复后每秒检查一次。`acquire_timeout_ms` 限制池内等待时间，`connect_timeout_ms` 限制获取可用连接（包括新建和重连）与执行探测语句各自的时间。`/api/v1/system/status` 的 `database.ok` 和 `latency_ms` 为最近一次探测结果，失败时延迟为空。数据库连接串不写入 HTTP 响应。

## 集成配置与审计

| 表 | 内容 |
| --- | --- |
| `app.integration_configs` | Connector ID、修改 revision 和最新配置版本 |
| `app.integration_versions` | 标的配置、状态和同一集成的可选凭据引用。部分唯一索引保证每个集成最多一个活动版本 |
| `app.integration_credentials` | 密文文件 UUID、固定掩码、格式、版本、轮换和吊销时间 |
| `app.integration_tests` | 候选版本、开始和结束时间及有限状态。每个集成最多一个待定测试 |
| `app.integration_audit` | 管理员、版本、操作、测试引用和时间。各状态变更与审计同事务提交 |

每个修改事务锁住集成行并校验 revision。候选测试启动、状态结算也更新 revision。启动新测试、修改、吊销或停用可以使旧测试失效，迟到结果无法覆盖。数据库中保存配置和凭据元数据，凭据密文与 PostgreSQL 分开持久化。

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
