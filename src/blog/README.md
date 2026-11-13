# 博客模块

管理文章、内容状态与相关业务规则，持久化归 PostgreSQL blog schema。入口注册在 routes/blog，数据库迁移由 migrations 统一管理，不在此目录散落独立迁移。

TBD：文章模型、发布权限、内容渲染和缓存策略，待博客需求 issue 确定。本次无内容管理实现。
