# web 运行手册

在 finance 与数据库所在的盒子准备好工具链、运行用户、已初始化的 PostgreSQL、可写的 web 数据库和 frontend 构建产物。数据库连接串由运维以受控文件或进程环境提供，勿将密码写入仓库或命令历史。

## 构建、迁移与启动

从 fertile 仓库根目录，以持有数据目录的普通用户启动 PostgreSQL。首次在独立开发实例上创建 web 数据库。托管环境沿用运维提供的数据库和 DATABASE_URL。

    source /home/box/fertile/env.sh
    scripts/postgres.sh init
    scripts/postgres.sh start
    export PGHOST="$FERTILE_ROOT/run/postgres" PGPORT="${FERTILE_POSTGRES_PORT:-5432}"
    createdb web  # 仅在开发库首次创建时执行
    export DATABASE_URL="postgresql://localhost/web?host=$PGHOST&port=$PGPORT"

从 web 仓库根目录构建和运行，静态目录须含 frontend/dist 的 index.html 与 assets。

    source /home/box/fertile/env.sh
    SQLX_OFFLINE=true cargo build --locked --release
    export FERTILE_PROFILE=cloudbox
    export WEB_STATIC_ROOT=/home/box/fertile/static/current
    export FINANCE_BASE_URL=http://127.0.0.1:8081
    target/release/web migrate
    target/release/web serve

migrate 是显式的部署步骤，连接失败会以错误退出。serve 即使数据库暂时不可达也会先建立监听，后台重连；默认执行兼容迁移后才将数据库标为 ready。完成独立迁移后可设 WEB_AUTO_MIGRATE=false。向 web 进程发送 SIGTERM 或 SIGINT 时，进程停止接纳新连接并等待在途请求，默认宽限 20 秒。完整部署与守护定义见 [fertile/deploy](https://github.com/viramoon/fertile/tree/main/deploy)。

    curl -fsS http://127.0.0.1:8080/healthz/live
    curl -i http://127.0.0.1:8080/healthz/ready
    curl -fsS http://127.0.0.1:8080/api/v1/system/status

## 故障定位与回滚

| 现象 | 诊断与恢复 |
| --- | --- |
| 数据库检查失败、live 200 / ready 503 | 核对 PostgreSQL 状态、连接地址、凭据和迁移权限。数据库恢复后观察同一 web PID 的 ready 回到 200 |
| 静态目录缺失 | 检查 WEB_STATIC_ROOT 和 index.html 的存在及权限，恢复完整版本目录，验证深链接与 /assets 缺失时的 404 |
| finance 不可达 | system/status 仍提供退化信息。检查 FINANCE_BASE_URL 与 finance 自身 ready。默认 web ready 为 degraded，如配置必须依赖 finance，则返回 503 |
| 数据库迁移失败 | 保存错误与迁移版本，核对备份；先修复失败原因再执行 migrate，不修改已应用脚本 |
| 回滚 | 确认数据库向后兼容，按 fertile 指针组合恢复上一套 web 二进制、配置和静态目录。必要时先在独立库演练恢复 |
| 本地接口正常、公网失败 | 核对现有 Tunnel 与外部访问。候选公网域名 www.viramoon.com、www.viramoonstar.com，待所有者确认，复现时用已确认的域名 |

按照请求的 X-Request-Id 查找标准错误的 JSON 日志，可看到状态、方法、路径与耗时。发行与回滚由 fertile 管理。

## finance 转发及恢复

用同一示例数据目录启动 finance API 与 web 后，对比两个端点的 JSON。金额与数量保持各自契约类型。

    curl -fsS http://127.0.0.1:8081/api/v1/backtests/sample | jq -S . > /tmp/finance-sample.json
    curl -fsS http://127.0.0.1:8080/api/v1/backtests/sample | jq -S . > /tmp/web-sample.json
    diff -u /tmp/finance-sample.json /tmp/web-sample.json

网页出现 502、503、504 时，先查 /api/v1/system/status 中的 finance.ok、latency_ms、circuit，再查 finance 的监听和 ready。熔断打开后等待对应 profile 的 circuit_open_ms，下一次访问会试探，成功后自动恢复。若请求为写操作，超时后先查作业状态，重交时沿用同一幂等键。

finance 的接口变动须使用同一锁定提交重新生成客户端；脚本与输入说明见[生成客户端说明](src/finance_client/README.md)。

## 管理员写权限

web 启动时读取 WEB_ADMIN_TOKEN_FILE 指定的文件，默认 /home/box/fertile/secrets/web.env。缺少文件时，匿名读取照常可用，POST 返回 403。新建或轮换时，以运行 web 的账号在受控终端执行。生成的新值直接写入同目录临时文件，原子替换后按 fertile 的进程管理流程重启 web。

```bash
set -e
umask 077
secret=/home/box/fertile/secrets/web.env
install -d -m 700 "$(dirname "$secret")"
candidate=$(mktemp "$(dirname "$secret")/.web.env.XXXXXX")
trap 'rm -f -- "$candidate"' EXIT
{ printf 'WEB_ADMIN_TOKEN='; openssl rand -hex 32; } > "$candidate"
chmod 600 "$candidate"
mv -- "$candidate" "$secret"
```

文件只包含一个 WEB_ADMIN_TOKEN 赋值，需为普通文件且权限恰为 0600。接口鉴权在解析请求前执行。管理员可在盒子上从文件读取临时 curl 配置，避免把 token 放入命令行参数。用相同的 Idempotency-Key 处理同一次提交的后续重试，从返回体取 id 查询状态。

```bash
secret=${WEB_ADMIN_TOKEN_FILE:-/home/box/fertile/secrets/web.env}
key=$(cat /proc/sys/kernel/random/uuid)
submit() {
  sed -n 's/^WEB_ADMIN_TOKEN=\(.*\)$/header = "Authorization: Bearer \1"/p' "$secret" |
    curl -sS --config - -H "Idempotency-Key: $key" -H 'Content-Type: application/json' \
      --data '{"strategy":"DemoStrategy","instrument":"6EH4.XCME","start":"2024-01-01T23:01:00Z","end":"2024-01-01T23:09:00Z","params":{}}' \
      http://127.0.0.1:8080/api/v1/backtests
}
id=$(submit | jq -er .id)
curl -fsS "http://127.0.0.1:8080/api/v1/backtests/$id"
```

已启用写权限时，无效或缺少 Bearer token 返回 401。请根据 429 响应的 Retry-After 等待窗口释放。相同 key、不同参数会被 finance 以 409 拒绝。超时返回 504，重新提交要保留 key 和原始请求。请求日志可按 X-Request-Id 关联，凭据只在读取 secret 和验证 Authorization 时使用。
