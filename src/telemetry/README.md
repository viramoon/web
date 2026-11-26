# HTTP 日志

标准错误输出结构化 JSON。请求完成日志的 span 包含 request_id、method 和匹配的路由模板 path，未匹配的请求记为 `<unmatched>`。事件字段包含 status、latency_ms。请求正文、查询参数及 Authorization、Cookie 和 CSRF 请求头均不记录。客户端可从响应的 X-Request-Id 关联单次请求。默认日志级别 info，可用 RUST_LOG 调整。服务健康与依赖延迟见[接口说明](../../docs/api.md)，外部告警与日志保留策略由 fertile 运维管理。

[日志初始化](../telemetry.rs)用 web 事件目标过滤第三方库输出，自定义 HTTP 方法记为 OTHER。启动错误记录固定阶段，panic 只记录固定事件。新增事件使用静态说明、有限枚举或必要计数。更多字段与样本规则见[数据防护](../../docs/data-guard.md)。
