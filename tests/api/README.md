# API 集成测试

`cargo test --locked` 运行 Rust 单元测试与 PostgreSQL 集成测试。以普通用户运行，将用于 `initdb`、`pg_ctl`、`psql` 的 PostgreSQL bin 目录加入 PATH。数据库测试启动独占的临时 Unix socket 实例，退出时停库，覆盖空库和已初始化 schema、断开时的 live / ready、同一 web 进程恢复就绪，以及集成配置启用、失败回退、并发、审计与脱敏。

短期会话、Cookie、Origin/CSRF、鉴权顺序及日志脱敏可以离线运行 `cargo test --locked --lib w1_http_tests`。凭据封装测试使用临时生成的 RSA 密钥，可通过 `cargo test --locked --lib credentials::tests` 单独运行。

先 `cargo build --locked`，设置 WEB_BIN 为 web 可执行文件的绝对路径，DATABASE_URL 指向专用的可迁移测试数据库，然后从仓库根目录运行 `python3 tests/api/core_http.py`。脚本使用独立静态目录和可控的 finance HTTP 服务，覆盖状态与退化、请求 ID、深链接、缓存、错误路由及 SIGTERM 在途请求完成。测试不依赖真实交易账户。
