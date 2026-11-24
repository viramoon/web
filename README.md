# web

fertile 的后端网关与 submodule。使用 axum 提供浏览器 API、通过 HTTP 连接 finance、托管 frontend 静态文件，管理业务与博客的 PostgreSQL schema 及迁移。

本仓库按 [issue #1 方案 v2](https://github.com/viramoon/web/issues/1#issuecomment-6834604360) 初始化。当前 axum 服务提供健康检查、带行情新鲜度的系统状态、静态托管与 PostgreSQL 迁移，并经 finance 客户端提供只读查询和管理员回测作业提交，使用方法见 [运行手册](RUNBOOK.md)。开发从 [AGENTS.md](AGENTS.md) 开始。

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
│   ├── finance_client/     从 finance 快照生成
│   ├── static/ / openapi/  静态托管与接口导出
│   └── startup/ / telemetry/
├── tests/api/              独立 PostgreSQL、HTTP 和信号测试
├── docs/                   架构、数据库、接口与 ADR
└── .github/                PR 模板、文档与 Rust CI
```

从仓库根目录执行 cargo run --locked -- migrate 与 cargo run --locked -- serve。默认监听 127.0.0.1:8090，数据库和静态产物的前置准备见 RUNBOOK.md。cargo run --locked -- export-openapi 将契约写入标准输出。

## 文档索引

- [AGENTS.md](AGENTS.md)：开发流程与硬规则
- [docs/README.md](docs/README.md)：知识放置、系统约定与文档索引
- [架构与请求流](docs/architecture.md)
- [schema 与迁移](docs/database.md)
- [API 与导出流程](docs/api.md)
- [技术栈决策](docs/adr/ADR-0001-tech-stack.md)
- [模块索引](src/README.md)
- [配置字段](config/README.md)
- [迁移约定](migrations/README.md)
- [API 验证设计](tests/api/README.md)
- [运行手册](RUNBOOK.md)
- [服务设计清单](service.yaml)

## 协作与检查

一个任务对应一个 issue，使用 issue-<号>-<简述> 分支和 PR，CI 通过后合并，禁止直接推送 main。PR 需含 Closes #<号>、测试结果和逐条验收自检；开发问题在原分支解决，文档随行为在同一 PR 更新。文档 CI 检查必需文件、非空内容、YAML、相对链接及 PR 描述。Rust CI 检查格式、clippy、集成测试、SQLx 离线元数据和 OpenAPI 导出。
