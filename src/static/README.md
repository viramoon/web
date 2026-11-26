# frontend 静态托管

frontend 是 Vite 单页应用，由 axum 托管构建产物，不单独起服务。本目录不提交构建产物。产物来源必须是选定 fertile tag 对应 frontend commit 的可重复构建，包含 index.html、带内容摘要的 assets 和产物摘要 / 来源记录；命名见 [frontend build](https://github.com/viramoon/frontend/blob/main/docs/build.md)。

静态根目录从配置读取，默认 /home/box 下的版本化目录；验证来源与文件完整后再切换目录，失败恢复上一个 fertile tag 的产物。带摘要资源可长期缓存，index.html / 版本入口短缓存或重新验证；index.html 始终 no-cache，Vite /assets/ 下带内容哈希的文件使用 public, max-age=31536000, immutable。

`/api`、`/healthz` 及其子路径由 API 和健康检查路由处理。其余 GET、HEAD 优先查找静态根中的文件。URL 从静态根起算。例如，`/third-party/lightweight-charts/LICENSE` 和 `/third-party/lightweight-charts/NOTICE` 直接返回相应文件。

查找未命中时，`/assets`、`/static`、`/third-party` 及其子路径返回 404。最后一个有效路径段含 `.` 或恰为 `LICENSE`、`NOTICE` 的请求也返回 404。判断前对 URL 路径做百分号解码，忽略空段和 `.` 段，保留大小写。无法解码为 UTF-8 的路径也返回 404。其余未命中的 GET 回退到 `index.html`，例如 `/dashboard`、`/trading`、`/backtests`。HEAD 未命中时返回 404。
