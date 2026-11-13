# PostgreSQL 与迁移

web 负责 PostgreSQL 的业务与博客 schema：

| schema | 归属 |
| --- | --- |
| app | 应用业务、受控操作与 Webhook 持久化 / 幂等索引 |
| blog | 文章及博客业务数据 |

具体表结构、索引、精确数值列与权限 TBD，由对应功能 issue 确定；不在初始化写 SQL。finance 行情使用自己的 Parquet catalog，不把业务数据库扩展成时序服务。

sqlx 迁移采用 `<YYYYMMDDHHMMSS>_<描述>.sql`，只增不改已应用文件，见 [migrations](../migrations/README.md)。流程设计：备份 → 在隔离库验证 → 检查兼容和锁影响 → 按 fertile 发版执行 → 核对迁移版本和 ready。失败时按迁移 PR 给出的补偿 / 恢复方案处理，禁止盲目执行逆向 SQL。

实际数据库目录从配置读取，默认 /home/box/fertile/data/postgres。定期备份到独立位置并演练恢复；TBD：备份频率、保留期、离机目的地、RPO / RTO、迁移命令与角色。回滚代码前先检查 schema 向后兼容，数据不可逆变更必须有已验证的恢复方案。
