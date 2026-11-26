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

    curl -fsS http://127.0.0.1:8090/healthz/live
    curl -i http://127.0.0.1:8090/healthz/ready
    curl -fsS http://127.0.0.1:8090/api/v1/system/status

## 故障定位与回滚

| 现象 | 诊断与恢复 |
| --- | --- |
| 数据库检查失败、live 200 / ready 503 | 核对 PostgreSQL 状态、连接地址、凭据和迁移权限。数据库恢复后观察同一 web PID 的 ready 回到 200 |
| 静态目录缺失 | 检查 WEB_STATIC_ROOT 和 index.html 的存在及权限，恢复完整版本目录，验证深链接与 /assets 缺失时的 404 |
| finance 不可达 | system/status 仍提供退化信息。检查 FINANCE_BASE_URL 与 finance 自身 ready。默认 web ready 为 degraded，如配置必须依赖 finance，则返回 503 |
| 数据库迁移失败 | 保存错误与迁移版本，核对备份；先修复失败原因再执行 migrate，不修改已应用脚本 |
| 回滚 | 确认数据库向后兼容，按 fertile 指针组合恢复上一套 web 二进制、配置和静态目录。必要时先在独立库演练恢复 |
| 本地接口正常、公网失败 | 由 Ruler 核对 https://fertile.viramoon.com 的 DNS、Tunnel 路由与外部可达性。web 源站使用 127.0.0.1:8090 |

按照请求的 X-Request-Id 查找标准错误的 JSON 日志，可看到状态、方法、匹配的路由模板与耗时。发行与回滚由 fertile 管理。

## finance 转发及恢复

用同一示例数据目录启动 finance API 与 web 后，对比两个端点的 JSON。金额与数量保持各自契约类型。

    curl -fsS http://127.0.0.1:8081/api/v1/backtests/sample | jq -S . > /tmp/finance-sample.json
    curl -fsS http://127.0.0.1:8090/api/v1/backtests/sample | jq -S . > /tmp/web-sample.json
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
      http://127.0.0.1:8090/api/v1/backtests
}
id=$(submit | jq -er .id)
curl -fsS "http://127.0.0.1:8090/api/v1/backtests/$id"
```

已启用写权限时，无有效管理员凭据的写请求返回 401。请根据 429 响应的 Retry-After 等待窗口释放。相同 key、不同参数会被 finance 以 409 拒绝。超时返回 504，先用原 key 查询，重试时沿用 key 和原始请求。请求日志可按 X-Request-Id 关联。

## 启用浏览器管理会话

在 HTTPS 入口的 web 进程环境设置 `WEB_PUBLIC_ORIGIN`，如 `https://fertile.viramoon.com`。Origin 包含协议、主机和非默认端口，省略尾部斜杠。保留 `WEB_ADMIN_TOKEN_FILE` 供会话登录和脚本鉴权。可用 `WEB_ADMIN_SESSION_TTL_SECONDS` 设置 1 至 900 秒的会话期限，默认 900 秒。

浏览器以 Bearer 登录并接收 HttpOnly Cookie。页面使用返回的 CSRF token 执行同源写入，可从能力接口恢复 CSRF token。退出立即撤销该会话。管理 Bearer 轮换或进程重启后，浏览器重新登录。W1 的会话状态保存在单个 web 进程中。

## 集成配置及凭据

启用凭据录入时，成对设置 `WEB_CREDENTIAL_DIR` 与 `WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE`，然后重启 web。前者是 web 运行身份所有的 0700 持久绝对目录，置于静态资源树之外，使用支持原子重命名和目录同步的本地文件系统。后者是受部署保护的 RSA 公钥 PEM 绝对路径，采用 `BEGIN PUBLIC KEY`，RSA 长度 2048 至 8192 位。公钥由 web 运行身份或 root 所有，组与其他用户不得写入。由获授权的接收方保管私钥，仅将对应公钥交给 web。

未配置密文存储时，有效的 `PUT /api/v1/integrations/{id}/credential` 录入请求返回 503。`DELETE /api/v1/integrations/{id}/credential` 只更新 PostgreSQL 中的吊销状态。管理员鉴权、请求体、`If-Match` 校验及数据库事务均成功时返回 200。

web 将新凭据封装为 JWE Compact，使用 `RSA-OAEP-256` 和 `A256GCM`。0600 密文文件命名为 `<凭据 UUID>.jwe`，受保护头含 `credential_id` 及 `kid`。后者是公钥 DER SPKI 的 SHA-256，经 base64url 编码且省略填充。数据库文件引用应与头中的 UUID 一致。消费者的凭据交付将在对应集成接入时配置，使用独立的运行身份或受控交付路径。

密文先写入持久目录，再提交 PostgreSQL 引用。数据库事务取消或提交结果不明后可能留下无引用密文。备份时保留配套的 PostgreSQL、密文目录及对应接收方私钥。轮换接收方公钥后，新录入的凭据使用新密钥，旧密文继续依赖其 `kid` 对应的私钥。清理时暂停凭据写入，与 PostgreSQL 中的全部引用、历史保留和备份核对后，再删除确认无引用的文件。接口吊销凭据会更新 PostgreSQL 中的可用状态，并保留历史密文供审计和备份。

W1 catalog 允许按标的创建草稿。连接测试记录失败或不可用，保留原活动版本。更新请求超时后先读配置 revision 和最近测试 ID。W1 不需要修改端口或新增守护进程。部署前执行 `web migrate`，再启动新 web 进程。需要回到 W1 前的二进制时，保留已迁移的数据库和密文，并设置 `WEB_AUTO_MIGRATE=false`。

## 历史回测与提交确认

回测页从 /api/v1/backtests/datasets 读取已审核的历史标的。配置数据后提交 SmaCrossover，并在工作台查看作业状态和历史结果。POST 超时或连接中断后，使用管理员身份和原 Idempotency-Key 请求 GET /api/v1/backtests/by-key。若查到 ID，继续轮询该作业。只有明确返回 404 后才允许带原请求及原键重试。
