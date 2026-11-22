# PostgreSQL 与迁移

web 拥有 `app`（业务）和 `blog`（博客）两个 PostgreSQL schema。初始化迁移建立 `app.schema_info` 及一条服务版本记录。sqlx 把已经执行的迁移、校验值和执行结果记录在 `_sqlx_migrations`。新增迁移采用 UTC 时间戳命名；已经应用的文件保持原样。

从仓库根运行 `web migrate`，以 `DATABASE_URL` 指定目标库，例如本机测试用 `postgresql://box@127.0.0.1:5432/web`。部署时由运行环境注入连接串并限制配置文件权限。运行角色须能连接并读取 `app.schema_info`，迁移角色另需建 schema 和表的权限。自动迁移默认启用；使用独立迁移角色时先 `web migrate`，再设置 `WEB_AUTO_MIGRATE=false` 以只读检查启动。

服务启动时先监听 HTTP。后台从连接池获取连接，读取 `app.schema_info` 的 web 版本和 blog schema。缺少迁移、标记不匹配或连接中断时 live 仍为 200，ready 为 503。探测失败从 250ms 退避至最多 4s，恢复后每秒检查一次。`acquire_timeout_ms` 限制池内等待时间，`connect_timeout_ms` 限制获取可用连接（包括新建和重连）与执行探测语句各自的时间。`/api/v1/system/status` 的 `database.ok` 和 `latency_ms` 为最近一次探测结果，失败时延迟为空。数据库连接串不写入 HTTP 响应。

## 校验与恢复

```sh
web migrate
psql "$DATABASE_URL" -X -c 'SELECT component, version FROM app.schema_info;'
cargo sqlx prepare --workspace
cargo sqlx prepare --check --workspace
SQLX_OFFLINE=true cargo test --locked
```

`.sqlx/` 由 SQLx CLI 0.8.6 对已迁移的隔离库生成。修改查询或迁移后重新生成。数据库恢复测试用 initdb、pg_ctl、createdb 和 psql 创建一个带专用 Unix socket 的一次性集群，通过同一 web 进程执行未迁移 → 迁移 → 停库 → 恢复的 HTTP 验证。将目标 PostgreSQL 的 bin 目录加入 PATH 后运行 `cargo test --test api postgres_ -- --nocapture`，同时验证启动时断库后能自动迁移。

正式部署前先备份和演练恢复，再执行前向迁移。骨架迁移只增 schema 和版本表，回退应用时保留数据库及已执行迁移。不兼容变更应先在新目录或新库验证备份、行数和关键记录，再切换受控部署配置。持久化目录随 fertile 设置，默认 `/home/box/fertile/data/postgres`。
