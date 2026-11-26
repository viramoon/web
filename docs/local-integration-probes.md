# 策略、研究文件与本地备份探测

集成的 `settings.profile` 选择部署槽位 `primary` 或 `secondary`。浏览器只能提交槽位名。文件、工具及测试 remote 路径由部署提供，填写绝对路径，并将文件和 remote 置于静态目录之外。保留已测试和活动槽位的原始配置及内容，变更时在另一槽位准备新候选，然后测试和启用。

| 环境变量 | 配置项 | 默认值与用途 |
| --- | --- | --- |
| `WEB_STRATEGY_COMPILER` | `local_probes.strategy_compiler` | 空。部署同版本的 finance F2 `finance-strategy` 二进制 |
| `WEB_RESEARCH_PYTHON` | `local_probes.research_python` | 空。使用已安装对应 finance Python 包及研究依赖的解释器 |
| `WEB_TEST_RCLONE` | `local_probes.rclone` | `/usr/bin/rclone`。仅在配置本地测试目录后使用 |
| `WEB_INTEGRATION_PRIMARY_STRATEGY_FILE` | `local_probes.primary.strategy_file` | 空。预置的策略文件 |
| `WEB_INTEGRATION_PRIMARY_RESEARCH_MANIFEST` | `local_probes.primary.research_manifest` | 空。预置的研究文件清单 |
| `WEB_INTEGRATION_PRIMARY_DRIVE_TEST_ROOT` | `local_probes.primary.drive_test_root` | 空。web 运行用户所有的专用本地测试目录，权限 `0700` |

第二槽位使用 `WEB_INTEGRATION_SECONDARY_*` 和 `local_probes.secondary.*`。探测未配置的槽位会返回 `not_configured`。启动 web 只检查配置，探测在管理员发起候选测试时执行。

## 策略和研究

策略文件支持 YAML、YML 和 JSON，上限 256 KiB。同一批读取的字节交给 `finance-strategy compile` 的标准输入，并用编译器返回的原文 SHA-256 复核。成功证据包含执行模型、计划版本、哈希及因子、模块、子组合计数。编译器诊断只映射为有限失败状态，配置全文、内部 ID 和诊断原文留在本机。

研究清单上限 16 KiB，其同目录的 `bars.csv` 上限 8 MiB。以既有 finance 研究导入器的 `dataset.json` 为 `dataset` 对象，声明 `license` 为 `synthetic` 或 `private_research`。两种用途分别对应 `data_kind` 的 `fixture` 和 `historical`。当前接入 XNYS 股票和 ETF，要求 `price_basis` 为 `raw_unadjusted`，`corporate_actions` 为 `cash_dividends_excluded_split_free_reviewed`。字段包括 `instrument`、`asset_type`、`source`、`source_sha256`、`data_kind`、`calendar`、`first_session`、`last_session`、`observations`、`price_basis` 和 `corporate_actions`。

web 验证清单和 CSV 哈希，将同一批字节交给 finance 研究导入器，在 `/tmp` 中使用私有的临时目录预演。导入器校验列、精确 USD 价格和整股成交量、实际交易日及时间，并在临时目录原子发布。web 用清单复核结果和落盘元数据后清理临时目录，接口只返回标的、日历、覆盖、许可与哈希。正式研究导入由 finance 管理。

## Google Drive 配置占位

测试目录配置完成后，`google_drive` 卡的候选测试可运行本地加密往返。web 为本次测试在内存中生成独立的临时密码、盐和合成对象，使用 rclone 把对象经 `crypt` 写入一次性本地目录。检查底层对象的加密名称、密文与解密后的内容，随后删除对象并清理目录。成功证据为 `local_test`，包含本地可用字节数、往返结果和清理结果。测试输出只保留类型化证据，不记录密码、盐、文件名、临时路径、对象内容或 rclone 输出。

Drive 账户、OAuth 和备份密钥由 vira 后续决定。真实备份配置和调度归 fertile T2b 管理，备份水位以其状态文件为准。

## 本地验证

`cargo test --lib --locked --offline integrations::local_probe` 运行合成策略与 CSV、导入器替身、路径校验和 rclone 输出屏蔽测试。另用现有的本机 rclone 验证名称及内容加密、读回和清理。

```sh
WEB_TEST_RCLONE=/absolute/path/to/rclone \
  cargo test --lib --locked --offline rclone_local_crypt_round_trip_uses_real_ciphertext_and_cleans_up -- --ignored
```

后台调度和水位接口见[运行手册](../RUNBOOK.md)，测试、启用和回滚请求见[接口文档](api.md)。
