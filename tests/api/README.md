# API 集成测试

`cargo test --locked` 运行 Rust 单元测试与 PostgreSQL 集成测试。以普通用户运行，将用于 `initdb`、`pg_ctl`、`psql` 的 PostgreSQL bin 目录加入 PATH。数据库测试启动独占的临时 Unix socket 实例，退出时停库，覆盖空库和已初始化 schema、断开时的 live / ready、同一 web 进程恢复就绪，以及集成配置启用、失败回退、并发、审计与脱敏。

短期会话、Cookie、Origin/CSRF、鉴权顺序及日志脱敏可以离线运行 `cargo test --locked --lib w1_http_tests`。凭据封装测试使用临时生成的 RSA 密钥，可通过 `cargo test --locked --lib credentials::tests` 单独运行。

从仓库根目录构建并运行三个本地 HTTP 验证脚本。WEB_BIN 指向本次构建产物，PATH 包含 PostgreSQL 工具。

    cargo build --locked --bin web
    export WEB_BIN="$PWD/target/debug/web"
    python3 -B tests/api/core_http.py
    python3 -B tests/api/finance_resilience.py
    python3 -B tests/api/finance_writes.py
    python3 -B -m unittest discover -s tests/api -p test_http_isolation.py -v

三个脚本使用随机回环端口上的合成 finance 桩、临时静态目录和显式构造的 web 子进程环境。测试客户端直连回环地址，禁用系统代理和 HTTP 重定向。状态与优雅退出用例自行初始化仅监听临时 Unix socket 的 PostgreSQL，迁移后验证状态与退化、请求 ID、深链接、缓存、错误路由及 SIGTERM 在途请求完成，退出时停止实例。另外两个脚本用临时目录内不存在的 Unix socket 路径测试无数据库场景。写权限脚本只使用临时生成的令牌文件，其他 HTTP 脚本将令牌文件指向临时目录内的缺失路径。运行这些脚本只需提供构建产物和工具路径，DATABASE_URL、代理和部署配置都由脚本隔离。
