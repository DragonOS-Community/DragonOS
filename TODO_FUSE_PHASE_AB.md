# TODO：实现 Phase A + Phase B（FUSE 基础设施 + mount/INIT 闭环）

> 范围：仅覆盖 `ROADMAP_FUSE.md` 中 Phase A、Phase B（协议层 /dev/fuse / mount(fd=) / INIT 协商），并配套 `user/apps/c_unitest` 的单元测试与集成测试。

## Phase A（协议 + /dev/fuse）

- [x] A1：引入 FUSE uapi 子集（结构体/常量/安全解析）
  - 位置：`kernel/src/filesystem/fuse/protocol.rs`
- [x] A2：实现 `FuseConn`（请求队列 + unique + abort + INIT request 入队）
  - 位置：`kernel/src/filesystem/fuse/conn.rs`
- [x] A3：实现 `/dev/fuse` 字符设备
  - [x] devfs 注册 `/dev/fuse`
  - [x] open：初始化连接对象并写入 fd private data
  - [x] read：从 pending 队列取 request（支持 nonblock 返回 `EAGAIN`）
  - [x] write：写入 reply，匹配 unique，并驱动 INIT 完成
  - [x] poll/epoll：队列非空可读；断连返回 `POLLERR`
  - 位置：`kernel/src/filesystem/fuse/dev.rs`、`kernel/src/filesystem/devfs/mod.rs`

## Phase B（挂载闭环 + INIT 协商）

- [x] B1：注册 `FuseFS` 到 `FSMAKER`（`-t fuse`）
  - [x] 解析 mount data：`fd=,rootmode=,user_id=,group_id=`
  - [x] 校验 fd 对应 `/dev/fuse`
  - [x] 一个连接仅允许 mount 一次（重复 mount 返回 `EBUSY`）
  - 位置：`kernel/src/filesystem/fuse/fs.rs`
- [x] B2：mount 成功后自动投递 `FUSE_INIT` request
- [x] B3：处理 `FUSE_INIT` reply 并将连接标记为 initialized

## 测试（`user/apps/c_unitest`）

- [x] 单元测试：`/dev/fuse` nonblock read + poll 空队列语义
  - `user/apps/c_unitest/test_fuse_dev.c`
- [x] 集成测试：`mount -t fuse -o fd=...` 触发 INIT，并能接受 INIT reply
  - `user/apps/c_unitest/test_fuse_mount_init.c`

## 反思/回归（执行到“完全确认正确”为止）

- [x] 运行 `make user`，确认以上两个测试程序能编译进 rootfs
- [x ] 运行内核并执行：
  - `/bin/test_fuse_dev`
  - `/bin/test_fuse_mount_init`
- [x] 若失败：记录 errno/输出 → 回到对应模块修复 → 重复直到稳定通过

## 后续（不在 Phase A+B 范围内，仅记录）

- [ ] `FUSE_DEV_IOC_CLONE`（libfuse 多线程路径）
- [ ] fusectl（`/sys/fs/fuse/connections`）与 abort 控制接口
- [ ] 目录/文件 opcode（Phase C 起）

