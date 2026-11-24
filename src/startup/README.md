# 启动与关闭

[src/main.rs](../main.rs) 实现 `web serve`、`web migrate`、`web export-openapi`。配置无效时启动失败。serve 先监听，再在后台检查数据库；部署时可用 migrate 提前执行迁移。SIGTERM / SIGINT 触发停止接受新请求、等待在途响应以及释放连接池，共享配置的关闭时限。详细命令见[运行手册](../../RUNBOOK.md)。
