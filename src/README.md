# 后端源码

入口 main.rs 装配配置、连接池、信号处理及 HTTP 服务，lib.rs 定义路由和健康响应，spa.rs 处理静态托管。按资源组织 [routes](routes/README.md)，使用 [domain](domain/README.md) 表达精确业务类型；[blog](blog/README.md)、[auth](auth/README.md)、[idempotency](idempotency/README.md) 隔离职责。

[finance_client](finance_client/README.md) 从 finance OpenAPI 生成；[static](static/README.md) 托管 frontend 产物；[openapi](openapi/README.md) 汇总导出。启动组装见 [startup](startup/README.md)，请求日志见 [telemetry](telemetry/README.md)，不在 handler 混入配置和连接池初始化。请求流与依赖方向见 [架构](../docs/architecture.md)。

数据库生命周期见 database.rs，finance 探测见 finance.rs。

[integrations.rs](integrations.rs) 管理集成 catalog 和 HTTP 契约，[store.rs](integrations/store.rs) 负责版本、测试、启停与审计事务，[credentials.rs](credentials.rs) 封装和持久化密文。

[notifications](notifications.rs) 管理告警规则、事件入库与审计。独立的 [web-notifier](bin/web-notifier.rs) 消费 PostgreSQL outbox 并调用邮件渠道。部署配置见[运行手册](../RUNBOOK.md#告警与邮件进程)。
