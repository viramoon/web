# Webhook 持久化与幂等

依据 [fertile ADR-001](https://github.com/viramoon/fertile/blob/main/docs/adr/ADR-001-cloud-box-network-constraints.md)，Webhook 先在 app schema 持久化，再以来源与唯一事件 / 请求 ID 去重；记录处理中、成功、失败和可恢复状态，不用进程内内存承担持久去重。

重复请求返回可核对的已有结果，写请求响应丢失时先查状态。记录保留、并发锁、事务边界、崩溃恢复和投递确认时机 TBD，需验证后才能承诺交付语义；交易核心不依赖外部 Webhook 触发。本次无实现或迁移。
