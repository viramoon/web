# 采集缺口的本机快照

`web publish-quality` 将 finance F3 本机 WAL 工具输出汇成 `GET /api/v1/quality/gaps` 读取的快照。部署使用同一个 web 用户执行一次性命令，并为命令与 HTTP 进程设置相同的 `WEB_QUALITY_STATUS_FILE`。快照路径位于静态根之外，父目录须预先创建。定期采样由 fertile 为已授权数据安排。

部署提供受控的本机 JSON 文件。工具和数据根使用绝对路径。每一项的 `license` 使用该产品经过审核的 F1 许可标识。列表支持最多 64 对无重复的源和产品。

```json
{
  "wal_bin": "/opt/finance/bin/finance-market-wal",
  "data_root": "/path/to/authorized-wal",
  "streams": [
    {"source": "fixture", "product": "trades", "license": "synthetic"}
  ]
}
```

```sh
WEB_QUALITY_STATUS_FILE=/path/to/private/quality-status.json \
    web publish-quality /path/to/private/quality-inputs.json
```

发布器先通过 F3 `products` 获取可读持久产品，再对与审核列表重合的项调用 `quality` 和 `watermarks`。发布时校验结构、缺口区间及水位，保留精确的十进制字符串。交集为空时发布空快照。撤下产品后重新发布即可清除其条目，空审核列表可直接发布空快照。

每次采样限时 30 秒。成功时以 0600 权限原子替换快照，`observed_at` 记录本次采样完成时间。已有合格快照在采样失败、无效输出或超限时继续可读。错误只报告固定原因。查询响应的 `as_of` 可用于识别尚未刷新的快照。

接口字段与页间一致性约定见[接口文档](api.md#备份与采集质量)。测试以合成 F3 命令输出覆盖发布和 HTTP 读取。执行 `cargo test --locked --offline --test quality_publish`。
