# 分层配置设计

规划 `config/base.yaml` + `config/cloudbox.yaml` 或 `config/stable.yaml`，按 base → profile → 受控环境变量 / 密钥引用覆盖；本次只记录字段，不创建运行配置。部署环境与 finance 的 Backtest / Sandbox / Live、paper / 实盘账户是独立维度，web 不通过 profile 隐式改变交易账户。

```yaml
profile: cloudbox
paths:
  root: /home/box/fertile
  data: /home/box/fertile/data/web
  logs: /home/box/fertile/logs/web
  static_root: /home/box/fertile/releases/TBD/frontend
  backups: /home/box/fertile/backups/web
server:
  bind_host: TBD
  port: TBD
  public_origin: TBD # 公网端到端检查入口
database:
  url_secret_ref: TBD
  schemas: [app, blog]
  data_root: /home/box/fertile/data/postgres
  max_connections: TBD
  migration_timeout_ms: TBD
finance:
  base_url: TBD
  auth_secret_ref: TBD
  read_timeout_ms: TBD
  read_retry_limit: TBD
  max_data_age_ms: TBD
auth:
  session_secret_ref: TBD
  allowed_origins: TBD
  permissions: TBD
idempotency:
  storage_schema: app
  retention: TBD
  request_timeout_ms: TBD
static:
  release_id: TBD # 对应 fertile tag 与 frontend 指针
  immutable_asset_cache: TBD
  index_cache: TBD
  spa_fallback: true # 仅页面路由，不吞 API、健康和资源缺失
health:
  path: /healthz
  ready_dependencies: [database, finance, static]
telemetry:
  log_level: TBD
  metrics_address: TBD
  redact_fields: TBD
```

TBD：端口、域名、连接池、超时、缓存时长和权限，由实现与网络验证 issue 确认。所有路径由配置读取，默认 /home/box 下；cloudbox / stable 只覆盖环境参数，不维护两套业务实现。环境变量规划为 FERTILE_PROFILE、FERTILE_DATA_ROOT、WEB_STATIC_ROOT、FINANCE_BASE_URL 和数据库密钥引用，具体加载规则实施时验证。
