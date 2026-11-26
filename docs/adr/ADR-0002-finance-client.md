# ADR-0002 从 finance 的 OpenAPI 3.1 生成 Rust 客户端

状态为已采纳，日期为 2026-11-22。适用于 web 调用 finance 的 HTTP 边界。关联 [web issue #5](https://github.com/viramoon/web/issues/5)。

## 背景

finance 使用 utoipa 5 从 Rust 代码导出 OpenAPI 3.1。早期候选 progenitor 针对 OpenAPI 3.0。将契约降为 3.0 会多维护一次转换，尤其要处理既可省略、又可显式传 null 的字段。

## 决策与备选

采用 OpenAPI Generator CLI 7.27.0 的 Rust/reqwest 后端。输入保存在 [finance.openapi.json](../../openapi/finance.openapi.json)，对应的完整提交保存在 [finance.commit](../../openapi/finance.commit)。脚本支持从已签出的 finance 仓库按 Git 提交导出、从固定快照生成，以及在总仓比对选定契约。生成器的 Maven JAR 通过脚本内的固定 SHA-256 校验。输出为 [finance-client](../../crates/finance-client/Cargo.toml)，由 web 配置同一个 reqwest 客户端、连接时限和响应时限。重试、熔断及 HTTP 状态映射由 web 的调用层承担。

交易金额在接口和 Rust 模型中保持字符串。utoipa 将 Rust 的无符号整型导出为带非负下界的 int32、int64 或不带 format 的 integer。生成器使用官方 normalizer 扩展点识别这一组合并选择 u32 或 u64，让计数能读回 finance 可发出的完整范围。有符号作业 ID、分页游标保留 i64。日期时间保持契约中的字符串，嵌套 Option 区分缺失和显式 null。生成器在内存中读取原始快照，仓库内记录的快照由原导出命令逐字节写入。

F1 Event 的 OpenAPI 用 `allOf` 组合共享信封、消息分支和版本约束。生成器的 Rust 后端会将组合中的消息分支合并成单一 payload 类型。normalizer 将内存中的 Event 转成共享信封与生成的消息联合类型，模板使用 Serde flatten 写回原来的 `kind` 和 `payload` 字段。原 OpenAPI 快照及 finance 独立导出的 F1 schema 保留完整的分支和版本约束。回归测试逐个验证已发布的消息分支、精确十进制字符串、未知成交方向和显式 null。

生成脚本按固定 JAR 中的原版模板应用一份小补丁，省略空文档、使用等价的 Rust match 表达式和派生的枚举默认值，让生成的 Rust 通过同一套格式和 Clippy 检查。生成的内部 crate 标为 publish = false，未声明许可证的契约保留空缺。调整生成行为时修改模板、normalizer 或生成器版本，随后运行脚本再审查 diff。

## 影响与验证

OpenAPI Generator 的 3.1 支持仍标为 beta。升级生成器、Rust 或契约时，用已锁定 SHA 再次导出，运行生成一致性检查和 Rust 测试。数值边界回归覆盖 u64::MAX、超过 JavaScript 安全整数范围的精确金额字符串、有符号负游标、错误类型与溢出。普通 PR 的生成检查只读仓库内已提交快照，因此也可在仅能访问 web 的 CI 中执行。

## 后续工作

fertile 集成同一组子模块指针时，从其 finance 子模块代码导出到 contracts/finance.openapi.json，并以该文件运行 web 的快照校验。命令和依赖见 [finance 客户端操作说明](../../src/finance_client/README.md)。
