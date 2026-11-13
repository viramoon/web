# 按资源组织路由

规划 trading / backtest / dashboard / blog / health 五组资源，路由层只处理请求、认证、输入转换和错误映射，调用领域逻辑与 finance_client，不承担交易内核。交易、回测、看板、博客的具体路径与权限 TBD。

health 提供 /healthz，并区分 live / ready；判断数据库、finance 状态和静态产物新鲜度的细则见 [service.yaml](../../service.yaml)。所有接口由 handler 的 utoipa 声明导出；TBD 路由不手写成 OpenAPI 文件。
