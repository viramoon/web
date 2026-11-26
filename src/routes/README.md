# HTTP 路由

健康检查在 /healthz/live 与 /healthz/ready，系统状态在 /api/v1/system/status。handler、返回类型与 utoipa 定义见 [lib.rs](../lib.rs)，HTTP 契约和状态码见[接口说明](../../docs/api.md)。业务路由调用领域逻辑和 finance 客户端，交易内核由 finance 提供。

回测作业的提交、详情和分页在 [backtests.rs](../backtests.rs)，市场状态和新鲜度摘要在 [market.rs](../market.rs)。

[auth.rs](../auth.rs) 提供会话与能力查询，[integrations.rs](../integrations.rs) 提供 catalog、配置、凭据与连接测试接口。
