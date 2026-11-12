# 编码 agent 工作规范

本规范从初始化 PR 起生效。开始工作先读本文件、关联 issue 的最新方案和本仓库文档；issue #1 的「方案 v2」优先于正文 v1。

## 任务与交付

- 一个任务对应一个 issue；一个 agent 会话只做一个 issue，越界改动另开 issue。
- 分支命名为 `issue-<号>-<简述>`；本次初始化使用 `issue-1-init`。禁止直接推送 `main`，所有变更必须走 PR，CI 必须通过后才能合并。
- 开发中测出的问题在本分支修复到测试通过，不另开 issue；issue 只记录需求和现网问题。
- PR 描述必须含 `Closes #<号>`、改动说明、测试结果以及开发中问题的解决过程。逐条核对 issue 的验收标准。
- 代码变更必须在同一个 PR 同步更新文档；仓库文档是当前事实（current truth），仓库之外不维护第二份文档。
- 方案改版以 issue 评论追加，写明改了什么、为什么，保留原文作为历史；重大决策使用 [ADR 模板](docs/adr/TEMPLATE.md)。

## 硬规则

- 不许为了让测试通过而削弱、删除、绕过或改写测试；修复导致失败的问题。
- 生成的文件（OpenAPI 快照、生成的客户端等）只能通过生成器或导出流程修改，禁止手改。
- 不做与本 issue 无关的顺手重构。
- finance、web 的价格、数量、金额等交易相关数值必须用精确类型（定点 / Decimal / 项目领域类型），不用浮点；跨接口传输也必须保留精度。
- 本仓库专属 Codex skill 放在 `.agents/skills/<名字>/SKILL.md`，元数据必含 `name` 和 `description`；通用 skill 在 [agent](https://github.com/viramoon/agent/tree/main/skills) 维护，不在各仓库复制。

## 初始化范围与校验

issue #1 只写中文文档和仓库组织结构，允许最小文档检查 CI；不写 Rust / Python / TypeScript / JavaScript 实现，不添加 Cargo.toml、package.json、pyproject 等构建配置。未来目录只放 README 说明，未定项写 `TBD` 并说明要填什么，不能把设计占位当成已运行的能力。

提交前检查必需文件、非空内容、相对链接、YAML 格式、文档一致性和 `git diff --check`；PR 上的文档检查必须通过。详细知识放置规则见 [docs/README.md](docs/README.md)。

## web 专属规则

交易数值全链路使用精确类型，不转为浮点。finance_client 只能由 finance 的 OpenAPI 快照生成；迁移使用 sqlx 时间戳命名，只增不改已应用迁移。WebHook 输入先持久化再幂等处理，写请求超时必须先查后试；静态资源由配置路径读取。详见 [架构](docs/architecture.md) 和 [数据库](docs/database.md)。
