# API 集成测试

`cargo test --locked` 运行 Rust 单元测试与 PostgreSQL 停启测试。以普通用户运行，将用于 `initdb`、`pg_ctl`、`psql` 的 PostgreSQL bin 目录加入 PATH。数据库测试启动独占的临时实例，验证迁移、断开时的 live / ready，以及同一 web 进程在数据库恢复后自动就绪。

先 `cargo build --locked`，设置 WEB_BIN 为 web 可执行文件的绝对路径，DATABASE_URL 指向专用的可迁移测试数据库，然后从仓库根目录运行 `python3 tests/api/core_http.py`。脚本使用独立静态目录和可控的 finance HTTP 服务，覆盖状态与退化、请求 ID、深链接、缓存、错误路由及 SIGTERM 在途请求完成。测试不依赖真实交易账户。
