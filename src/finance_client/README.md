# finance HTTP 客户端

输入是 finance 从代码导出的 fertile `contracts/finance.openapi.json`，输出为本模块的生成代码，禁止手改。生成工具和命令 TBD，候选为 progenitor；选型后记录版本、输入快照来源与可重复生成步骤。

统一连接、认证、错误分类和读超时；写请求超时先查询状态再决定重试，不能由通用重试中间件盲目重发。交易数值保留精度。当前没有 OpenAPI 快照，故不创建假客户端或依赖配置。
