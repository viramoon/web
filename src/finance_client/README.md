# 调用 finance

Rust 客户端在 [crates/finance-client](../../crates/finance-client/Cargo.toml)。输入为 [finance OpenAPI 快照](../../openapi/finance.openapi.json)，[来源提交](../../openapi/finance.commit)记录 40 位 Git SHA。模型和端点由生成器写入。超时、幂等 GET 的有限重试、熔断等策略放在 web 的调用层。同一提交的六个 `openapi/finance.*` 读流 JSON 由 finance 导出，涵盖现有行情和 F16 期权流，供契约比对和合成回放测试使用。[读流产物和契约差异](../../docs/api.md#finance-契约版本)

在 web 仓库运行以下命令。需要 Bash、Git、curl、tar、unzip、patch、sha256sum、Java 21 JDK 和仓库锁定的 Rust/rustfmt。脚本默认将 OpenAPI Generator 7.27.0 缓存于 XDG_CACHE_HOME/openapi-generator（默认 ~/.cache/openapi-generator），每次执行都校验 SHA-256。离线环境可将 OPENAPI_GENERATOR_JAR 指向已取得的同版本 JAR。

[Finance client generation 工作流](../../.github/workflows/finance-client.yml)从生成脚本读取版本、SHA-256 和 Maven 下载地址，并按版本和 SHA-256 缓存 runner 临时目录中的单个公开 JAR。命中时直接校验本地 JAR。未命中时，curl 最多重试 3 次，默认按指数退避，单次传输最长 30 秒，重试窗口为 90 秒。下载内容通过 SHA-256 校验后才移至缓存路径，作业成功后保存缓存。校验失败时作业停止，缓存损坏时可删除对应键后重跑。生成步骤通过 `OPENAPI_GENERATOR_JAR` 使用该文件，仍由脚本再次校验并运行 `--check`。

```sh
# 直接从指定 finance 提交导出 HTTP 与读流快照，并更新来源记录和生成 crate。
scripts/gen-finance-client.sh from-finance ../finance "$(git -C ../finance rev-parse HEAD)"
scripts/gen-finance-client.sh --check-from-finance ../finance

# 根据 web 保存的快照再生成。只校验时用 --check。
scripts/gen-finance-client.sh from-snapshot
scripts/gen-finance-client.sh --check
cargo test --locked --test finance_client_wire

# 在同一组 fertile 指针下核对 HTTP 快照、生成代码。可追加读流导出目录核对六份读流产物。
scripts/gen-finance-client.sh --check ../contracts/finance.openapi.json

# 用总仓 HTTP 快照和同一提交的 read-stream 导出目录更新客户端及来源提交。
scripts/gen-finance-client.sh from-snapshot ../contracts/finance.openapi.json \
  "$(git -C ../finance rev-parse HEAD)" /tmp/finance-read-stream
```

`from-finance` 和 `--check-from-finance` 从指定或已记录的 Git 提交解出临时源码树。两者用 `cargo run --locked -p finance-api` 执行 `export-openapi` 和 `export-read-stream DIRECTORY`。离线使用已缓存的依赖并设置 `CARGO_NET_OFFLINE=true`。`--check` 在临时目录从已记录的 HTTP 快照独立生成客户端并逐文件比对，也检查读流产物齐全。提供外部 HTTP 快照或读流目录时，逐字节比对相应文件。F10c、F10d 的展示读取和 F16 恢复快照由生成器保留查询参数和错误类型，并提供泛型 JSON 解码。web 选用原始 JSON 解码并检查许可边界，以保留 finance 的缺值、未知枚举值和精确文本。F16 的 WS 和 SSE 操作生成请求构造器，分别保留游标查询参数和 `Last-Event-ID`。web 管理上游升级、连接寿命和流式响应。模板、数值映射和兼容性取舍见 [ADR-0002](../../docs/adr/ADR-0002-finance-client.md)。
