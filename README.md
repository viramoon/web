# web

fertile 的后端网关与 submodule。使用 axum 提供浏览器 API、通过 HTTP 连接 finance、托管 frontend 静态文件，管理业务与博客的 PostgreSQL schema 及迁移。

本仓库按 issue #1 方案 v2 初始化。axum 服务提供健康检查、带行情新鲜度的系统状态、静态托管与 PostgreSQL 迁移，并经 finance 客户端提供只读查询和管理员回测作业提交。管理接口支持短期同源会话、产品权限查询、加密凭据录入和集成候选测试、启用与回滚。运维页面可读取备份恢复点和授权的采集缺口。`web publish-quality` 将本机 F3 观测发布为缺口快照。`web-notifier` 消费 PostgreSQL outbox，并按活动 SMTP 版本绑定的加密配置投递邮件。见 [issue #1 方案 v2](https://github.com/viramoon/web/issues/1#issuecomment-6834604360)、[运行手册](RUNBOOK.md) 和 [AGENTS.md](AGENTS.md)。

W2a 提供管理员专用的 F10a 行情读取与 WAL 视图、PostgreSQL 自选、盘口 focus 和运维 SSE。市场 WebSocket 在同一服务端口提供授权的有界 WAL 回放、共享订阅额度和重置帧。期权查询和券商线路额度等待 finance 后续契约。见 [行情、自选与推送](docs/api.md#行情自选与推送)。

W6 将内部 finance 契约锁定到 `fc5e153`。WAL 与投影 REST 读取兼容新增的 2.0 永续事件。浏览器行情和指标查询继续使用 W2a 的 WAL 视图，市场 WebSocket 仍使用 1.0 数据帧。[契约差异](docs/api.md#finance-契约版本)

## 系统中的位置

| 仓库 | 在 fertile 系统中的职责 |
| --- | --- |
| [fertile](https://github.com/viramoon/fertile) | 金融看板 + 自动交易系统的总仓库；初始化、集成、监控、运维和发布 |
| [finance](https://github.com/viramoon/finance) | 主项目；基于 NautilusTrader 的策略、数据采集、交易、回测和 HTTP API |
| [web](https://github.com/viramoon/web) | axum 后端网关；调用 finance HTTP API，管理 PostgreSQL 业务与博客数据，托管前端静态文件 |
| [frontend](https://github.com/viramoon/frontend) | React + TypeScript + Vite 单页应用；只调用 web |
| [agent](https://github.com/viramoon/agent) | 独立的开发流程知识库；通用 skill、prompt、工作流、评估和复盘 |


系统关系、版本、契约、配置、健康和环境规则见 [docs/README.md](docs/README.md)。服务部署唯一来源是 fertile deploy/，agent 只分发知识；只有 fertile 对外发版，Maas · Ruler 只按 fertile tag 部署 / 回滚。
## 目录说明

```text
web/
├── README.md / AGENTS.md / RUNBOOK.md / service.yaml
├── .agents/skills/          本仓库专属 skill 约定
├── config/                 base / cloudbox / stable 分层配置
├── migrations/             sqlx 时间戳迁移，只增不改
├── src/                    web 进程、API 与模块说明
│   ├── routes/             trading / backtest / dashboard / blog / health
│   ├── domain/ / blog/ / auth/ / idempotency/
│   ├── notifications/       告警规则、事件、主动静默、事务 outbox 和邮件投递
│   ├── integrations/        版本事务与集成测试适配
│   ├── ops/                 T2b 备份与采集质量只读状态
│   ├── finance_client/     从 finance 快照生成
│   ├── static/ / openapi/  静态托管与接口导出
│   └── startup/ / telemetry/
├── tests/api/              独立 PostgreSQL、HTTP 和信号测试
├── docs/                   架构、数据库、接口与 ADR
└── .github/                PR 模板、文档与 Rust CI
```

从仓库根目录执行 cargo run --locked -- migrate 与 cargo run --locked -- serve。默认监听 127.0.0.1:8090，数据库和静态产物的前置准备见 RUNBOOK.md。cargo run --locked -- export-openapi 将契约写入标准输出。

管理员可以按规则或单条告警设置带截止时间和原因的主动静默，也可提前解除。告警事件照常记录，受影响的邮件继续排队，到期或解除后恢复投递资格。[告警接口](docs/api.md#主动静默)

fertile 默认关闭 `web-notifier`。手动启动时，普通投递使用活动 W4 版本绑定的 SMTP 快照，首次启用 W4 前使用 SMTP 主槽。显式关闭或所需配置不可用时，进程安全空转并保留 outbox，收到停止信号后正常退出。[安全启动条件](RUNBOOK.md#告警与邮件进程)

## 文档索引

- [AGENTS.md](AGENTS.md)：开发流程与硬规则
- [docs/README.md](docs/README.md)：知识放置、系统约定与文档索引
- [架构与请求流](docs/architecture.md)
- [schema 与迁移](docs/database.md)
- [API 与导出流程](docs/api.md)
- [日志、样本与 CI 防护](docs/data-guard.md)
- [技术栈决策](docs/adr/ADR-0001-tech-stack.md)
- [模块索引](src/README.md)
- [配置字段](config/README.md)
- [迁移约定](migrations/README.md)
- [API 验证设计](tests/api/README.md)
- [运行手册](RUNBOOK.md)
- [服务设计清单](service.yaml)

## 协作与检查

一个任务对应一个 issue，使用 issue-<号>-<简述> 分支和 PR，CI 通过后合并，禁止直接推送 main。PR 需含 Closes #<号>、测试结果和逐条验收自检；开发问题在原分支解决，文档随行为在同一 PR 更新。文档 CI 检查必需文件、非空内容、YAML、相对链接及 PR 描述。Rust CI 检查格式、clippy、集成测试、SQLx 离线元数据和 OpenAPI 导出。

每个克隆在首次提交或推送前按[运行手册](RUNBOOK.md#安装仓库数据防护)安装数据防护钩子。新增或修改的文件须匹配[路径与类型白名单](docs/data-guard.md#规则-v1)，或登记可复现的合成来源。各 CI 检查先扫描 PR 引入的 Git 对象，测试和生成校验只报告检查结果。
