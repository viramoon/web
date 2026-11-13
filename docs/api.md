# web API 与契约流程

| 接口组 | 职责与占位 |
| --- | --- |
| 交易 | 策略 / 订单状态、受控操作；具体路径、权限、幂等与精确数值表示 TBD |
| 回测 | 任务提交、状态和结果；异步语义与资源限制 TBD |
| 看板 | 聚合行情与账户 / 业务展示，数据时间与陈旧标识；查询和刷新规则 TBD |
| 博客 | 文章读取及受控写入；内容模型、发布权限和缓存 TBD |

所有具体接口由后续 handler 代码确定，使用 utoipa / utoipa-axum 导出 OpenAPI，不能在本次手写契约。快照为 fertile `contracts/web.openapi.json`；finance 的输入快照为 `contracts/finance.openapi.json`，web 根据它生成 finance_client（工具 TBD，候选 progenitor）。

变更流程：改代码并验证 → 从相同 commit 导出 → 向 fertile 提 PR 更新快照与子模块指针 → CI 比对差异 → 重生成消费者客户端 → 集成检查。破坏性变更先制定兼容窗口和迁移，生成产物禁止手改。规则见 [fertile contracts](https://github.com/viramoon/fertile/tree/main/contracts)。

frontend 全局只有一个调用 web 的 API client，不直接连接 finance。实时 WebSocket / SSE 需单独定义消息与重连协议，不能依赖 OpenAPI 自动覆盖。健康 /healthz 提供 live / ready 语义，具体选择方式和响应格式 TBD。
