# HTTP 日志

标准错误输出结构化 JSON。请求完成日志的 span 包含 request_id、method、path，事件字段包含 status、latency_ms。客户端可从响应的 X-Request-Id 关联单次请求。默认日志级别 info，可用 RUST_LOG 调整。服务健康与依赖延迟见[接口说明](../../docs/api.md)，外部告警与日志保留策略由 fertile 运维管理。
