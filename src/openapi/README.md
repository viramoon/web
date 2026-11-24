# OpenAPI 汇总与导出

[路由及模型](../lib.rs) 使用 utoipa 声明接口。`cargo run --locked -- export-openapi` 输出 OpenAPI 3.1 JSON，独立于配置、数据库和网络。CI 重复导出并比较字节，集成快照由 fertile 生成到 contracts/web.openapi.json。破坏性变更需兼容与迁移计划。WebSocket / SSE 另写协议。生成和消费流程见[接口说明](../../docs/api.md)。
