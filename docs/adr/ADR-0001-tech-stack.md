# ADR-0001：网关技术栈与组织方式

状态：已采纳（设计，尚未实现）

依据：issue #1 方案 v2

适用范围：web

## 决策与理由

采用 Rust axum + sqlx + PostgreSQL。按资源组织 routes，独立 startup、telemetry、idempotency 和 tests/api，借鉴 [Zero To Production](https://github.com/LukeMathWalker/zero-to-production) 与 [realworld axum sqlx](https://github.com/launchbadge/realworld-axum-sqlx)。

用 utoipa 从代码导出接口，快照放 fertile contracts/，不手写契约；与 finance 走 HTTP，客户端根据 finance 快照生成。frontend 使用 Vite SPA，静态文件由 web 托管，减少云电脑常驻进程与隧道。

借鉴 [loco](https://github.com/loco-rs/loco) 的分层与 AGENTS / skills 模板，不采用整套框架，避免超出当前需求的依赖和框架约束。PostgreSQL app / blog schema 及 sqlx 时间戳迁移统一归 web。

## 后果与后续

web 不能承担交易核心生命周期；入口故障不影响 finance。必须验证精确数值、幂等、导出契约、SPA 回退和数据库恢复。TBD：依赖版本、生成器（候选 progenitor）、启动命令和运行配置，在实现 issue 确认。

环境约束见 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)。
