# 配置与覆盖

从仓库根目录运行 web，或通过 WEB_CONFIG_DIR 指定配置目录。启动按 base.yaml → cloudbox.yaml / stable.yaml → 已支持的环境变量加载。FERTILE_PROFILE 默认为 cloudbox，两个 profile 都独立于交易账户和运行模式。未知的 YAML 字段、相对静态根路径、非法 IP、端口或 URL 会导致启动失败。DATABASE_URL 由部署环境注入，包含密码时使用仅服务用户可读的 secret 文件。

| 环境变量 | YAML 字段 | 默认值 |
| --- | --- | --- |
| WEB_BIND_HOST | server.bind_host | 127.0.0.1 |
| WEB_PORT | server.port | 8090 |
| WEB_STATIC_ROOT | paths.static_root | /home/box/fertile/static/current |
| DATABASE_URL | database.url | postgres://localhost/web |
| WEB_AUTO_MIGRATE | database.auto_migrate | true |
| FINANCE_BASE_URL | finance.base_url | http://127.0.0.1:8081 |
| WEB_REQUIRE_FINANCE | finance.required_for_ready | false |
| WEB_ADMIN_TOKEN_FILE | security.admin_token_file | /home/box/fertile/secrets/web.env |
| WEB_PUBLIC_ORIGIN | security.public_origin | 空，浏览器管理会话限同源 HTTP 回环地址 |
| WEB_ADMIN_SESSION_TTL_SECONDS | security.session_ttl_seconds | 900 秒，允许 1 至 900 |
| WEB_CREDENTIAL_DIR | credentials.directory | 空，与公钥路径成对配置 |
| WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE | credentials.recipient_public_key_file | 空，与密文目录成对配置 |
| WEB_BINANCE_PROBES_ENABLED | binance_probes_enabled | false |
| WEB_BACKUP_STATUS_FILE | ops.backup_status_file | 空 |
| WEB_BACKUP_RESTORE_FILE | ops.backup_restore_file | 空，需同时配置状态文件 |
| WEB_QUALITY_STATUS_FILE | ops.quality_status_file | 空 |

YAML 还配置请求时限 server.request_timeout_ms（默认 15000）、请求体上限 server.max_body_bytes（1 MiB）、退出宽限 server.shutdown_timeout_ms（20000）。数据库连接与连接池获取时限均为 2000ms；finance 就绪探测时限 2000ms。可在 profile 文件仅写需要覆盖的键。

finance 下的连接与单次请求时限分别为 connect_timeout_ms、request_timeout_ms；cloudbox 均为 3000ms，stable 分别为 1000ms、2000ms。max_retries 为 0 到 2（默认 2），仅幂等读取重试；backoff_ms 为第一轮退避上限，默认 cloudbox 150ms、stable 100ms。circuit_failure_threshold 控制连续失败次数（cloudbox 3、stable 5），circuit_open_ms 为冷却时间（10000ms、5000ms）。

web 自行读取这个 0600 普通文件中的 WEB_ADMIN_TOKEN。文件缺失将关闭写权限。唯一管理员在 security.window_ms（默认 60000ms）滑动窗口内共享 security.max_requests（默认 10）次已认证写请求额度。重启后重新加载 token，并清空会话和限流状态。

配置 WEB_PUBLIC_ORIGIN 时，其值须与浏览器的 HTTPS Origin 完全相同，例如 `https://fertile.viramoon.com`，省略尾部斜杠和默认端口。未配置时，接受与请求 Host 精确匹配的 `http://127.0.0.1:<port>` 和 `http://localhost:<port>`。凭据目录与公钥须使用绝对路径。目录和 PEM 的权限以及备份要求见[运行手册](../RUNBOOK.md)。

`web-notifier` 从独立进程环境读取 SMTP 和投递策略，与 web 使用同一 `DATABASE_URL`。配置键、默认值和 SMTP 认证文件要求见[告警与邮件进程](../RUNBOOK.md#告警与邮件进程)。

策略、研究和 Drive 配置位于 `local_probes`，默认未配置。环境覆盖、工具和槽位说明见[本机探测配置](../docs/local-integration-probes.md)。
