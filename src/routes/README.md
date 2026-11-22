# HTTP 路由

健康检查在 /healthz/live 与 /healthz/ready，系统状态在 /api/v1/system/status。handler、返回类型与 utoipa 定义见 [lib.rs](../lib.rs)，HTTP 契约和状态码见[接口说明](../../docs/api.md)。业务路由调用领域逻辑和 finance 客户端，交易内核由 finance 提供。
