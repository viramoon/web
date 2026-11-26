# sqlx 数据库迁移约定

PostgreSQL 的 `app`（业务、Webhook 幂等记录）与 `blog`（博客）schema 都归 web 管理，finance 不管理业务数据库。迁移命名为 `<YYYYMMDDHHMMSS>_<描述>.sql`，按时间顺序、唯一时间戳追加。

已应用迁移只增不改，禁止删除、重写历史迁移或手改数据库绕过流程。每次变更在 PR 写前向迁移、数据备份、兼容窗口、回滚 / 补偿方案和验证。不可逆操作需提前导出可恢复备份，不能假设回滚代码能恢复数据。

运行 `web migrate` 应用内嵌迁移。首个迁移创建 `app`、`blog` 和 `app.schema_info`，W1 追加集成配置、版本、凭据引用、测试和审计表，W3 追加告警规则、事故、事件、outbox、投递尝试、延迟记录和通知健康表。后续迁移 `20261127010000` 增加 `app.notification_outbox.replaces_id`，用于记录汇总成员的替代投递。重复运行会跳过已应用版本。数据库就绪检查验证 `app.schema_info` 的 web 版本标记、`blog` schema、`app.integration_configs`、`app.notification_health` 和 outbox 的 `replaces_id` 列。数据库迁移、离线元数据与异常恢复见[数据库说明](../docs/database.md)。
