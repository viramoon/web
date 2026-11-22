# 调用 finance

Rust 客户端在 [crates/finance-client](../../crates/finance-client/Cargo.toml)。输入为 [finance OpenAPI 快照](../../openapi/finance.openapi.json)，[来源提交](../../openapi/finance.commit)记录 40 位 Git SHA。模型和端点由生成器写入。超时、幂等 GET 的有限重试、熔断等策略放在 web 的调用层。

在 web 仓库运行以下命令。需要 Bash、Git、curl、tar、unzip、patch、sha256sum、Java 21 JDK 和仓库锁定的 Rust/rustfmt。脚本默认将 OpenAPI Generator 7.27.0 缓存于 XDG_CACHE_HOME/openapi-generator（默认 ~/.cache/openapi-generator），每次执行都校验 SHA-256。离线环境可将 OPENAPI_GENERATOR_JAR 指向已取得的同版本 JAR。

```sh
# 直接从指定 finance 提交导出，并更新快照、来源记录和生成 crate。
scripts/gen-finance-client.sh from-finance ../finance "$(git -C ../finance rev-parse HEAD)"

# 根据 web 保存的快照再生成。只校验时用 --check。
scripts/gen-finance-client.sh from-snapshot
scripts/gen-finance-client.sh --check
cargo test --locked --test finance_client_wire

# 在同一组 fertile 指针下，核对总仓快照与 web 使用的快照、生成代码。
scripts/gen-finance-client.sh --check ../contracts/finance.openapi.json

# 从总仓的真实快照更新客户端，并记录该快照对应的 finance 子模块提交。
scripts/gen-finance-client.sh from-snapshot ../contracts/finance.openapi.json "$(git -C ../finance rev-parse HEAD)"
```

from-finance 从传入提交的 Git 对象生成临时源码树，执行 cargo run --locked -p finance-api -- export-openapi；工作区的未提交改动不会进入快照。--check 在临时目录独立生成并逐文件比对，外部快照与 web 记录不一致时也会失败。模板、数值映射和兼容性取舍见 [ADR-0002](../../docs/adr/ADR-0002-finance-client.md)。
