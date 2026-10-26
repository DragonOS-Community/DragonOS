# ocm-agent DragonOS 移植说明

ocm-agent 源码**不保存在本仓库**。DADK 通过 git source 直接引用
<https://github.com/sparkzky/ocm-agent>（上游为
<https://gitee.com/OpenCloudOS/ocm-agent>），固定 commit
`d08b700e20243cee892ef67b975f89500f352c4b`（`dragonos` 分支）；Nix 用 `fetchgit`
引用同一 revision。该分支的 `master` 是上游的纯净镜像；`dragonos` 仅包含两个
可移植性修复（见 §7）。本目录只保存 DragonOS 的构建、打包、Nix 集成与测试文件。

## 1. 结论

在 `feat/ocm-agent` 分支对应的 DragonOS 内核上，ocm-agent 的端到端流程已经跑通：

- 静态 musl 二进制可直接执行（Go 1.25 runtime + cgo/sqlite）；
- `TCP + TLS 1.3 + mTLS` 连接、首次注册、证书签发与落盘、mTLS 重连；
- 首次注册分支的主机指纹探测（netlink 网卡枚举）正常；
- SQLite WAL（`ongoing.db`）在 ext4 rootfs 上正常创建/写入；
- AF_UNIX 数据通道（`/opt/ocm-agent/var/chan.sock`）可监听；
- 心跳、小数据 ACK、大数据分片拉取、断线重连按上游逻辑工作；
- 插件管理器可以拉起外部进程（`execve` + 进程组）；
- `os_monitor`、`sysinfo_collector` 的采集结果可以在 gateway 侧解码。

