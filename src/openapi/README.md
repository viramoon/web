# OpenAPI 汇总与导出

使用 utoipa / utoipa-axum 从 handler 声明汇总 web 接口，导出到 fertile `contracts/web.openapi.json`；先改实现再导出，不手写契约，不手改快照。

TBD：导出命令、工具版本、schema 精确数值表达和差异检查门禁，待接口实现确定。CI 需比较导出与快照差异，破坏性变更需兼容与迁移计划。WebSocket / SSE 消息不由 OpenAPI 自动覆盖，另写协议。当前无 handler，故不创建空 JSON 快照。
