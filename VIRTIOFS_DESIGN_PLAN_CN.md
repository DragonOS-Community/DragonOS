# DragonOS virtiofs 设计与实现方案（对齐 Linux 6.6.21）

## 1. 目标与范围

目标：在 DragonOS 中实现 `virtiofs` 文件系统类型，使其可直接通过 virtio-fs 设备挂载并提供 FUSE 语义访问能力，优先保证语义正确与可维护性，再逐步提升性能。

范围：
- 实现 `mount -t virtiofs <tag> <mountpoint>` 的内核路径。
- 复用 DragonOS 现有 FUSE VFS 逻辑（`FuseFS/FuseNode`），新增 virtio 传输后端。
- 首阶段不实现 DAX（后续阶段实现）。

非目标（首阶段）：
- Linux 同等级的多队列调度策略与 NUMA 亲和。
- 完整 DAX（`FUSE_SETUPMAPPING/REMOVEMAPPING`）链路。

## 2. 现状调研结论

### 2.1 DragonOS FUSE 现状

关键代码：
- `kernel/src/filesystem/fuse/fs.rs`
- `kernel/src/filesystem/fuse/conn.rs`
- `kernel/src/filesystem/fuse/inode.rs`
- `kernel/src/filesystem/fuse/dev.rs`
- `kernel/src/filesystem/fuse/protocol.rs`

结论：
- DragonOS 已实现较完整 FUSE 内核侧语义，当前挂载入口是 `fuse`（依赖 `/dev/fuse` fd）。
- `FuseConn` 的请求模型是“内核入队 -> daemon 通过 `/dev/fuse` read/write 取放消息”。
- VFS 层与协议层基本已对齐 Linux 7.39 子集，适合作为 virtiofs 的上层复用基础。

### 2.2 DragonOS VirtIO 现状

关键代码：
- `kernel/src/driver/virtio/virtio.rs`
- `kernel/src/driver/virtio/transport.rs`
- `kernel/src/driver/virtio/transport_pci.rs`
- `kernel/src/driver/virtio/sysfs.rs`
- `virtio-drivers/src/transport/mod.rs`

结论：
- 已有 virtio 探测、transport、IRQ 框架，block/net/console 已接入。
- 当前 `virtio-drivers` 的 `DeviceType` 缺少 `FileSystem(26)`，virtio-fs 设备会被识别为 `Invalid`，这是第一阻塞点。

### 2.3 Linux 6.6.21 virtiofs 关键机制

关键代码：
- `~/code/linux-6.6.21/fs/fuse/virtio_fs.c`
- `~/code/linux-6.6.21/include/uapi/linux/virtio_fs.h`
- `~/code/linux-6.6.21/include/uapi/linux/virtio_ids.h`

关键语义：
- 设备 ID：`VIRTIO_ID_FS = 26`。
- 配置空间：`tag[36]`、`num_request_queues`。
- 挂载类型：`virtiofs`，`source` 为 tag。
- 队列模型：`hiprio`（forget）+ `request.N`（普通请求），并在卸载/拔设备时 drain in-flight 请求。
- DAX 为可选能力，通过共享内存窗口实现。

## 3. DragonOS 与 Linux 差异（需补齐）

1. 设备识别差异：DragonOS 当前无法识别 virtio-fs 设备类型。
2. 挂载模型差异：DragonOS `fuse` 必须 `fd=`；Linux `virtiofs` 使用 `tag` 直接挂载。
3. 传输模型差异：DragonOS FUSE 只支持 `/dev/fuse` 用户态 daemon 通路；Linux virtiofs 是内核直接驱动 virtqueue。
4. 生命周期差异：Linux 在 remove/umount 路径明确 stop/drain/cleanup 队列，DragonOS 尚无 virtiofs 对应实现。
5. DAX 差异：DragonOS FUSE 协议与内存映射路径当前不含 DAX 操作。

## 4. 目标架构（建议）

采用“FUSE 语义层复用 + 传输后端分离”架构：

