# 日志、样本与 CI 防护

v1 私有使用。付费行情、真实行情及其衍生样本、截图、数据库、日志导出和测试报告留在获准的私有环境。GitHub 仓库、PR、issue、LFS、CI 输出和 artifact 均在该边界之外。允许归档的数据及运行目录置于工作树之外，是否可以归档、计算和备份按各产品许可确定。

## 运行日志与审计

web 对标准错误输出 JSON。请求完成事件只记录 UUID 请求 ID、标准方法或 OTHER、匹配的路由模板或 <unmatched>、响应状态码和耗时。URI 参数、查询字符串、请求头、请求和响应正文均不写入日志。集成审计表只保存动作、集成及测试 ID、配置版本和审计时间。凭据正文只在校验及封装边界内处理。

启动、迁移及数据库异常使用固定阶段或有限错误类别，panic 使用固定事件。这些诊断省略底层异常正文、堆栈、SQL、配置值、令牌与上游响应原文。日志级别仍由 RUST_LOG 调整，事件目标限于 web 和 web-notifier 自身，两个进程共用日志过滤器。请求关联和故障定位见[运行手册](../RUNBOOK.md#故障定位与回滚)。

## 检查入口

| 入口 | 检查范围和输出 |
| --- | --- |
| pre-commit | 检查实际暂存 blob 和来源清单。部分暂存时以 Git 索引为准。 |
| pre-push | 检查每个非删除 ref 引入的提交。已有远端 ref 使用 Git 提供的原 SHA，新 ref 使用远端跟踪 HEAD 或 main 为基准，缺失时检查完整历史。 |
| CI | 以 PR 基础 SHA 或 push 的先前 SHA 为基准。新分支优先用远端 main 为基准。每个新提交与第一父提交比对，覆盖合并分支以及后来被删除的对象。CI 先扫描再执行服务、文档和客户端校验。 |

[安装脚本](../scripts/install-data-guard-hooks.sh)可重复运行并保留现有钩子。如需把检查接入自定义钩子，分别调用 [.githooks/pre-commit](../.githooks/pre-commit) 和 [.githooks/pre-push](../.githooks/pre-push)。pre-push 须接收 Git 传来的参数和完整 stdin ref 列表，原钩子有需要时也须收到同一输入。浅克隆或缺少已知基准对象时，补齐本地历史和跟踪 ref 后再检查。

钩子仅显示拒绝原因和转义后的路径。路径含凭据形状时整体显示为 `<redacted-path>`，包括常见百分号编码的凭据路径。CI 扫描只打印固定拒绝原因与计数，测试、文档解析和再生成命令只打印检查状态。文档检查在进程内读取 PR 事件文件。OpenAPI 输出在临时目录中逐字节比较，工作流不上传 artifact。若需查看本地原始失败输出，在隔离环境按[运行手册](../RUNBOOK.md#安装仓库数据防护)复测。规则依靠 Git 对象和可识别特征，人工仍需审核内联文本、数据来源、私有资料和网页附件。钩子需要每个克隆启用。

## 规则 v1

[rules.json](../.data-guard/rules.json) 和 [sources.json](../.data-guard/sources.json) 采用与 finance FG 相同的 UTF-8 JSON 规则 v1。每个文件上限 256 KiB，须包含整数 `version: 1`，未知键、重复键或未知版本会被拒绝。各仓维护自己的入口和策略文件。路径为 Git 根相对路径，按 `/` 分段，禁止空段、反斜杠、控制字符和点路径段。规则匹配忽略 ASCII 大小写，来源清单使用原路径精确匹配。

| 字段 | 规则 |
| --- | --- |
| max_blob_bytes | 1 到 16,777,216 的整数，按 Git blob 字节数计。web 使用 262,144。 |
| blocked_path_components | 匹配任一目录段，如 logs、reports、secrets。 |
| blocked_suffixes | 匹配路径结尾。后缀用点起始，使用小写 ASCII 字母、数字、点、下划线或连字符。 |
| blocked_magic | 每项含从零起计的字节 offset 和小写 hex 签名。签名必须在大小上限内。 |
| fixture_paths | 匹配完整仓库相对路径的 shell glob。`*` 和 `?` 可跨路径段，也支持字符集合。 |
| fixture_suffixes | 后缀语法同 blocked_suffixes，匹配时需登记来源。 |

新增或修改的文件默认拒绝。普通文件须命中[路径与类型白名单](../scripts/data_guard.py)，并通过内容检查。表中未列的组合须登记合成来源。此白名单由 web 检查器维护，共享规则文件仍使用 v1 格式。

| 路径 | 默认允许的文件 |
| --- | --- |
| `src/`、`tests/` | 递归允许 Rust 和目录 README。`tests/` 另允许 Python。 |
| `docs/`、`crates/finance-client/src/` | 分别递归允许 Markdown 和 Rust。 |
| `migrations/`、`config/`、`openapi/`、`.sqlx/` | 直接子文件分别允许 SQL、`.yaml`、JSON 和 `query-*.json`。另允许迁移和配置的 README 与 `openapi/finance.commit`。 |
| `scripts/`、`scripts/openapi/` | 直接子文件分别允许 Shell 与 Python、Java 与 patch。 |
| `.github/workflows/`、`.github/ISSUE_TEMPLATE/` | 直接子文件允许 YAML，issue 模板另允许 Markdown。PR 模板按确切路径放行。 |
| 根目录和其他工具文件 | 按确切路径登记，包括 README、RUNBOOK、AGENTS、Rust 构建清单、服务清单、生成客户端清单和钩子。 |

普通文件须为 UTF-8 文本，允许 BOM、制表符和换行。禁止路径、后缀、大小、Git 对象类型和魔数仍按硬规则检查。可识别的凭据、Cargo/libtest 文本或 JSON 报告、JUnit、异常转储及带时间戳和级别的日志均属硬拒绝项。报告和日志专用目录也被禁止。Markdown 代码块、引用及其他文本内嵌记录仍执行相同检查。

fixture、回放、原始 HTTP 转储和生成客户端样例须登记合成来源。内容检查识别行情表，以及 OHLC、报价、成交和盘口的字段组合与值，覆盖独立 JSON、嵌入文本和转义字符串。命名为源码的文件仍检查独立的数据行，普通实现中的常量和内联构造由代码审查核对。连续的 Rust 参数和元组不视为表格行。

## 合成来源

`sources.json` 的 `fixtures` 以精确相对路径为键。每项包含 `sha256`（原始文件字节的 64 位小写十六进制摘要）、`generator`（同一 Git 树中的生成器）、`seed`（固定生成可用 `fixed`）和非空 `source`（生成命令、方法和覆盖场景）。清单与生成器随样本一起提交，PR 审阅者按说明重建并核对。登记不能覆盖硬拒绝规则。真实样本经变换后仍属于原来源。

W3 的 [SMTP 测试源码](../src/notifications/channel/tests.rs) 已审查，固定值仅用于在临时目录构造合成凭据。检查器以该文件的确切路径和 W3 已合并源码的 SHA-256 固定这一凭据特征例外，并继续执行其他扫描。改动字节或复制到其他路径都会重新触发凭据检查。

当前没有独立持久化的行情 fixture。现有 API 与传输测试从源码常量构造带固定时钟、价格和数量的合成响应。WG 的防护测试以固定常量现场构造行情、违规正文、改名签名和报告，临时仓库中的正例由固定十进制字符串生成并核对原始字节摘要。会话和凭据测试只使用临时生成的随机令牌或测试密钥。测试运行于内存路由、临时 PostgreSQL 或本地 finance 桩，IBKR 保持 paper 只读，web 无下单入口。[API 测试](../tests/api/README.md) · [防护测试](../tests/test_data_guard.py)
