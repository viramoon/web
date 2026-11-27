# frontend 静态托管

frontend 是 Vite 单页应用，由 axum 托管构建产物，不单独起服务。本目录不提交构建产物。产物来源必须是选定 fertile tag 对应 frontend commit 的可重复构建，包含 index.html、带内容摘要的 assets 和产物摘要 / 来源记录；命名见 [frontend build](https://github.com/viramoon/frontend/blob/main/docs/build.md)。

静态根目录从配置读取，默认 /home/box 下的版本化目录；验证来源与文件完整后再切换目录，失败恢复上一个 fertile tag 的产物。带摘要资源可长期缓存，index.html / 版本入口短缓存或重新验证；index.html 始终 no-cache，Vite /assets/ 下带内容哈希的文件使用 public, max-age=31536000, immutable。

`/api`、`/healthz` 的已注册路由照常处理。路由未命中时，对 URL 路径做一次百分号解码，忽略空段和 `.` 段，保留大小写。首段为 `api` 或 `healthz` 时返回 JSON 404，包括带编码前缀或无效后缀的路径。

其余 GET、HEAD 若解码后含 `..` 段或无法解码为 UTF-8，直接返回 404。解码并忽略空段和 `.` 段后，路径恰为 `/NOTICE` 时提供静态根中的 `third-party/lightweight-charts/NOTICE`，目标文件缺失时返回 404。其他有效路径按静态根中的相对路径查找文件，例如 `/third-party/lightweight-charts/LICENSE` 和 `/third-party/lightweight-charts/NOTICE`。

查找未命中时，`/assets`、`/static`、`/third-party` 及其子路径返回 404。`/strategies/{id}` 的合法策略 ID 可含点，按页面处理。其他末段含 `.` 或恰为 `LICENSE`、`NOTICE` 的请求返回 404。页面 GET 回退到 `index.html`，例如 `/dashboard`、`/trading`、`/backtests?job=7`。HEAD 未命中时返回 404。
