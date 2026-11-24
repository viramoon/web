# 文档与知识放置规则

| 载体 | 保存什么 | 更新方式 |
| --- | --- | --- |
| issue | 需求原话、实现方案和验收标准，作为历史记录 | 方案改版不覆盖原文，以评论追加版本、变化和理由，例如 v2 |
| PR | 怎么实现、测试结果、开发中问题及解决过程 | 关联 `Closes #<号>`，按验收标准自检 |
| 仓库文档 | 当前事实（current truth）与明确标注的设计 | 行为发生变化时，在同一个 PR 更新对应文档 |
| ADR | 重大决策、背景、备选和后果 | `docs/adr/ADR-<编号>-<标题>.md`，使用 [模板](adr/TEMPLATE.md)，后续变更另记 ADR |

原始需求与历史讨论留在 issue，PR 说明实现过程，文档只保留当前结论；仓库之外不维护第二份文档。文档统一用中文。目录中的 README 说明职责与边界，不代表该模块已经实现。

`TBD` 必须紧跟待确认事项、确认依据或负责阶段，不能用空文件代替说明。组件说明见 service.yaml；进程实际加载 config/ 内 YAML，完整部署由 fertile 管理。

## 系统共同约定

| 仓库 | 在 fertile 系统中的职责 |
| --- | --- |
| [fertile](https://github.com/viramoon/fertile) | 金融看板 + 自动交易系统的总仓库；初始化、集成、监控、运维和发布 |
| [finance](https://github.com/viramoon/finance) | 主项目；基于 NautilusTrader 的策略、数据采集、交易、回测和 HTTP API |
| [web](https://github.com/viramoon/web) | axum 后端网关；调用 finance HTTP API，管理 PostgreSQL 业务与博客数据，托管前端静态文件 |
| [frontend](https://github.com/viramoon/frontend) | React + TypeScript + Vite 单页应用；只调用 web |
| [agent](https://github.com/viramoon/agent) | 独立的开发流程知识库；通用 skill、prompt、工作流、评估和复盘 |

fertile 以 git submodule 引入 finance、web、frontend；agent 是独立仓库，不是 submodule。常驻业务服务只有 finance（trader / collector / api）和 web；frontend 构建产物由 web 托管，不单独起服务。

- **发布**：只有 fertile 对外带版本号。发布时更新各子模块指针 → 集成检查 → bump `VERSION` → 打 `vX.Y.Z` → 由发版脚本自动生成 `RELEASES.md` 条目（包含子模块 PR / issue），并自动给子模块打内部 tag `fertile-vX.Y.Z`。子模块不对外发版。Maas · Ruler 只按 fertile tag 部署和回滚。规则唯一入口是 [fertile docs/release.md](https://github.com/viramoon/fertile/blob/main/docs/release.md)，具体脚本由总仓库维护。
- **部署**：服务部署以 [fertile deploy/](https://github.com/viramoon/fertile/tree/main/deploy) 为唯一准绳。agent 的 deploy-map.yaml 只管 skill / prompt / 工作流分发，涉及服务只引用 fertile。
- **契约**：finance、web 的 OpenAPI 从代码导出（Rust 侧用 utoipa），快照存放于 [fertile contracts/](https://github.com/viramoon/fertile/tree/main/contracts)，CI 比对差异；不手写契约，不手改生成客户端。web ↔ finance 走 HTTP，frontend 只调用 web。web 使用 export-openapi 命令导出当前接口。
- **配置**：部署环境使用 `config/base.yaml` + `config/cloudbox.yaml` 或 `config/stable.yaml`，profile 与运行模式是两个独立维度。finance 的运行模式为 Backtest / Sandbox / Live；paper 与实盘是 Live 下不同账户配置。本阶段云电脑只用 IBKR paper；未来迁移专用机器的实盘需另行验证。
- **路径与恢复**：按只有 `/home/box` 持久处理。所有运行时路径从配置读取，默认根目录位于 `/home/box` 下，不硬编码 `/srv`、`/etc`。系统级改动纳入可重复执行的初始化设计。环境约束见 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。
- **健康**：finance、web 使用 `/healthz/live` 检查进程存活、`/healthz/ready` 检查依赖就绪状态。web/frontend 故障不影响 finance 交易核心，finance 不依赖 Cloudflare Tunnel。

## 本仓库文档索引

- [架构与请求流](architecture.md)
- [schema 与迁移](database.md)
- [API 与导出流程](api.md)
- [技术栈决策](adr/ADR-0001-tech-stack.md)
- [模块索引](../src/README.md)
- [配置字段](../config/README.md)
- [迁移约定](../migrations/README.md)
- [API 验证设计](../tests/api/README.md)
- [运行手册](../RUNBOOK.md)
- [服务设计清单](../service.yaml)