- 保持 `FuseNode/FuseFS` 作为统一 FUSE VFS 语义实现。
- 将“请求消息怎么发出去、回复怎么回来”从 `/dev/fuse` 单一路径扩展为双后端：
  - `DevFuseBackend`（现有，用户态 daemon）
  - `VirtioFsBackend`（新增，内核 virtqueue）
- `virtiofs` 挂载时创建 `FuseConn + VirtioFsBackend` 绑定，不再依赖 `/dev/fuse fd`。

这样做的核心收益：
- 不复制 FUSE inode/file 语义代码。
- 保持 Linux 兼容行为集中在同一套 `FuseConn/FuseNode`。
- 后续 DAX、多队列优化只改 backend，不撕裂 VFS 层。

## 5. 分阶段实现计划

## 5.1 P0：打通最小可用链路（必须）

目标：`virtiofs` 可探测、可挂载、可读写目录文件、可卸载。

改动点：
1. 增加 virtio-fs 设备识别。
- 修改 `virtio-drivers/src/transport/mod.rs`：新增 `DeviceType::FileSystem = 26`，并补全 `From<u32/u16/u8>` 映射。
- 修改 `kernel/src/driver/virtio/virtio.rs`：在 `virtio_device_init()` 分派 `DeviceType::FileSystem`。

2. 新增 virtiofs 驱动模块。
- 新增 `kernel/src/driver/virtio/virtio_fs.rs`（建议路径）。
- 读取 config space：`tag` 与 `num_request_queues`。
- 建立最小队列：先只启用 1 条 request queue（后续扩展 hiprio/multi-queue）。
- 建立全局实例注册表（key=tag）。

3. 新增 `virtiofs` 文件系统 maker。
- 新增 `kernel/src/filesystem/fuse/virtiofs.rs`（或等效路径）。
- `register_mountable_fs!(..., "virtiofs")`。
- `make_mount_data(raw_data, source)`：从 `source`（tag）查找 virtiofs 设备实例，创建并返回 `FuseMountData`。
- `make_fs()`：复用 `FuseFS` 创建流程，但 conn 来源改为 virtio backend，而不是 `/dev/fuse fd`。

4. `FuseConn` 增加 backend 绑定能力。
- 需要为 virtio backend 提供请求提取与回复回填接口（避免仅能经 `/dev/fuse` 路径）。
- 推荐增加“内部消息泵”接口，不改变上层 `FuseNode` 调用方式。

验收：
- QEMU + `virtiofsd` 环境下，`mount -t virtiofs <tag> /mnt` 成功。
- `ls/stat/cat/echo > file/mkdir/rm` 可用。
- `umount` 后无悬挂请求，无内核 panic。

## 5.2 P1：并发与稳定性增强（高优先级）

目标：避免串行瓶颈与队列饥饿，完善错误回收。

改动点：
1. 请求提交/完成分离。
- 提交线程：从 `FuseConn` 拉取 pending 请求，入 virtqueue。
- IRQ 完成路径：`pop_used` 后按 `unique` 回填到 `FuseConn`。

2. 队列满处理。
- 对 `QueueFull/NotReady` 做重试与延迟调度（参考 Linux `-ENOSPC/-ENOMEM` 重排队行为）。

3. 生命周期。
- 设备 remove/unmount 时：`connected=false`，drain in-flight，唤醒等待者返回 `ENOTCONN`。

验收：
- 多线程并发 `fio`/小文件风暴不死锁。
- 热卸载场景中请求可收敛退出。

## 5.3 P2：Linux 语义对齐增强（中优先级）

目标：更接近 Linux `virtio_fs.c`。

改动点：
1. hiprio 队列。
- 将 FORGET（及后续 INTERRUPT）优先发到 hiprio，避免普通请求堵塞导致 inode 引用回收滞后。

2. 多 request queues。
- 使用 `num_request_queues`，按 CPU/哈希分发普通请求。

3. 挂载参数语义。
- 支持 `-o dax=always|never|inode` 的解析框架（即使先只接受 `never`）。
- 默认策略对齐 Linux virtiofs：`default_permissions=1`，`allow_other=1`。

## 5.4 P3：DAX（后置）

目标：支持共享内存窗口与 FUSE DAX 协议。

