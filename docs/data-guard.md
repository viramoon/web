# 日志、样本与 CI 防护

v1 私有使用。付费行情、真实行情及其衍生样本、截图、数据库、日志导出和测试报告留在获准的私有环境。GitHub 仓库、PR、issue、LFS、CI 输出和 artifact 均在该边界之外。允许归档的数据及运行目录置于工作树之外，是否可以归档、计算和备份按各产品许可确定。

## 运行日志与审计

web 对标准错误输出 JSON。请求完成事件只记录 UUID 请求 ID、标准方法或 OTHER、匹配的路由模板或 <unmatched>、响应状态码和耗时。URI 参数、查询字符串、请求头、请求和响应正文均不写入日志。集成审计表只保存动作、集成及测试 ID、配置版本和审计时间。凭据正文只在校验及封装边界内处理。

启动、迁移及数据库异常使用固定阶段或有限错误类别，panic 使用固定事件。这些诊断省略底层异常正文、堆栈、SQL、配置值、令牌与上游响应原文。日志级别仍由 RUST_LOG 调整，事件目标限于 web 和 web-notifier 自身，两个进程共用日志过滤器。请求关联和故障定位见[运行手册](../RUNBOOK.md#故障定位与回滚)。

## 检查入口

| 入口 | 硬规则来源 | 检查范围 |
| --- | --- | --- |
| pre-commit | 本地 HEAD 已提交的版本 | 检查实际暂存 blob 和来源清单，部分暂存时以 Git 索引为准。 |
| pre-push | 远端跟踪默认分支，优先使用 `refs/remotes/<remote>/HEAD`，缺失时查 `refs/remotes/<remote>/main` | 检查每个非删除 ref 引入的提交。已有 ref 以 Git 提供的远端原 SHA 划定范围，新 ref 以远端跟踪默认分支划定范围。 |
| CI | PR 基分支 SHA。push 使用先前 SHA，新分支优先使用远端 main | 每个新提交与第一父提交比对，覆盖合并分支以及后来被删除的对象。 |

规则文件的候选改动仍执行 v1 格式校验。本次扫描始终使用入口选定的硬规则，待审规则放宽对同一次扫描中的文件和后续候选提交均不生效。来源登记继续按每个候选 Git 树读取，可随合成样本一起提交。

基线早于规则文件引入时，例如本 PR 的 W3 main，检查器从固定的已审查提交 `e663bbd3fe64abf75f62925f2170e2cb58506e4f` 读取初始 v1 规则。缺少该提交时返回 `missing-trusted-rules`。pre-push 缺少远端跟踪默认分支时返回 `missing-remote-default`，只删除 ref 的推送不需要读取规则。

Web service 的 data-guard 作业在完整检出后执行[共享检查入口](../scripts/ci-data-guard.sh)，通过后才运行 core。文档和客户端工作流通过可复用的 data-guard 作业调用同一入口。core 的 PostgreSQL 容器沿用 main 的密码认证和 `5432:5432` 映射，迁移和 SQLx 校验使用作业内的回环地址。可使用文档工作流已有的 PyYAML 运行 `python3 -B -m unittest discover -s tests -p test_ci_workflow.py -v`，检查作业结构和检查顺序。

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

fixture、回放、原始 HTTP 转储和生成客户端样例须登记合成来源。内容检查识别行情表，以及 OHLC、报价、成交和盘口的字段组合与值，覆盖独立 JSON、嵌入文本和转义字符串。Rust 源码使用相同的表格与行情特征检查。含空末列的 CSV 即使改名为 `.rs` 也会触发检查。合法源码命中特征时，须按确切路径与字节摘要登记，由代码审查核对来源。

## 合成来源

`sources.json` 的 `fixtures` 以精确相对路径为键。每项包含 `sha256`（原始文件字节的 64 位小写十六进制摘要）、`generator`（同一 Git 树中的生成器）、`seed`（固定生成可用 `fixed`）和非空 `source`（生成命令、方法和覆盖场景）。清单与生成器随样本一起提交，PR 审阅者按说明重建并核对。登记不能覆盖硬拒绝规则。真实样本经变换后仍属于原来源。

W3 与 W4 的 [SMTP 测试源码](../src/notifications/channel/tests.rs) 已审查，固定值仅用于在临时目录构造合成凭据。检查器以确切路径和已审查源码的 SHA-256 限定凭据特征例外，并继续执行其他扫描。改动字节或复制到其他路径都会重新触发凭据检查。

检查器还按路径和 SHA-256 固定 Rust 源文件的已审查版本，来源包括 W3 main 提交 `4c966be`、WG 的 `4992a15` 和 `7e641b6`，以及 W4 合并后的测试和通知代码。匹配内容包括策略整数边界、迁移编号、合成状态元组和 OHLC 脱敏探针。该清单只豁免对应字节的行情特征，凭据和其他硬拒绝规则仍执行。Web service 中从 `4c966be` 恢复的两行 CI 专用合成数据库配置单独按工作流路径及行摘要核对，值被改动或复制到其他文件时恢复凭据检查。

W4 在 WG 前的三个提交保留原历史。范围扫描对 `981dd24`、`e46a991` 和 `dce9c80` 中的特定对象，按提交、路径、SHA-256 和命中的检查逐项核对。这些记录覆盖手工构造的合成 CSV、SMTP 测试凭据、迁移及通知源码的表格误报，以及只归档确定性 OpenAPI 的旧工作流。当前工作流仅在临时目录导出和比对。历史记录只作用于范围和推送扫描中的非末端提交，其余检查继续运行。暂存内容、扫描末端及新提交中的复制对象继续使用正常规则。

PR 作业以目标分支为合并提交的第一父。W4 研究测试在运行时构造固定列、日期和十进制文本的 CSV，通知测试按顺序调用规则更新。数据防护回归将这两份实际源码放入同形态的本机合并树，并以末端规则扫描目标分支到该树的差异。

当前没有独立持久化的行情 fixture。现有 API 与传输测试从源码常量构造带固定时钟、价格和数量的合成响应。WG 的防护测试以固定常量现场构造行情、违规正文、改名签名和报告，临时仓库中的正例由固定十进制字符串生成并核对原始字节摘要。会话和凭据测试只使用临时生成的随机令牌或测试密钥。测试运行于内存路由、临时 PostgreSQL 或本地 finance 桩，IBKR 保持 paper 只读，web 无下单入口。[API 测试](../tests/api/README.md) · [防护测试](../tests/test_data_guard.py)
