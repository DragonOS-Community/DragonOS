# ocm-agent DragonOS 端到端验收报告

- 生成时间：2026-10-25 20:55:03
- 镜像：`/home/dragonos/DragonOS-ocm-agent/bin/disk-image-x86_64.img`
- gateway：TLS 1.3 mock（协议兼容，非生产 gateway）

## 1. 连接与注册

| 项 | 值 |
|---|---|
| 连接 | 127.0.0.1:17958 TLS1.3 CN=`` |
| 连接 | 127.0.0.1:17968 TLS1.3 CN=`1001` |
| REGISTER | uuid=`0123456789abcdef0123456789abcdef` ip=`10.0.2.15` mac=`52:54:00:12:34:56` |
| 下发 agent_id | `1001`（客户端证书 CN=`1001`）|

## 2. 帧与 ACK

| 指标 | 值 |
|---|---|
| DATA_REQ_NEW | 1 |
| DATA_SMALL | 11 |
| DATA_UPLOAD_FRAG | 4 |
| HEARTBEAT | 13 |
| REGISTER | 1 |
| DATA_SMALL payload bytes | 4776 |
| ACK ok / fail | 11 / 0 |
| agent sent / ack | 10 / 10 |

## 3. 采集插件

| 插件 | 版本 | 帧 | 压缩字节 | 指标数 | 指标名数 |
|---|---|---:|---:|---:|---:|
| large_test | v-synthetic | 1 | 122493 | 832 | 1 |
| os_monitor | v1.0.0 | 10 | 3311 | 154 | 15 |
| sysinfo_collector | v0.0.1 | 1 | 1465 | 53 | 29 |

### os_monitor

属性：`uuid=0123456789abcdef0123456789abcdef`

分组：`(unnamed)`×154

| 指标 | 次数 | 最后值 | 标签 |
|---|---:|---|---|
| `cpu_load` | 30 | 0.98 | period=15 |
| `mem_used` | 10 | 79380 |  |
| `mem_used_app` | 10 | 2065672 |  |
| `cpu_usage` | 9 | 0.4170141784820629 | cpu_id=all |
| `guest_nice_usage` | 9 | 0 | cpu_id=all |
| `guest_usage` | 9 | 0 | cpu_id=all |
| `idle_usage` | 9 | 99.58298582151794 | cpu_id=all |
| `iowait_usage` | 9 | 0 | cpu_id=all |
| `irq_usage` | 9 | 0 | cpu_id=all |
| `nice_usage` | 9 | 0 | cpu_id=all |
| `softirq_usage` | 9 | 0.16680567139282734 | cpu_id=all |
| `steal_usage` | 9 | 0 | cpu_id=all |
| `system_usage` | 9 | 0.16680567139282734 | cpu_id=all |
| `user_usage` | 9 | 0.08340283569641367 | cpu_id=all |
| `disk_usage` | 5 | 20.77917458711572 | name=ext4 |

### sysinfo_collector

属性：`uuid=0123456789abcdef0123456789abcdef`

分组：`base_info`×12, `modules`×0, `users`×1, `systemd`×1, `system_args`×21, `(unnamed)`×0, `log`×1, `bios`×0, `baseboard`×0, `pci`×0, `cpu`×10, `memory`×1, `disk`×4, `net`×2

| 指标 | 次数 | 最后值 | 标签 |
|---|---:|---|---|
| `system_arg` | 21 | kernel.keys.root_maxkeys | val=1000000 |
| `mount_point` | 4 | /run | available=1057611776, device=tmpfs, filesystem=tmpfs, total=1057624064, used=... |
| `net_device` | 2 | lo | ip=, mac=00-00-00-00-00-00, rcv_bytes=0, rcv_drops=0, rcv_packets=0, snd_byte... |
| `agent_ver` | 1 | 0.1.0 |  |
| `arch` | 1 | x86_64 |  |
| `cmd_line` | 1 | console=/dev/hvc0  rw  init=/bin/busybox init AUTO_TEST=none SYSCALL_... |  |
| `core_per_socket` | 1 | 0 |  |
| `cpus` | 1 | 0 |  |
| `dmesg` | 1 | exit status 1 |  |
| `dns` | 1 | 10.0.2.3 | type=nameserver |
| `family` | 1 | 6 |  |
| `grub` | 1 |  |  |
| `host_name` | 1 | dragonos |  |
| `kdump_status` | 1 | 2 |  |
| `kernel` | 1 | 6.6.21-dragonos |  |
| `machine_model` | 1 |  |  |
| `mem_total` | 1 | 2115248128 | unit=byte |
| `mhz` | 1 | 0.000 |  |
| `model` | 1 | Intel(R) Core(TM) i5-8500 CPU @ 3.00GHz |  |
| `os_name` | 1 | Ubuntu 24.04.5 LTS |  |
| `server_type` | 1 | docker |  |
| `sockets` | 1 | 0 |  |
| `systemd_version` | 1 |  |  |
| `threads_per_core` | 1 | 0 |  |
| `uptime` | 1 | 1.56 | unit=second |
| `user` | 1 | root | group=root, last_login=**Never logged in**, login_from= |
| `uuid` | 1 |  |  |
| `vendor` | 1 | GenuineIntel |  |
| `virtualization` | 1 | full |  |

