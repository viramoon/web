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

migrate 是显式的部署步骤，连接失败会以错误退出。serve 即使数据库暂时不可达也会先建立监听，后台重连；默认执行兼容迁移后才将数据库标为 ready。完成同一发布的显式迁移后可设 WEB_AUTO_MIGRATE=false。向 web 进程发送 SIGTERM 或 SIGINT 时，进程停止接纳新连接并等待在途请求，默认宽限 20 秒。完整部署与守护定义见 [fertile/deploy](https://github.com/viramoon/fertile/tree/main/deploy)。

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
| 含新增迁移的发布回退 | 将迁移前备份恢复到独立库和数据根，再按下节启动旧发布 |
| 本地接口正常、公网失败 | 由 Ruler 核对 https://fertile.viramoon.com 的 DNS、Tunnel 路由与外部可达性。web 源站使用 127.0.0.1:8090 |

按照请求的 X-Request-Id 查找标准错误的 JSON 日志，可看到状态、方法、匹配的路由模板与耗时。发行与回滚由 fertile 管理。

非标准方法统一记为 OTHER，未匹配路由记为 <unmatched>。启动或显式迁移失败时按日志中的 stage 检查相应配置、文件权限或服务状态。panic 只记固定事件，HTTP 请求内的 panic 返回通用 500。提高 RUST_LOG 级别可查看 web 自身的诊断事件，第三方追踪仍被过滤。日志和故障资料留在获准的私有目录。字段说明见[日志、样本与 CI 防护](docs/data-guard.md)。

### 含新增迁移的发布回退

旧代码绝不在新发布写过的数据库上运行。凡含新增迁移的发布，在首次运行迁移前保留已验证可恢复的备份和上一套发布制品。回退时，从同一迁移前快照恢复独立数据库 `web_rollback_<sha>` 与独立数据根 `data-paper-rollback-<sha>`，然后在这套目标上运行旧发布。`<sha>` 取自要恢复的旧 fertile 发布提交，两处使用同一标识。

#### 先确认目标实例

暂停接入或停止服务前，从部署记录核对目标实例的绝对持久根、PostgreSQL 端口和进程管理器。在 fertile 源码目录的同一个 Bash 会话中设置经核对的值，并在下列检查成功后继续。后续命令沿用这个会话的实例配置。

```bash
[[ "${FERTILE_ROOT:?先设置目标实例的绝对持久根}" == /* && -d "$FERTILE_ROOT" &&
   "${FERTILE_POSTGRES_PORT:?先设置目标实例的 PostgreSQL 端口}" =~ ^[1-9][0-9]{3,4}$ &&
   "$FERTILE_POSTGRES_PORT" -gt 1023 && "$FERTILE_POSTGRES_PORT" -lt 65536 ]] &&
  export FERTILE_ROOT FERTILE_POSTGRES_PORT
```

使用本机受管 PostgreSQL 时，核对恢复连接的 `PGHOST` 为 `$FERTILE_ROOT/run/postgres`，`PGPORT` 为已确认的 `FERTILE_POSTGRES_PORT`。使用外部 PostgreSQL 时，由数据库管理员确认恢复连接及其权限。

暂停接入和任务领取，等待在途作业结束。保全新版数据库、数据根和发布指针，记录 notifier 原启用状态。核对备份清单的截点早于首次迁移，并与选定的旧发布匹配。留存备份后到切换前的写入与投递记录，供恢复后对账。

#### systemd 实例

先核对 `fertile.target` 的单元绑定到已确认的实例根，并确认旧发布支持 systemd。管理员按[备份手册的恢复步骤](https://github.com/viramoon/fertile/blob/main/ops/backup.md#恢复步骤)执行 `sudo systemctl stop fertile.target` 停栈，单独启动该实例的 PostgreSQL，完成预检与正式恢复。两次恢复命令都显式指向本节规定的新建 `web_rollback_<sha>` 与 `data-paper-rollback-<sha>`。核对恢复库的 `_sqlx_migrations` 与旧发布相符后，停止单独启动的 PostgreSQL。

在目标实例的 systemd 运行配置中指定恢复库和数据根，将独立研究目录指向恢复根下的对应目录，并设置 `FERTILE_ENABLE_WEB_NOTIFIER=0`。按[普通服务器部署指南](https://github.com/viramoon/fertile/blob/main/deploy/linux-server.md#systemd)选定旧发布并重新安装 unit，确认旧版 unit 使用这套配置后，再由 systemd 启动。启动后核对旧版实际使用的库和数据根，确认 notifier 未运行并完成[队列对账](#告警与邮件进程)，再按原启用状态恢复投递。

#### supervisord 实例

1. 从 fertile 仓库根目录执行 `scripts/fertile down` 停止整栈，确认 supervisord 已退出。随后执行 `scripts/postgres.sh start` 单独启动 PostgreSQL。等待数据库可用后再执行预检和正式恢复，期间保持业务进程停止。正式恢复会检查 supervisord 是否仍在运行。
2. 在 fertile 仓库根目录，以受控的 PostgreSQL 恢复角色运行下例。预先设置旧发布提交 `old_sha` 和经核对的迁移前快照绝对路径 `pre_migration_snapshot`。选择尚未建立的目标库和数据根。预检或恢复失败时停止切换，核对目标占用、归档和访问权限。

   ```bash
   (
     set -e
     : "${FERTILE_ROOT:?先设置受控实例根}"
     : "${pre_migration_snapshot:?先选择迁移前快照}"
     export FERTILE_ROOT
     [[ ${old_sha:-} =~ ^[0-9a-f]{7,40}$ ]]
     test -f "$pre_migration_snapshot/postgres.dump"
     rollback_data="$FERTILE_ROOT/data-paper-rollback-$old_sha"
     rollback_db="web_rollback_$old_sha"
     scripts/restore.sh "$pre_migration_snapshot" --dry-run \
       --target-data-root "$rollback_data" --target-database "$rollback_db"
     scripts/restore.sh "$pre_migration_snapshot" \
       --target-data-root "$rollback_data" --target-database "$rollback_db"
   )
   ```

3. 核对恢复库的 `_sqlx_migrations` 与旧发布相符，并核对旧版所需的数据库角色、权限、业务状态及文件引用。执行 `scripts/postgres.sh stop` 停止单独启动的 PostgreSQL，交由 supervisor 随旧发布启动。按 Ruler 手册准备 `old_env`。为旧发布设置恢复后的 `FERTILE_DATA_ROOT`，将任何独立研究目录配置指向恢复根下的对应目录，并将 `web` 和 `web-notifier` 的 `DATABASE_URL` 指向同一个恢复库。使用 fertile 本机数据库配置时，核对 `FERTILE_WEB_DATABASE` 指向该库。运行 `"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=0 scripts/fertile rollback --switch-only "$old_sha"` 切换旧版二进制、配置和静态产物。
4. 运行 `"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=0 scripts/fertile ensure-up` 启动旧发布。对账完成前，每次 `ensure-up` 都沿用同一组恢复目标及 notifier 禁用设置。用 `scripts/fertile status --json` 确认 notifier 未运行。核实旧版进程实际使用的数据库及数据根，再检查 `/healthz/ready`、旧版页面和备份截点前的业务记录，完成[队列对账](#告警与邮件进程)。
5. 对账和验证通过后，若原部署已启用 notifier 且旧发布包含该进程，运行 `"${old_env[@]}" FERTILE_ENABLE_WEB_NOTIFIER=1 scripts/fertile ensure-up` 恢复邮件投递，并核对 notifier 已连接恢复库。其余情况下保持 notifier 禁用。恢复业务接入。新版数据库和数据根单独保留用于对账以及切回新版。

完整的快照恢复与发布切换由 fertile 管理。[备份与恢复](https://github.com/viramoon/fertile/blob/main/ops/backup.md) · [Ruler 回滚手册](https://github.com/viramoon/fertile/blob/main/ops/ruler-runbook.md#从升级前快照回滚并切回新版)

## 安装仓库数据防护

每个克隆在首次提交或推送前安装钩子。需本地 Git 和 Python 3，无额外 Python 包。

    sh scripts/install-data-guard-hooks.sh
    python3 -B scripts/data_guard.py --show-paths staged
    python3 -B scripts/data_guard.py --show-paths range origin/main HEAD
    python3 -B -m unittest discover -s tests -p test_data_guard.py -v

已有同名钩子或 core.hooksPath 时，保留原有配置，按[钩子接入说明](docs/data-guard.md#检查入口)把检查接入现有钩子。违规文件移到工作树外，并从待推送历史清除相应对象后再推送。CI 仅显示检查名和状态，遇到失败时用同一命令在隔离环境复测。生成器临时输出、数据库与测试日志都放在工作树外。

遇到 `fixture-source-required` 时，合成样本可按[来源登记说明](docs/data-guard.md#合成来源)补全生成器与摘要。Cargo 报告和运行日志留在工作树外，代码块与改名文件也会被检查。

待审的规则放宽须进入对应入口的受信版本后才生效。遇到 `missing-remote-default` 或 `missing-trusted-rules` 时，补齐目标远端的默认分支跟踪 ref 和完整提交历史再检查。 [各入口的规则来源](docs/data-guard.md#检查入口)

本地 HTTP 验证使用自身的临时配置、数据库 socket 和 finance 桩。运行命令及 PostgreSQL 工具要求见[API 集成测试](tests/api/README.md)。

## finance 转发及恢复

用同一示例数据目录启动 finance API 与 web 后，对比两个端点的 JSON。金额与数量保持各自契约类型。

    curl -fsS http://127.0.0.1:8081/api/v1/backtests/sample | jq -S . > /tmp/finance-sample.json
    curl -fsS http://127.0.0.1:8090/api/v1/backtests/sample | jq -S . > /tmp/web-sample.json
    diff -u /tmp/finance-sample.json /tmp/web-sample.json

网页出现 502、503、504 时，先查 /api/v1/system/status 中的 finance.ok、latency_ms、circuit，再查 finance 的监听和 ready。熔断打开后等待对应 profile 的 circuit_open_ms，下一次访问会试探，成功后自动恢复。若请求为写操作，超时后先查作业状态，重交时沿用同一幂等键。

finance 契约升级时，从同一锁定提交导出 HTTP 和读流快照并生成客户端。W9 使用 F10d 的期权流读取，W16 使用 F16 的推送和恢复快照，W2b 的公开回测分页依赖 F14。回测继续通过现有 HTTP 接口提交和查询，F13 的回测 worker 由 finance 部署管理。[生成客户端说明](src/finance_client/README.md) · [契约版本](docs/api.md#finance-契约版本)

策略版本、因子快照和幂等队列由 finance 管理。W2b 复用 web 现有配置、数据库、监听端口和进程。

`FINANCE_BASE_URL` 指向数字回环地址（如 `127.0.0.1` 或 `[::1]`）时，web 直接连接 finance，忽略环境代理。

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

本地运行时可省略 `WEB_PUBLIC_ORIGIN`，使用 `http://127.0.0.1:8090` 或 `http://localhost:8090` 打开终端，并沿用所选主机与端口登录、调用 REST 和接收 SSE、WebSocket。请使用支持回环 HTTP 上 Secure Cookie 的浏览器，入口代理应保留原始 `Host` 和 `Origin`。写入仍需同源 `Origin` 与 `X-CSRF-Token`。同源 REST GET 和 EventSource 在省略 `Origin` 时依靠浏览器的 `Sec-Fetch-Site: same-origin`。

浏览器以 Bearer 登录并接收 HttpOnly Cookie。页面使用返回的 CSRF token 执行同源写入，可从能力接口恢复 CSRF token。退出立即撤销该会话。管理 Bearer 轮换或进程重启后，浏览器重新登录。W1 的会话状态保存在单个 web 进程中。

## 行情网关与运维事件

W14 引入的 finance 提交 `7b874d7` 包含 F14 公开回测筛选与 FCI2 的 Python 投影读取修复。联调使用当前记录的 finance 版本，沿用 `FINANCE_BASE_URL` 和 IBKR paper 只读配置。[finance 契约来源](openapi/finance.commit) · [兼容契约与升级顺序](docs/api.md#finance-契约版本)

升级前按[迁移与回退流程](#含新增迁移的发布回退)保留快照，再执行 `web migrate` 或启用默认自动迁移。迁移 `20261127040000` 创建自选表，迁移后的备份应包含该表。只使用受控的 `FINANCE_BASE_URL` 及管理员会话或 bearer。浏览器通过已配置的 HTTPS 入口或本地回环地址同源访问。WAL 和投影事件正文须同时具备 finance 本机记录与显示许可。保留部署侧的 IBKR paper 只读配置，行情网关读取既有 finance 契约。

`/api/v1/events` 使用现有 web 端口。入口代理应关闭 SSE 响应缓冲并允许 15 秒心跳，转发 `Last-Event-ID`。收到 `reset` 时从受保护的 REST 路径重取快照，断线后 EventSource 会携带最近游标重连。进程重启清空 SSE 回放，进程退出时主动关闭流。无效或过期的会话需重新登录。避免采集流正文、查询参数、身份头和付费数据。

`GET /api/v1/market/subscriptions` 返回单管理员共享的 web 订阅额度；券商实际预算和观察覆盖分别读取 `/api/v1/market/subscriptions/budget` 与 `/api/v1/market/subscriptions/coverage`。focus 是进程内的盘口选择，重启后重新设置。读取盘口时显式标的优先于 focus。更新自选前读取当前 revision，写超时后先查自选或 focus 再重试。如果盘口响应为 `snapshot_required`，客户端应清空旧盘口。

`/api/v1/stream/market` 在既有 web 端口升级。入口代理应保留 WebSocket 升级头与浏览器 Origin。握手返回 `503 finance_unavailable` 时先恢复受控的 finance API，再重连。收到重置时清空旧盘口，再以有效快照或游标建立新的订阅。各路径、节流和接续语义见[行情网关接口](docs/api.md#行情自选与推送)。

## W8 期权和场所深度

先部署含 F10c 的 finance，再切换 web。使用现有 `FINANCE_BASE_URL`，由 finance 的已授权 IBKR paper 只读行情所有者接入链和调度器、Nasdaq 深度回调、敞口输入快照及已取得的历史链批次。单独启动 finance API 时，这些来源可能未装配。若新读路径返回 `503 finance_unavailable`，除连通性和熔断外，还应检查该来源是否已接入且具有展示许可。上游已接入但不存在相应快照或历史窗口时，保留 404。

期权链使用已确认的券商 `underlying_con_id`，仅带旧参数的合法请求仍返回 `503 finance_contract_pending`。敞口使用 F1 `underlying_id`。场所深度请读 `/api/v1/market/depth?symbol=…`，仅当状态为 `receiving` 时展示盘口。历史链查询使用已取得的精确窗口。上述 GET 仅查已接入的数据与状态，供应商会话及采集任务由行情所有者管理。[路径和参数](docs/api.md#w8-期权场所深度与券商线路)

## W9 期权流

先切换至包含 F10d 的 finance，再切换 web。W9 沿用 `FINANCE_BASE_URL`、现有进程和端口，无需迁移、环境变量或数据目录。共享行情所有者需以 IBKR paper 只读模式分别装配 OPRA 观察快照和 OPRA 历史核验报告。独立 finance API 未装配数据源时返回 503。在已装配的实例上，先用管理员会话按合约 `con_id` 与已知窗口查询两条读取路径。404 时核对上游库存窗口和产品展示许可。核验中的局部覆盖和无法回补原因可能随成功响应返回。GET 仅查看已有结果，保持原有订阅和采集流程。[请求与错误语义](docs/api.md#w9-期权流观察与历史核验)

## W16 期权流推送

先部署含 F16 的 finance main，再升级 web 和 frontend。沿用 `FINANCE_BASE_URL`、web 进程及端口，新路径无需新增环境变量、迁移或数据目录。需展示观察时，由已授权的 IBKR paper 只读行情所有者向 finance 发布观察。独立 finance API 未装配发布器时会发送 `disabled` 状态并保持空闲，恢复快照返回 503。

入口代理转发 WebSocket 升级头、同源 `Origin` 和 SSE 的 `Last-Event-ID`。SSE 关闭响应缓冲，空闲超时应容纳 finance 的 15 秒保活。请求、流及代理日志只记录必要的路由和状态元数据，勿采集行情正文、游标、查询参数和身份头。

客户端断线时以当前有效会话或 bearer 重连，并携带最后一次应用的事件游标。若鉴权失败，更新会话后再试。遇到 429 时按 `Retry-After` 或在已有流关闭后重试。收到 `reset_required` 时主动关闭旧连接，按当前合约与窗口读取恢复快照，替换本地视图，再用快照游标建立新连接。finance 重启会使旧的续传游标失效。[W16 路径与恢复流程](docs/api.md#w16-期权流推送与续传)

## 集成配置及凭据

启用凭据录入时，成对设置 `WEB_CREDENTIAL_DIR` 与 `WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE`，然后重启 web。前者是 web 运行身份所有的 0700 持久绝对目录，置于静态资源树之外，使用支持原子重命名和目录同步的本地文件系统。后者是受部署保护的 RSA 公钥 PEM 绝对路径，采用 `BEGIN PUBLIC KEY`，RSA 长度 2048 至 8192 位。公钥由 web 运行身份或 root 所有，组与其他用户不得写入。由获授权的接收方保管私钥，仅将对应公钥交给 web。

未配置密文存储时，有效的 `PUT /api/v1/integrations/{id}/credential` 录入请求返回 503。`DELETE /api/v1/integrations/{id}/credential` 只更新 PostgreSQL 中的吊销状态。管理员鉴权、请求体、`If-Match` 校验及数据库事务均成功时返回 200。

web 将新凭据封装为 JWE Compact，使用 `RSA-OAEP-256` 和 `A256GCM`。0600 密文文件命名为 `<凭据 UUID>.jwe`，受保护头含 `credential_id` 及 `kid`。后者是公钥 DER SPKI 的 SHA-256，经 base64url 编码且省略填充。数据库文件引用应与头中的 UUID 一致。消费者的凭据交付将在对应集成接入时配置，使用独立的运行身份或受控交付路径。

密文先写入持久目录，再提交 PostgreSQL 引用。数据库事务取消或提交结果不明后可能留下无引用密文。备份时保留配套的 PostgreSQL、密文目录及对应接收方私钥。轮换接收方公钥后，新录入的凭据使用新密钥，旧密文继续依赖其 `kid` 对应的私钥。清理时暂停凭据写入，与 PostgreSQL 中的全部引用、历史保留和备份核对后，再删除确认无引用的文件。接口吊销凭据会更新 PostgreSQL 中的可用状态，并保留历史密文供审计和备份。

管理员创建草稿并发起版本化测试，通过后显式启用。失败、不可用或候选变化时保留活动版本。超时先读配置 revision、活动版本和最近测试 ID。进程中断留下未结算的本机测试时，可从最新 revision 发起一次新测试，使原测试失效。回滚复用曾启用且凭据仍有效的配置，生成更高版本。

部署先留存迁移前备份，再执行 `web migrate` 并同步更新 web 与 web-notifier。文件和 Drive 预检的部署配置槽由运维管理。SMTP 配置槽用于创建候选快照，正在投递或保留用于配置版本回滚的 SMTP 版本继续使用其绑定的快照。发布回退按[独立库和数据根流程](#含新增迁移的发布回退)执行。核对恢复库仍引用的 SMTP 快照和密钥，以及新版待处理的 W4 SMTP 候选，按下文完成对账后再启动旧 notifier。

| 集成 | 部署侧设置及测试行为 |
| --- | --- |
| IBKR paper 和加密源 | 本机 finance 行情所有者通过数字回环 `FINANCE_BASE_URL` 提供只读观察。真实探测和活动配置应用依赖 F10 后续批次。当前锁定版本的探测结果为 `adapter_unavailable`。Binance Spot 和 USDⓈ-M 仅在部署显式设置 `WEB_BINANCE_PROBES_ENABLED=true` 后发请求。IBKR 由 vira 按券商流程完成 paper 登录与 2FA。见[本机行情契约](docs/market-probes.md) |
| 策略文件、研究文件 | 由部署预置文件并绑定 finance 工具。策略返回编译计划摘要，研究文件在一次性本机目录预演导入。见[配置及文件格式](docs/local-integration-probes.md) |
| SMTP | `primary` 读取 notifier 的 `WEB_SMTP_*`，`secondary` 读取 `WEB_SMTP_SECONDARY_*`。首个候选尝试保存并绑定加密配置快照。候选重试沿用原快照，启用和回滚绑定对应版本的快照。普通投递每次领取时选用当前活动快照。候选失败只结算其测试证据。启用前沿用 W3 默认主槽，停用后保留队列 |
| Google Drive | 默认关闭。配置 web 用户所有、权限 0700 的专用本机临时根后，以 rclone `crypt(local)` 校验合成对象的文件名加密、内容加密、读回及清理。见[本机测试配置](docs/local-integration-probes.md#google-drive-配置占位) |

文件与测试根使用静态根以外的绝对路径。Google Drive 的账户、OAuth 和长期 crypt 密钥由 vira 另行配置于 fertile，Web 的 Drive 卡只验证本机加密链路。

## 备份与采集缺口

| 环境变量 | 本机固定文件 |
| --- | --- |
| `WEB_BACKUP_STATUS_FILE` | 可选。指向 T2b 发布的 `run/backup-remote-status.json`。未配置时状态为 `disabled` |
| `WEB_BACKUP_RESTORE_FILE` | 可选。指向同一实例的 `run/backup-remote-restore.json`，需同时配置状态文件 |
| `WEB_QUALITY_STATUS_FILE` | 可选。指向授权质量发布者原子替换的本机 JSON 快照 |

文件必须为静态根之外的绝对路径和普通文件，末级禁止符号链接。切换备份目标时由 fertile 刷新状态和隔离恢复凭据。查询读取状态文件，不执行备份、恢复或补采。T2b 的恢复截点由清单验证推进，失败后可保留最后合格水位，并更新观测时间。查看 `alert` 可识别已启用备份的缺失、降级和超过一小时的恢复点。由部署提供已授权的 F3 输入文件并执行 `web publish-quality INPUTS` 发布缺口快照。[发布配置](docs/quality-publishing.md) · [运维状态接口](docs/api.md#备份与采集质量)

## 告警与邮件进程

默认关闭邮件进程。`FERTILE_ENABLE_WEB_NOTIFIER=0` 时，fertile 守护器不会启动 `web-notifier`。需要启用时，先对与 web 共用的 `DATABASE_URL` 执行 `web migrate`，确认专用部署环境中有完整的 SMTP 主槽，包括中继、发件人、本人收件人和中继所需的认证，再在 fertile 设置 `FERTILE_ENABLE_WEB_NOTIFIER=1`。启用后用隔离队列核对一次投递结果。进程不监听端口，依赖 Linux `/proc`、系统 CA 和 `/usr/bin/curl` 的 SMTP/SMTPS 支持。SMTP 使用 TLS 1.2 或更高版本，证书与主机名由系统 CA 验证。

如果直接启动二进制时传入 `FERTILE_ENABLE_WEB_NOTIFIER=0`，或本机缺少任何可能可用的 SMTP 配置，进程在连接 PostgreSQL 前进入安全空转。否则启动时只读检查数据库选择的 SMTP 配置。本机配置和数据库预检判定空转时都先记录一次 `notifier idle`，再等待停止信号。诊断使用固定阶段和原因。空转保留 outbox、尝试、延迟和投递健康记录，无 SMTP 请求。修复配置后重启，原队列继续应用投递策略。

尚未启用 W4 SMTP 版本时，普通任务和独立渠道测试依赖主槽。主槽要求有效的 `WEB_SMTP_HOST`、`WEB_SMTP_FROM`、`WEB_SMTP_TO` 以及配置的可选字段。此时仅配置备选槽和快照目录会空转。启用 W4 后，普通任务与独立渠道测试使用活动版本绑定的快照。保留 `WEB_SMTP_SNAPSHOT_DIR`、对应密钥和可读取解密的绑定文件后，移除主槽并重启仍可投递。缺目录、缺文件或快照无法解密时，保留原队列并等待恢复。备选槽用于创建 W4 候选快照，空目录在主槽缺失时不会生成新密钥。

普通投递可用时，启动检查也会读取待重试 W4 候选测试已绑定的快照。若已配置存储但该文件缺失或无法解密，进程记录一次 `SMTP candidate test paused`，保留该候选的尝试、下次投递时间和测试证据，继续处理其他可投递邮件。恢复原文件和密钥后重启，候选按原 UUID 重试。未设置快照目录时，W3 主槽仍可投递，W4 候选继续排队。如果前一进程已领取候选，接手者等满 70 秒后将该次尝试结算为 `interrupted`，按原重试上限处理，释放发送名额给 W3。

进程运行中，若领取后读取候选的绑定文件失败，outbox 和集成测试保持 `pending`，outbox 保留绑定 UUID，尝试表记录 `configuration_error`，日志记录 `SMTP candidate test deferred` 与固定原因 `bound candidate SMTP snapshot is unavailable`。日志省略路径、快照 UUID 和 SMTP 凭据。退避从 30 秒起逐次翻倍，到 240 秒封顶，文件持续不可读时超过常规尝试上限也继续等待。队列仍遵循小时额度。将原文件以 0600 权限恢复到运行进程打开的快照目录后，候选到期自动重试。若进程曾在文件缺失期间重新启动，按上一段的启动暂停流程恢复。

通过管理接口停用 SMTP 时，以数据库领取任务的时刻为界。停用事务先提交，普通任务继续排队。已领取的尝试按实际结果结算。切换活动版本时，下一次领取重新核对当前绑定。

SIGINT 或 SIGTERM 让空转进程以状态码 0 退出。正常投递的进程收到同样信号后停止领取新任务，并结算已领取的任务。配置快照目录时，最长发送与结算宽限为 78 秒，随后留两秒关闭连接池。部署管理器设置 85 秒停止宽限。有效 SMTP 已配置时，其他启动失败仍以状态码 1 退出。notifier 与 web 共用日志过滤器，标准错误的 JSON 事件使用固定 `stage` 和 `reason`。提高 `RUST_LOG` 级别仍会过滤第三方追踪。

| notifier 环境变量 | 含义及默认值 |
| --- | --- |
| `FERTILE_ENABLE_WEB_NOTIFIER` | fertile 守护器的进程开关，默认 `0`。直接执行且显式传入 `0` 时进入空转 |
| `DATABASE_URL` | 由部署注入，与 web API 共用 PostgreSQL |
| `WEB_SMTP_HOST`、`WEB_SMTP_PORT` | 固定中继的主机名或 IP。隐式 TLS 默认端口 465，STARTTLS 默认 587 |
| `WEB_SMTP_TLS` | `implicit`（默认）或 `starttls` |
| `WEB_SMTP_FROM`、`WEB_SMTP_TO` | 固定发件地址和本人收件地址，各一个邮箱 |
| `WEB_SMTP_CREDENTIAL_FILE` | 可选，部署提供的本机秘密文件绝对路径，运行用户所有、0600 普通文件、存放于静态目录之外。仅含 `username` 和 `password` 两个 JSON 字符串字段 |
| `WEB_SMTP_TIMEOUT_SECONDS` | 单次 SMTP 超时，默认 10 秒，范围 1–60 |
| `WEB_SMTP_SNAPSHOT_DIR` | W4 候选测试和已绑定快照的投递所需。由 notifier 运行用户独占的 0700 持久绝对目录，置于静态资源树之外 |
| `WEB_STATIC_ROOT` | 配置快照目录时必填。由部署为 Web 和 notifier 传入同一个静态根绝对路径 |
| `WEB_PUBLIC_ORIGIN` | 可选，与 web API 相同的规范 HTTPS origin。配置后邮件附私有 `/alerts` 绝对链接 |
| `WEB_NOTIFIER_QUIET_TIME_ZONE` | 默认 `Asia/Shanghai`，从 PostgreSQL 的 IANA 时区表校验 |
| `WEB_NOTIFIER_QUIET_START`、`WEB_NOTIFIER_QUIET_END` | 默认 `23:00`–`08:00`，当地时间 `HH:MM`。静默开始包含、结束排除，两值相同关闭静默 |
| `WEB_NOTIFIER_COOLDOWN_SECONDS` | 默认 900 秒，范围 0–604800。规则可覆盖此值 |
| `WEB_NOTIFIER_NORMAL_HOURLY_LIMIT`、`WEB_NOTIFIER_URGENT_HOURLY_LIMIT` | 滚动一小时各自允许的投递尝试数，默认普通 6、紧急 2，各允许 1–1000 |
| `WEB_NOTIFIER_URGENT_BYPASS_QUIET` | 默认 `true`，紧急投递可绕过静默，仍计入紧急额度 |

notifier 在打开或创建快照密钥前核对快照目录与部署给 Web 的静态根及其解析目标。配置了快照存储且静态根缺失、无效或不可用时，notifier 空转。快照路径必须逐级为实际目录，包含符号链接或父目录穿越的路径会被拒绝。静态根可使用部署的 `current` 链接。

更新 SMTP 部署槽位后重启 notifier，再创建并测试候选版本。现有活动版本仍使用绑定的加密快照。目录中的 `snapshot.key` 和快照均为 0600 文件。备份与恢复时保留数据库、全部仍被引用的快照和对应密钥，限制密钥访问。升级时，早期 W4 未绑定快照的测试需要重新运行，未绑定的旧活动配置需在补测通过后重新启用才会投递。运维验证可由管理员通过 W4 集成测试，或用新 UUID 通过邮件渠道发起测试，按各接口返回的路径查询。测试邮件占用普通额度，可在静默窗口执行。[API](docs/api.md#告警规则与邮件通知)

| 现象 | 诊断与恢复 |
| --- | --- |
| 测试一直 `pending` | 确认 fertile 已启用 notifier，查看是否有 `notifier idle` 启动日志。投递进程运行时，在测试记录中核对 `available_at` 和 `deferrals`，确认可读取已迁移的目标库 |
| W4 候选日志出现 `SMTP candidate test deferred` | 按 outbox 的绑定 UUID 恢复对应的原快照文件，核对尝试表中的 `configuration_error` 和 outbox 的 `available_at` |
| 投递 `pending` 且最近结果为 `timeout`、`transient` 或 `interrupted` | 查看尝试表、静默和额度，进程按 30 秒起的指数退避自动重试，最多 5 次尝试 |
| 投递 `failed` | 核对有限错误分类、TLS 证书、部署地址和本机凭据文件权限，修复后使用新 UUID 发测试邮件 |
| system/status 的 `notifications.state` 为 `degraded` | 管理员可在 `/alerts` 查看一次 SMTP 失效事件与后续恢复。每轮故障后的首次 SMTP 接受都会记录恢复并排入该轮失败尝试汇总，包括上一轮汇总重试成功的情况 |
| 投递 `accepted` | SMTP 中继已接受邮件。收件箱送达、异步退信与垃圾箱状态由邮件系统跟踪 |

一次尝试的结果未知时，同一 outbox ID 生成相同 `Message-ID`。SMTP 接收后进程崩溃或响应超时可能产生重复邮件。数据库保留每次尝试、延迟决定及汇总关联。发布回退时先停 notifier，并按[独立库和数据根流程](#含新增迁移的发布回退)恢复。用新版投递记录核对恢复库中待发或待重试的 outbox，处理备份后已接收或结果未知的投递，避免重复发送。核对候选测试及其快照引用后，才允许旧 notifier 使用恢复库的 `DATABASE_URL` 启动。

停用等待重试的汇总中的任一规则后，notifier 会停止原汇总的后续发送，为仍可发送的告警排入带新 `Message-ID` 的替代投递。替代投递沿用原汇总的重试等待，并接受当前冷却、静默和限流检查。

排查成员告警时，读取其 `GET /api/v1/alerts/{id}` 详情。在原成员投递的 `summary_attempts` 中查找汇总的拒绝或超时，在 `summary_deferrals` 中查看该汇总的延迟记录。用替代投递的 `replaces_id` 对照原成员投递的 `id`，再看替代投递自身的 `attempts`。需要读取主告警详情时，使用原成员投递的 `summary_alert_id` 作为请求路径中的告警 UUID。

W5 升级时，停下旧 notifier，保留迁移前快照，在共用 PostgreSQL 上执行 `web migrate`，再使用包含 W5 的 web 和 notifier。使用独立数据库角色时，给 web 角色授予 `app.notification_silences` 的读取、插入、更新权限及其序列使用权，给 notifier 角色授予表的读取权与静默判断函数的执行权。迁移后的备份需包含静默审计表。回退至 W5 前版本时按[独立库和数据根流程](#含新增迁移的发布回退)恢复，并在对账时核对新版仍有效的静默。旧 notifier 会按旧队列策略投递，需先处理受静默影响的待发邮件。

主动静默覆盖紧急告警，独立测试邮件仍可用于检查 SMTP。排查持续 `pending` 的业务邮件时，分别查询告警实例和所属规则的 `/silence`。在最后一个适用静默解除或到期后，notifier 下一轮轮询重新检查投递资格，并继续应用静默时段、冷却、重试等待和额度。已经领取的尝试按 SMTP 实际结果结算。[主动静默接口](docs/api.md#主动静默)

## 历史回测与提交确认

回测页从 /api/v1/backtests/datasets 读取已审核的历史标的。配置数据后提交 SmaCrossover，并在工作台查看作业状态和历史结果。POST 超时或连接中断后，使用管理员身份和原 Idempotency-Key 请求 GET /api/v1/backtests/by-key。若查到 ID，继续轮询该作业。只有明确返回 404 后才允许带原请求及原键重试。

先上线含 F14 的 finance。在切换 Web 前，用 finance 的内部 `GET /api/v1/backtests?visibility=legacy&limit=1` 确认返回 200。新版 finance API 首次打开已有队列或新版 worker 启动时，会在原 SQLite 队列建立 `jobs_legacy_id` 部分索引。大队列应在此步骤完成建索引。若回滚至 F14 之前的 finance，应配套回滚 Web。

提交与恢复返回的作业 ID 可达 `9223372036854775807`。排查时保留原十进制文本。匿名分页沿用最后一个可见作业的 ID。试用过旧分支临时句柄的客户端可从首页或最后已知公开 ID 重新读取。[分页语义](docs/api.md#作业与行情)

### 策略上传与因子报告

1. 管理员把 UTF-8 文件正文以 `{format, content}` 发送到 `POST /api/v1/strategies/validate`。一次上传最多 256 KiB，413 返回 `body_too_large`。收到编译器 422 时，用 `error.path` 与从 1 起算的 `error.location.line`、`error.location.column` 定位修改。
2. 校验后用相同正文调用 `POST /api/v1/strategies`。注册超时或连接中断后，先用预览中的 ID 和版本查询 `GET /api/v1/strategies/{id}/versions/{version}`。查询成功时核对 `strategy.content_sha256`，明确 404 后才用原文件注册。409 时读取已有版本，内容变更应使用新版本。
3. 以 `strategy: "StrategyBundle"`、UTC 午夜起止时间和 `source` 调用原回测提交入口。已登记版本使用 `kind: "registered"`、`id`、`version`，上传使用 `kind: "inline"`、`format`、`content`。若 finance 提示没有回放数据，在 finance 核对编译计划引用的获准数据集和成分版本。
4. 因子目录和报告来自 finance 已发布的 F8 本机快照。目录为空且 `as_of: null` 时，核对对应快照是否已发布。浏览器按报告的版本、许可、可用时间、覆盖和 `research_only` 等状态展示。旧 `/backtests?job=<id>` 仍加载终端页面，再从作业详情 API 读取结果。

[策略与研究接口](docs/api.md#策略与研究)