改动点：
- 读取 `VIRTIO_FS_SHMCAP_ID_CACHE`，建立 window 映射。
- 扩展 DragonOS FUSE 协议层支持 `FUSE_SETUPMAPPING/FUSE_REMOVEMAPPING`。
- 与 DragonOS 内存管理/VFS 页缓存策略协同。

说明：
- DAX 涉及 MM 与 FUSE 协同，复杂度高，必须在 P0/P1 稳定后推进。

## 6. 关键技术决策

1. 复用 `FuseFS/FuseNode`，不另写一套 virtiofs inode 逻辑。
- 原因：最大化语义复用，降低维护成本。

2. 以“transport backend 抽象”替代“新建旁路文件系统实现”。
- 原因：本质上 virtiofs 是 FUSE 的另一种传输，不应复制上层语义代码。

3. 首阶段不做 DAX。
- 原因：先保证功能正确与生命周期稳定，避免把 MM 风险与基础链路耦合。

4. 对 `max_write/读写分片` 语义做后端区分。
- 现有 `FuseConn` 存在面向 `/dev/fuse` 的 64KiB 约束逻辑；virtiofs 后端不应无条件继承该限制。
- 建议把该限制收敛为 `dev-fuse backend` 特有策略，virtiofs 使用队列能力上限。

## 7. 代码改动清单（建议落点）

必须改：
- `virtio-drivers/src/transport/mod.rs`
- `kernel/src/driver/virtio/virtio.rs`
- `kernel/src/driver/virtio/mod.rs`（导出新模块）
- `kernel/src/filesystem/fuse/mod.rs`（导出 `virtiofs` 子模块）
- `kernel/src/filesystem/mod.rs`（若新增模块需要接线）
- `kernel/src/filesystem/fuse/conn.rs`（backend 能力扩展）
- `kernel/src/filesystem/fuse/fs.rs`（支持从非 `/dev/fuse` conn 构建）

新增文件（建议）：
- `kernel/src/driver/virtio/virtio_fs.rs`
- `kernel/src/filesystem/fuse/virtiofs.rs`

测试与脚本（建议）：
- `tools/run-qemu.sh`（可选参数注入 `vhost-user-fs-pci`）
- 新增 dunitest 用例：`user/apps/tests/dunitest/suites/fuse/virtiofs_*.cc`

## 8. 验证方案

编译校验：
- `make kernel`
- `make fmt`

功能回归（最小）：
1. 宿主启动 `virtiofsd` 暴露共享目录。
2. QEMU 增加：
- `-chardev socket,id=charfs,path=/tmp/virtiofs.sock`
- `-device vhost-user-fs-pci,chardev=charfs,tag=hostshare`
3. DragonOS 内执行：
- `mount -t virtiofs hostshare /mnt`
- 基础文件操作与并发操作。
- `umount /mnt`。

语义回归（建议）：
- 复用现有 FUSE dunitest 思路，增加 virtiofs 版用例。
- 覆盖 open/read/write/readdir/rename/unlink/statfs/umount/hot-unplug。

## 9. 风险与规避

1. `virtio-drivers` 依赖来源风险。
- 当前 `kernel/Cargo.toml` 使用 git 依赖；若直接改本地子模块，需同步依赖策略（改 rev 或 path/patch）。

2. 队列生命周期竞态。
- remove/umount/连接 abort 三路径要统一状态机，避免重复回收和悬挂等待。

3. 性能退化风险。
- 若首版仅单队列串行，吞吐会弱于 Linux；P1/P2 要尽快补齐并发。

## 10. 里程碑建议

- M1（P0）：可挂载 + 基础 IO + 稳定卸载。
- M2（P1）：并发稳定、错误收敛、压力测试通过。
- M3（P2）：hiprio + 多队列 + 参数语义增强。
- M4（P3）：DAX。

---

本方案遵循“先语义正确、再性能”的顺序，并保持与 Linux 6.6.21 `virtio_fs.c` 的结构同构（实例注册、队列生命周期、挂载语义），同时最大化复用 DragonOS 已有 FUSE 内核语义实现。
