# sqlx 数据库迁移约定

PostgreSQL 的 `app`（业务、Webhook 幂等记录）与 `blog`（博客）schema 都归 web 管理，finance 不管理业务数据库。迁移命名为 `<YYYYMMDDHHMMSS>_<描述>.sql`，按时间顺序、唯一时间戳追加。

已应用迁移只增不改，禁止删除、重写历史迁移或手改数据库绕过流程。每次变更在 PR 写前向迁移、数据备份、兼容窗口、回滚 / 补偿方案和验证。不可逆操作需提前导出可恢复备份，不能假设回滚代码能恢复数据。

TBD：迁移命令、事务边界、锁超时和最初 schema，在独立实现 issue 确定。本次没有 SQL 文件或 sqlx 构建配置。流程详见 [database.md](../docs/database.md)。
