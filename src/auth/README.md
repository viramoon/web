# 管理员会话与产品权限

[src/auth.rs](../auth.rs) 在启动时从指定的 0600 普通文件读取管理员 Bearer。配置 HTTPS origin 后，可用 Bearer 创建短期会话，Cookie 由浏览器持有，写请求校验 Origin 与 CSRF。会话和 Bearer 共享管理员写入滑动窗口，鉴权在读取请求体前执行。

读取权限以产品为单位，`/api/v1/me/capabilities` 和读取检查共用同一规则。接口和运维配置见[接口说明](../../docs/api.md)和[运行手册](../../RUNBOOK.md)。
