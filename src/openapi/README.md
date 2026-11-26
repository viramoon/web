# OpenAPI 汇总与导出

[路由及模型](../lib.rs) 使用 utoipa 声明接口。`cargo run --locked -q -- export-openapi > openapi/web.openapi.json` 生成 [OpenAPI 3.1 快照](../../openapi/web.openapi.json)，无需配置、数据库或网络。CI 重复导出、比较快照并上传生成物。fertile 从锁定的 web 提交更新 contracts/web.openapi.json。破坏性变更需兼容与迁移计划。WebSocket / SSE 另写协议。生成和消费流程见[接口说明](../../docs/api.md)。