## 4. 大数据拉取链路（合成 120KB 载荷）

| task_id | data_id | 总大小 | 分片 | 插件 | 指标数 |
|---:|---:|---:|---:|---|---:|
| 1 | 10001 | 122493 | 4 | large_test | 832 |

## 5. DragonOS 运行效果

```text
### uname
Linux dragonos 6.6.21-dragonos #5-dragonos-0.4.0 Sun Oct 25 12:51:13 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
### uptime
60.96 119.09
### loadavg
10.76 3.10 1.07 1/52 278
### meminfo
MemTotal:	2065672 kB
MemFree:	1985828 kB
Buffers:	0 kB
Cached:		22908 kB
SwapTotal:	0 kB
SwapFree:	0 kB
Dirty:		48 kB
Writeback:	0 kB
### ps
  PID TTY          TIME CMD
  139 hvc0     00:00:00 dash
  220 ?        00:00:00 ocm-agent
  231 ?        00:00:00 os_monitor
  275 ?        00:00:00 dash
  280 ?        00:00:00 ps
### agent pid=220
Name:	ocm-agent
State:	Blocked(true)
VmPeak:	1254620 kB
VmSize:	1254620 kB
### agent statm
313655 2838 0 0 0 0 0
### agent tasks
11
### df
Filesystem       1-blocks      Used  Available Capacity Mounted on
ext4           2072879104 430751744 1534808064      22% /
### plugin dirs
/opt/ocm-agent/plugins/os_monitor/:
total 0
-rw-r--r-- 1 root root     224 Oct 25 12:53 app.conf.yml
-rwxr-xr-x 1 root root 5075128 Oct 25 12:53 os_monitor

/opt/ocm-agent/plugins/sysinfo_collector/:
total 0
-rwxr-xr-x 1 root root 4874424 Oct 25 12:53 sysinfo_collector
### data dir
total 0
-rw------- 1 root root  1147 Oct 25 20:54 ca.crt
-rw------- 1 root root  1119 Oct 25 20:54 client.crt
-rw------- 1 root root  1704 Oct 25 20:54 client.key
-rw-r--r-- 1 root root  4096 Oct 25 20:54 ongoing.db
-rw-r--r-- 1 root root 32768 Oct 25 20:54 ongoing.db-shm
-rw-r--r-- 1 root root 41232 Oct 25 20:54 ongoing.db-wal
### log tail
{"level":"info","time":"2026-10-25T20:54:00.174Z","msg":"starting ocm-agent","module":"agent","version":"0.1.0 (commit: d08b700, built: 2026-10-25T12:53:57Z)"}
{"level":"info","time":"2026-10-25T20:54:00.203Z","msg":"connected to gateway","module":"agent","addr":"10.0.2.2:15163"}
{"level":"info","time":"2026-10-25T20:54:00.205Z","msg":"fingerprint detected","module":"agent","iface":"eth2","ip":"10.0.2.15","ipv6":"","mac":"52:54:00:12:34:56"}
{"level":"info","time":"2026-10-25T20:54:00.231Z","msg":"registration successful","module":"agent","agent_id":1001}
{"level":"info","time":"2026-10-25T20:54:00.241Z","msg":"connected to gateway","module":"agent","addr":"10.0.2.2:15163"}
{"level":"info","time":"2026-10-25T20:54:00.391Z","msg":"taskrunner workdir cleaner started","module":"agent","tasks_dir":"/opt/ocm-agent/data/tasks","ttl_sec":3600,"interval_sec":600}
{"level":"info","time":"2026-10-25T20:54:00.395Z","msg":"starting connection epoch","module":"agent","epoch":1,"agent_id":1001}
{"level":"info","time":"2026-10-25T20:54:00.432Z","msg":"Unix socket server started","module":"agent","path":"/opt/ocm-agent/var/chan.sock"}
{"level":"warn","time":"2026-10-25T20:54:00.445Z","msg":"chattr +i failed (may need root)","module":"agent","dir":"/opt/ocm-agent","error":"exit status 1"}
{"level":"info","time":"2026-10-25T20:54:00.445Z","msg":"agent running","module":"agent","agent_id":1001}
{"level":"info","time":"2026-10-25T20:54:00.460Z","msg":"Loaded plugin","module":"agent","plugin":"os_monitor","cron":"*/6 * * * *","enable":true,"run_on_boot":true}
{"level":"info","time":"2026-10-25T20:54:00.466Z","msg":"Loaded plugin","module":"agent","plugin":"sysinfo_collector","cron":"1 13 18 * * ?","enable":true,"run_on_boot":true}
{"level":"info","time":"2026-10-25T20:54:00.469Z","msg":"builtin collector disabled, skipping start","module":"agent"}
{"level":"info","time":"2026-10-25T20:54:00.470Z","msg":"epoch components started","module":"agent","epoch":1}
{"level":"info","time":"2026-10-25T20:54:00.707Z","msg":"Received message","module":"agent","plugin":"sysinfo_collector","data_len":1465}
{"level":"info","time":"2026-10-25T20:54:05.699Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":198}
{"level":"info","time":"2026-10-25T20:54:10.448Z","msg":"small-data metrics","module":"agent","sent_total":2,"ack_success_total":2,"ack_fail_total":0,"retry_total":0,"drop_total":0,"queue_drop_total":0,"queue_depth":0}
{"level":"info","time":"2026-10-25T20:54:10.637Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":348}
{"level":"info","time":"2026-10-25T20:54:20.415Z","msg":"Received message","module":"agent","plugin":"large_test","data_len":122493}
{"level":"info","time":"2026-10-25T20:54:20.448Z","msg":"small-data metrics","module":"agent","sent_total":3,"ack_success_total":3,"ack_fail_total":0,"retry_total":0,"drop_total":0,"queue_drop_total":0,"queue_depth":0}
{"level":"info","time":"2026-10-25T20:54:20.642Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":367}
{"level":"info","time":"2026-10-25T20:54:25.653Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":330}
{"level":"info","time":"2026-10-25T20:54:30.448Z","msg":"small-data metrics","module":"agent","sent_total":5,"ack_success_total":5,"ack_fail_total":0,"retry_total":0,"drop_total":0,"queue_drop_total":0,"queue_depth":0}
{"level":"info","time":"2026-10-25T20:54:30.634Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":353}
{"level":"info","time":"2026-10-25T20:54:35.630Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":322}
{"level":"info","time":"2026-10-25T20:54:40.448Z","msg":"small-data metrics","module":"agent","sent_total":7,"ack_success_total":7,"ack_fail_total":0,"retry_total":0,"drop_total":0,"queue_drop_total":0,"queue_depth":0}
{"level":"info","time":"2026-10-25T20:54:40.650Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":364}
{"level":"info","time":"2026-10-25T20:54:50.448Z","msg":"small-data metrics","module":"agent","sent_total":8,"ack_success_total":8,"ack_fail_total":0,"retry_total":0,"drop_total":0,"queue_drop_total":0,"queue_depth":0}
{"level":"info","time":"2026-10-25T20:54:50.631Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":340}
{"level":"info","time":"2026-10-25T20:54:55.631Z","msg":"Received message","module":"agent","plugin":"os_monitor","data_len":355}
```

## 6. Agent 日志摘要

- 已加载插件：os_monitor, sysinfo_collector
- error 条数：0
- warn 条数：1
  - `chattr +i failed (may need root)`: exit status 1

> 说明：`large_test` / `synthetic_large_metric` 是协议健壮性测试注入的合成数据，不是真实采集结果。
