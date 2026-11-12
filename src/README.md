# 后端源码规划

本次所有目录只有 README，不包含 Rust 源码。按资源组织 [routes](routes/README.md)，使用 [domain](domain/README.md) 表达精确业务类型；[blog](blog/README.md)、[auth](auth/README.md)、[idempotency](idempotency/README.md) 隔离职责。

[finance_client](finance_client/README.md) 从 finance OpenAPI 生成；[static](static/README.md) 托管 frontend 产物；[openapi](openapi/README.md) 汇总导出。启动组装放 [startup](startup/README.md)，日志与指标放 [telemetry](telemetry/README.md)，不在 handler 混入配置和连接池初始化。请求流与依赖方向见 [架构](../docs/architecture.md)。

TBD：Rust 模块和可执行入口由后续实现 issue 创建，不采用 v1 gateway / api 的重叠划分，也不引入完整 loco 框架。
