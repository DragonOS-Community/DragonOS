:::{note}

本文作者: longjin <longjin@dragonos.org>

:::

# DragonOS FUSE 架构设计

本文面向“架构理解”，聚焦 DragonOS FUSE 子系统的分层、数据流和关键语义进行讲解。

## 设计目标

DragonOS 中 FUSE 的核心目标是：

- 对齐 Linux FUSE 语义（以 Linux 6.6 语义为参照）
- 将“文件系统策略”下沉到用户态 daemon，内核保持简洁
- 与现有 VFS 框架无缝集成，复用统一的路径解析、文件对象和权限框架

## 总体架构

```text
Userspace
  ┌─────────────────────────────────────────────────────────────┐
  │ FUSE daemon (libfuse3 或自定义协议处理循环)                    │
  │  - read(/dev/fuse) 取请求                                    │
  │  - write(/dev/fuse) 回应结果                                 │
  └───────────────────────────────┬─────────────────────────────┘
                                  │ FUSE 协议消息
Kernel                            │
  ┌───────────────────────────────▼─────────────────────────────┐
  │ /dev/fuse 字符设备层                                          │
  │  - 连接创建/克隆（clone）                                      │
  │  - 请求出队与回复入队                                          │
  ├─────────────────────────────────────────────────────────────┤
  │ FuseConn 连接层                                              │
  │  - INIT 协商、请求队列、pending 管理                            │
  │  - 中断/forget/notify 等控制消息                               │
  ├─────────────────────────────────────────────────────────────┤
  │ FuseFS + FuseNode (VFS 适配层)                               │
  │  - mount 参数解析与实例化                                      │
  │  - VFS inode/file 操作 -> FUSE opcode 映射                   │
  └───────────────────────────────┬─────────────────────────────┘
                                  │
                           VFS / Syscalls
```

## 分层职责

### 1) `/dev/fuse` 设备入口层

- 为每个打开 `/dev/fuse` 的文件描述符绑定连接对象
- `read` 向 daemon 提供待处理请求，`write` 接收 daemon 的 reply
- 提供 clone 能力用于多线程 daemon 并发处理

对应实现入口：`kernel/src/filesystem/fuse/dev.rs`

### 2) 连接与协议调度层（`FuseConn`）

- 管理连接生命周期：挂载前、已挂载、初始化完成、断连/卸载
- 维护请求队列与 in-flight 请求映射（按 unique id 匹配 reply）
- 完成 `FUSE_INIT` 能力协商（如 `max_write`、flags）
- 处理中断、forget、notify 等控制路径

对应实现入口：`kernel/src/filesystem/fuse/conn.rs`

### 3) 文件系统实例层（`FuseFS`）

- 解析挂载参数（如 `fd`、`rootmode`、`allow_other`、`default_permissions`）
- 创建 FUSE 文件系统实例并注入 VFS
- 提供实例级策略（例如权限策略选择）

对应实现入口：`kernel/src/filesystem/fuse/fs.rs`

### 4) 节点与文件操作层（`FuseNode`）

- 将 VFS inode/file 操作映射到 FUSE opcode（lookup/read/write/readdir/...）
- 维护节点缓存信息与 lookup/forget 生命周期
- 处理目录遍历、打开/关闭、属性读写等常见路径

对应实现入口：`kernel/src/filesystem/fuse/inode.rs`

### 5) 协议定义层

- 维护与 Linux uapi 对齐的 FUSE 协议常量与结构体
- 作为内核侧请求/响应编码解码的基础

对应实现入口：`kernel/src/filesystem/fuse/protocol.rs`

## 典型工作流

### 挂载与初始化

1. 用户态 daemon 打开 `/dev/fuse`
2. `mount -t fuse ... -o fd=...` 将该连接与挂载实例绑定
3. 内核发送 `FUSE_INIT`，与 daemon 完成能力协商
4. 协商完成后进入常规请求处理阶段

### 常规文件访问

1. 进程发起系统调用（如 `open/read/write/readdir`）
2. VFS 转发到 `FuseNode`，封装对应 FUSE 请求
3. 请求进入连接队列，daemon 从 `/dev/fuse` 读取
4. daemon 处理后写回 reply，内核唤醒等待请求并返回给调用者

### 卸载

1. 文件系统卸载时，连接层终止或清理 in-flight 请求
2. 内核向 daemon 发送销毁相关控制消息（如 `DESTROY` 路径）
3. 连接最终释放，`/dev/fuse` fd 关闭后完成回收

## 与 Linux 语义对齐的关键点

- 挂载类型支持 `fuse.<subtype>` 归一化到 `fuse` 处理
- `allow_other` 控制“非挂载所有者”访问策略
- `default_permissions` 控制 VFS 本地 DAC 检查与 remote 权限模型切换
- `INIT` 协商能力位与参数边界（例如写入大小）由连接层统一处理

## Demo 与上手入口

如果你想快速“看例子并开始写 FUSE 文件系统”，建议按下面顺序阅读：

- 无 `libfuse` 最小实现（直接读写 `/dev/fuse`）  
  `user/apps/fuse_demo/README.md`
- 基于 `libfuse3` 的示例（更接近常见用户态开发方式）  
  `user/apps/fuse3_demo/README.md`
- FUSE 内核/协议回归测试样例（覆盖更多语义边界）  
  `user/apps/tests/dunitest/suites/fuse/`

也可以查看对应 DADK 构建入口：

- `user/dadk/config/all/fuse_demo.toml`
- `user/dadk/config/all/fuse3_demo.toml`
