# frontend 静态托管

frontend 是 Vite 单页应用，由 axum 托管构建产物，不单独起服务。产物来源必须是选定 fertile tag 对应 frontend commit 的可重复构建，包含 index.html、带内容摘要的 assets 和产物摘要 / 来源记录；命名见 [frontend build](https://github.com/viramoon/frontend/blob/main/docs/build.md)。

静态根目录从配置读取，默认 /home/box 下的版本化目录；验证来源与文件完整后再切换目录，失败恢复上一个 fertile tag 的产物。带摘要资源可长期缓存，index.html / 版本入口短缓存或重新验证；具体时长 TBD。

SPA 页面回退不能吞掉 API、/healthz 或不存在的静态资源。TBD：路由前缀、缓存、原子切换和权限，由静态托管实现 issue 确认。本目录不提交构建产物。
