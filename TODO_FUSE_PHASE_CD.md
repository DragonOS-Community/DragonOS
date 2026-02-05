# TODO：实现 Phase C + Phase D（FUSE VFS 核心操作 + 写路径/创建操作）

> 范围：继续完成 `ROADMAP_FUSE.md` 的 Phase C、Phase D，并在 `user/apps/c_unitest` 下补齐对应单元/集成测试。
>
> 注意：本 TODO 覆盖“可演示/可用”的核心路径，不涵盖 Phase E（权限安全模型、fusermount/libfuse、clone、多线程等）。

## Phase C：读路径（`ls/stat/cat`）

- [x] C0：`FuseConn` 支持通用 request/reply（阻塞等待 + errno 透传）
  - `kernel/src/filesystem/fuse/conn.rs`
- [x] C1：实现 FUSE inode（VFS → FUSE opcode 映射）
  - `LOOKUP`：`IndexNode::find()`
  - `GETATTR`：`IndexNode::metadata()`
  - `OPENDIR/READDIR/RELEASEDIR`：`IndexNode::list()`
  - `OPEN/READ/RELEASE`：`IndexNode::open/read_at/close`
  - `kernel/src/filesystem/fuse/inode.rs`
- [x] C2：FuseFS 提供 node cache + root nodeid=1
  - `kernel/src/filesystem/fuse/fs.rs`
- [x] C3：扩展 FUSE 协议结构体/常量（最小子集）
  - `kernel/src/filesystem/fuse/protocol.rs`

## Phase D：写与创建（`touch/echo/mkdir/rm/mv/truncate`）

- [x] D1：`MKNOD/MKDIR`：`IndexNode::create_with_data()`
- [x] D2：`UNLINK/RMDIR`：`IndexNode::unlink()/rmdir()`
- [x] D3：`RENAME`：`IndexNode::move_to()`
- [x] D4：`WRITE`：`IndexNode::write_at()`（依赖 `open()` 得到 fh）
- [x] D5：`SETATTR(size)`：`IndexNode::resize()`（覆盖 `ftruncate`）
- [x] D6：`SETATTR(mode/uid/gid/size)`：`IndexNode::set_metadata()`（最小实现，后续可按 valid 精化）

## 测试（`user/apps/c_unitest`）

### 单元测试（偏接口/行为）

- [x] `test_fuse_phase_c`：daemon 提供只读树，验证 `readdir/stat/open/read`
  - 文件：`user/apps/c_unitest/test_fuse_phase_c.c`
- [x] `test_fuse_phase_d`：daemon 提供可写树，验证 `create/write/ftruncate/rename/unlink/mkdir/rmdir`
  - 文件：`user/apps/c_unitest/test_fuse_phase_d.c`

### 集成回归（与 Phase A+B 组合）

- [ ] 组合运行：
  - `/bin/test_fuse_dev`
  - `/bin/test_fuse_mount_init`
  - `/bin/test_fuse_phase_c`
  - `/bin/test_fuse_phase_d`

## 反思/迭代（直到“完全确认正确”）

- [ ] 每次失败都必须记录：触发的 opcode、返回 errno、以及用户态 daemon 收到的请求序列
- [ ] 优先修根因：协议字段/长度/对齐、offset 语义、inode type/mode 映射、fd private data 生命周期
- [ ] 对齐 Linux 6.6：错误码（如重复 mount 应返回 `EINVAL`）、阻塞语义（`EAGAIN`/poll）、release 行为
