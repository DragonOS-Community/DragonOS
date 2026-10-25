# 2026-10-25 端到端验收原始产物

由以下命令生成（DragonOS 内核 `6.6.21-dragonos #5`，ocm-agent `d08b700`）：

```bash
python3 user/apps/ocm-agent/scripts/dragonos-acceptance.py --duration 60
```

| 文件 | 说明 |
|---|---|
| `summary.md` | 验收报告（注册、帧、插件指标、运行效果） |
| `report.json` | gateway 解码后的完整数据（指标名/值/标签/分组） |
| `frames.ndjson` | 帧时间线（类型/seq/大小） |
| `agent.log` | guest 内 agent 日志 |
| `runtime-stats.txt` | guest 内 `ps`/`/proc`/`df` 快照 |
| `gateway.log` | mock gateway 日志 |
| `qemu-transcript.log` | QEMU 串口交互记录 |
| `config.yaml` | 本次 agent 配置（端口为一次性端口） |
| `os_monitor.conf.yml` | 本次 os_monitor 采集配置 |
| `acceptance-*.sh` | guest 内启动/大数据注入/统计脚本 |

说明：

- 本目录不包含 `certs/`（一次性 CA/客户端私钥），需要时由验收脚本现场生成。
- 本次运行使用协议兼容的 mock gateway，不是生产 gateway 服务。
- `large_test` / `synthetic_large_metric` 是协议健壮性测试注入的合成数据。