内核侧**不需要新增 syscall**，但 `sysinfo_collector` 依赖 gopsutil 的
`host.Info()`，后者需要 `/proc/uptime`；DragonOS 原先没有该文件，因此本移植在
procfs 中新增了 `/proc/uptime`（格式与 Linux 一致：uptime + 各 CPU idle 总和；PR
[#2565](https://github.com/DragonOS-Community/DragonOS/pull/2565)）。

## 2. 为什么必须静态链接

DragonOS 的 Nix rootfs 只包含 BusyBox 等静态工具，不保证存在 glibc/musl 动态加载器。
上游默认构建产物是动态链接的，复制进镜像后无法启动。

`github.com/mattn/go-sqlite3` 使用 cgo，因此不能简单地 `CGO_ENABLED=0`。本移植使用
musl 工具链并显式静态链接：

```bash
CGO_ENABLED=1 CC=musl-gcc go build \
  -trimpath -buildvcs=false \
  -ldflags '-s -w -linkmode external -extldflags "-static"' \
  -o bin/ocm-agent ./cmd/ocm-agent
```

`user/apps/ocm-agent/Makefile` 封装了上述命令，并支持 `ARCH=x86_64|riscv64`
（riscv64 需要 `riscv64-linux-musl-gcc`）。

## 3. 构建与打包

### 3.1 Make / DADK 流程

```bash
# 本地直接编译（可选）：把上游克隆到未跟踪的 upstream/
make -C user/apps/ocm-agent fetch-src

# 只编译本程序
make -C user/apps/ocm-agent

# 由 DADK 安装到 bin/sysroot/opt/ocm-agent/
# DADK 会按 ocm_agent.toml 的 git source + revision 自动克隆
make user
```

本地源码缺失时 Makefile 会提示执行 `make -C user/apps/ocm-agent fetch-src`。

DADK 首次构建会按 `source-path` 克隆到 `bin/dadk_cache/source/ocm_agent_0_1_0`。
若宿主机访问 GitHub 受限，可预先准备同 commit 的本地镜像，或临时把
`source-path` 指向本地裸仓库；`revision` 保证构建内容不变。

DADK 配置：`user/dadk/config/all/ocm_agent.toml`，已链接进
`sets/default` 与 `sets/ubuntu2404`。安装布局与上游一致：

```text
/opt/ocm-agent/ocm-agent
/opt/ocm-agent/config.yaml.example
/opt/ocm-agent/data/          # ca.crt / client.crt / client.key / ongoing.db
/opt/ocm-agent/plugin-conf/
/opt/ocm-agent/var/           # chan.sock
```

若主机没有 Go 工具链，`make install-dadk` 会像 `go_demo` 一样跳过而不是失败。

随仓库分发的两个采集插件（`os_monitor`、`sysinfo_collector`）**不参与默认安装**。
它们可以单独构建为静态二进制：

```bash
make -C user/apps/ocm-agent plugins          # 产物在 bin/plugins、bin/plugin-conf
make -C user/apps/ocm-agent install-plugins  # 安装到 $(INSTALL_DIR)/opt/ocm-agent
```

是否启用取决于 guest 对 Linux `/proc`、`/sys` 和外部工具的兼容程度。

### 3.2 Nix rootfs 流程

`user/apps/ocm-agent/default.nix` 用 `fetchgit` 按 commit + NAR hash 拉取同一个
GitHub 镜像，再交给 `pkgsStatic.buildGoModule`（`CGO_ENABLED=1`），并通过
`user/apps/default.nix` 加入 x86_64 rootfs。模块依赖由 `vendorHash` 固定，构建过程
不依赖宿主 Go，也不依赖本地源码缓存。

升级上游时需要同时更新两处：`user/dadk/config/all/ocm_agent.toml` 的 `revision`
（make/DADK 流程）和 `default.nix` 的 `rev`/`hash`（Nix 流程）。

## 4. 在 DragonOS 中运行

```bash
# 编译内核、用户态、写镜像并进入 DragonOS
make run-nographic
```

进入 guest 后（只使用已有镜像可直接 `make qemu-nographic`）：

```sh
cp /opt/ocm-agent/config.yaml.example /opt/ocm-agent/config.yaml
# 修改 gateway_addrs 为控制平面地址
# 将 ca.crt / client.crt / client.key 放到 /opt/ocm-agent/data/
/opt/ocm-agent/ocm-agent -config /opt/ocm-agent/config.yaml
```

DragonOS rootfs 没有 systemd，因此不会自动拉起 agent；需要由 init 脚本或手工执行。
QEMU 用户态网络下，宿主机地址为 `10.0.2.2`。

## 5. 冒烟测试

`scripts/dragonos-smoke.py` 会：

1. 生成一次性 CA / 服务端证书 / `CN=123` 客户端证书；
2. 挂载 `bin/disk-image-x86_64.img`（ext4，偏移 1 MiB），注入配置、证书和启动脚本；
3. 在宿主机启动一个 TLS 1.3 sink 扮演 gateway；
4. 启动 DragonOS，执行 agent，等待心跳帧；
5. 关闭虚拟机后从镜像中读回 `/opt/ocm-agent/smoke.log` 并断言。

```bash
# 先准备镜像：make user && SKIP_GRUB=1 make write_diskimage
OCM_SMOKE_RUNNER="make qemu-nographic" \
  python3 user/apps/ocm-agent/scripts/dragonos-smoke.py                 # 已注册（mTLS）+ 插件 exec
OCM_SMOKE_RUNNER="make qemu-nographic" \
  python3 user/apps/ocm-agent/scripts/dragonos-smoke.py --mode register # 首次注册分支
make -C user/apps/ocm-agent plugins
OCM_SMOKE_RUNNER="make qemu-nographic" OCM_SMOKE_SECONDS=24 \
  python3 user/apps/ocm-agent/scripts/dragonos-smoke.py --mode plugins  # os_monitor 数据链路
```

`OCM_SMOKE_RUNNER` 默认是 `nix run .#start-x86_64`；镜像由 `make` 流程生成时使用
`make qemu-nographic`。`OCM_SMOKE_SECONDS` 控制 guest 内 agent 的运行时长（默认 12 秒，
插件推送需要 24 秒）。

### 5.2 端到端功能验收

完整的前置条件、运行步骤、引用关系与验收结果见
[ACCEPTANCE.md](./ACCEPTANCE.md)。下面只列关键命令。

`scripts/dragonos-acceptance.py` 会启动一个协议兼容的 mock gateway（TLS 1.3 +
REGISTER/证书签发 + mTLS + DATA_SMALL ACK + 大数据分片），在 guest 中运行
`ocm-agent`、`os_monitor` 与 `sysinfo_collector`，并解码所有上传数据：

```bash
python3 user/apps/ocm-agent/scripts/dragonos-acceptance.py --duration 60
```

产物在 `bin/ocm-acceptance/latest/`：

| 文件 | 内容 |
|---|---|
| `summary.md` | 人类可读的验收报告（注册、帧、插件指标、运行效果） |
| `report.json` | 解码后的完整数据（指标名、数值、标签、分组、属性） |
| `frames.ndjson` | 每一帧的类型/序号/大小时间线 |
| `agent.log` | guest 内 agent 日志 |
| `runtime-stats.txt` | guest 内 `ps`、`/proc`、`df` 快照 |
| `qemu-transcript.log` | 串口交互记录 |

该脚本需要宿主机 Go（构建 gateway）、免密 `sudo`（loop 挂载镜像）和 `python-pexpect`。

脚本需要宿主机具备 `openssl`、`python-pexpect`，以及免密 `sudo`（用于 loop 挂载镜像）。

### 已实测结果

| 项目 | 结果 |
|---|---|
| TLS | `TLSv1.3` 握手成功，gateway 收到 HEARTBEAT 帧 |
| 身份 | 从 `client.crt` 的 CN 恢复 `agent_id=123` |
| 首次注册 | `fingerprint detected iface=eth2 ip=10.0.2.15`，REGISTER 帧已发出 |
| SQLite | `ongoing.db`、`-wal`、`-shm` 正常创建 |
| Unix socket | `/opt/ocm-agent/var/chan.sock` 监听成功 |
| 插件进程 | `run_on_boot` 的测试插件被执行（`plugin-ran` 落盘） |
| 插件数据链路 | `os_monitor` 采集 → Unix socket → agent → TLS `DATA_SMALL`（单次 190–363 字节，gateway 共收到 2757 字节） |
| 断线 | gateway 关闭后触发 reconnect，指数退避 |

## 6. 已知限制

- **chattr +i 自保护**：DragonOS 尚未实现 `FS_IOC_SETFLAGS`，`protect.Protect` 会打印
  `chattr +i failed` 警告后继续运行，不影响主流程。
- **cgroup 自限**：`cgroup.enabled` 在 DragonOS 上未验证，默认配置保持关闭。
- **systemd**：镜像没有 systemd，升级后由 systemd 拉起新进程的语义不适用；热升级
  只完成文件替换，需要外部 supervisor。
- **插件**：`os_monitor`、`sysinfo_collector` 已能静态构建（`make plugins`），但默认
  不安装。两者均已在 DragonOS 上实测，采集与上报链路正常。受 DragonOS 当前的
  procfs/sysfs 覆盖范围限制：
  - `os_monitor` 的 CPU/负载/内存/`df` 磁盘使用率可用；但缺少 `/proc/diskstats`
    （磁盘 IO 利用率不可用）和 `/sys/class/net/*/statistics/`（网络流量不可用）。
  - `/proc/meminfo` 目前没有 `MemAvailable`，且 `Buffers` 恒为 0，因此
    `mem_used_app = MemTotal - MemAvailable` 会退化为 `MemTotal`；`mem_used`
    仍然正确。
  - `sysinfo_collector` 的 `host_name`、`kernel`、`model`、`mem_total`、`dns`、`users`、
    `cpu`、`memory`、`disk`、`net`、`system_args` 等可用；DMI 型号/UUID、
    RPM、systemd、PCI、`dmesg` 等在精简 rootfs 中为空或不可用。
  `plugin-conf` 默认为空，核心 agent 不依赖插件。
- **主机 UUID**：`/sys/class/dmi/id/product_uuid` 不存在，回退 `/etc/machine-id`；
  两者都缺失时上游会使用 `"unknown"`，首次注册需要控制面容忍该值。
- **控制台长命令**：DragonOS `hvc0` 控制台对超长输入行会截断/丢字符，冒烟脚本因此
  通过短命令执行镜像内脚本。这是控制台输入路径的独立问题，不影响 agent。

## 7. 本移植涉及的仓库改动

- `kernel/src/filesystem/procfs/uptime.rs`：新增 `/proc/uptime`，并在
  `procfs/mod.rs`、`procfs/root.rs` 注册。注意该文件**不追加尾随 NUL**：gopsutil 的
  `ReadLines` 会把尾随 NUL 当成额外一行并报 `wrong uptime format`；
- `user/apps/ocm-agent/`：`Makefile`、`default.nix`、端到端验收脚本与说明，不含上游源码；
- `user/dadk/config/all/ocm_agent.toml`：`source = "git"`，
  `source-path = "https://github.com/sparkzky/ocm-agent.git"`，
  `revision = "d08b700..."`；构建命令通过 `DADK_SOURCE_CACHE_DIR_*` 使用 DADK
  克隆的源码目录；
- `user/apps/default.nix`：把 x86_64 的 ocm-agent 加入 Nix rootfs；
- `user/dadk/config/all/ocm_agent.toml` 及 `sets/{default,ubuntu2404}` 软链接；
- `user/apps/busybox/Makefile`：禁用 `tc` applet 并在追加配置后执行 `oldconfig`，
  以便在缺少 CBQ uapi 的新内核头文件（Linux 6.16）主机上完成 `make user`。

GitHub 镜像 `dragonos` 分支上的两个修复：

1. `internal/data/large_reporter.go`：大数据会话的占位 task_id 原先使用
   `0xFF00000000000000 | data_id`，最高位为 1，`database/sql` 无法写入 SQLite 的
   INTEGER 列，导致续传状态持久化失败。改为 `0x7F...` 前缀（仍在 int64 范围）。
2. `plugins/sysinfo-collector/base_info.go`：`host.Info()` 返回 `(nil, err)` 时原先
   直接解引用 `hostInfo.Hostname`，在缺少 `/proc/uptime` 的内核上会 SIGSEGV。增加
   空值降级。

## 8. 更新上游

```bash
# 1. 同步镜像的 dragonos 分支（在本地 clone 中）
git push git@github.com:sparkzky/ocm-agent.git <new-commit>:dragonos

# 2. 更新 DADK 的 revision
#    user/dadk/config/all/ocm_agent.toml -> revision = "<new-commit>"

# 3. 更新 default.nix 的 rev，并用新源码树重算 hash
git clone https://github.com/sparkzky/ocm-agent.git /tmp/ocm-upstream
git -C /tmp/ocm-upstream checkout <new-commit>
nix hash path --type sha256 /tmp/ocm-upstream
```
