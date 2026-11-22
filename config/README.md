# 配置与覆盖

从仓库根目录运行 web，或通过 WEB_CONFIG_DIR 指定配置目录。启动按 base.yaml → cloudbox.yaml / stable.yaml → 已支持的环境变量加载。FERTILE_PROFILE 默认为 cloudbox，两个 profile 都独立于交易账户和运行模式。未知的 YAML 字段、相对静态根路径、非法 IP、端口或 URL 会导致启动失败。DATABASE_URL 由部署环境注入，包含密码时使用仅服务用户可读的 secret 文件。

| 环境变量 | YAML 字段 | 默认值 |
| --- | --- | --- |
| WEB_BIND_HOST | server.bind_host | 127.0.0.1 |
| WEB_PORT | server.port | 8080 |
| WEB_STATIC_ROOT | paths.static_root | /home/box/fertile/static/current |
| DATABASE_URL | database.url | postgres://localhost/web |
| WEB_AUTO_MIGRATE | database.auto_migrate | true |
| FINANCE_BASE_URL | finance.base_url | http://127.0.0.1:8081 |
| WEB_REQUIRE_FINANCE | finance.required_for_ready | false |

YAML 还配置请求时限 server.request_timeout_ms（默认 15000）、请求体上限 server.max_body_bytes（1 MiB）、退出宽限 server.shutdown_timeout_ms（20000）。数据库连接与连接池获取时限均为 2000ms；finance 就绪探测时限 2000ms。可在 profile 文件仅写需要覆盖的键。
