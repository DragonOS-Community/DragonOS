:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/filesystem/fuse.md

- Translation time: 2026-02-16 09:00:06

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

:::{note}

Author: longjin <longjin@dragonos.org>

:::

# DragonOS FUSE Architecture Design

This document focuses on "architecture understanding" and explains the layered structure, data flow, and key semantics of the DragonOS FUSE subsystem.

## Design Goals

The core objectives of FUSE in DragonOS are:

- Align with Linux FUSE semantics (referencing Linux 6.6 semantics)
- Push "filesystem policy" down to user-space daemons while keeping the kernel simple
- Seamlessly integrate with the existing VFS framework, reusing unified path resolution, file objects, and permission frameworks

## Overall Architecture

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

## Layer Responsibilities

### 1) `/dev/fuse` Device Entry Layer

- Binds a connection object to each opened `/dev/fuse` file descriptor
- `read` delivers pending requests to the daemon, `write` receives replies from the daemon
- Provides cloning capability for concurrent processing by multi-threaded daemons

Corresponding implementation entry: `kernel/src/filesystem/fuse/dev.rs`

### 2) Connection & Protocol Scheduling Layer (`FuseConn`)

- Manages connection lifecycle: pre-mount, mounted, initialization complete, disconnection/unmount
- Maintains request queues and in-flight request mappings (matching replies by unique ID)
- Completes `FUSE_INIT` capability negotiation (e.g., `max_write`, flags)
- Handles control paths such as interrupts, forget, and notify

Corresponding implementation entry: `kernel/src/filesystem/fuse/conn.rs`

### 3) Filesystem Instance Layer (`FuseFS`)

- Parses mount parameters (e.g., `fd`, `rootmode`, `allow_other`, `default_permissions`)
- Creates a FUSE filesystem instance and injects it into VFS
- Provides instance-level policies (e.g., permission policy selection)

Corresponding implementation entry: `kernel/src/filesystem/fuse/fs.rs`

### 4) Node & File Operation Layer (`FuseNode`)

- Maps VFS inode/file operations to FUSE opcodes (lookup/read/write/readdir/...)
- Maintains node cache information and lookup/forget lifecycles
- Handles common paths such as directory traversal, open/close, and attribute read/write

Corresponding implementation entry: `kernel/src/filesystem/fuse/inode.rs`

### 5) Protocol Definition Layer

- Maintains FUSE protocol constants and structures aligned with Linux uapi
- Serves as the foundation for request/response encoding/decoding on the kernel side

Corresponding implementation entry: `kernel/src/filesystem/fuse/protocol.rs`

## Typical Workflow

### Mount & Initialization

1. The user-space daemon opens `/dev/fuse`
2. `mount -t fuse ... -o fd=...` binds this connection to the mount instance
3. The kernel sends `FUSE_INIT`, completing capability negotiation with the daemon
4. After negotiation, it enters the regular request processing phase

### Regular File Access

1. A process initiates a system call (e.g., `open/read/write/readdir`)
2. VFS forwards it to `FuseNode`, which encapsulates the corresponding FUSE request
3. The request enters the connection queue, and the daemon reads it from `/dev/fuse`
4. The daemon processes it and writes back a reply; the kernel wakes the waiting request and returns it to the caller

### Unmount

1. When the filesystem is unmounted, the connection layer terminates or cleans up in-flight requests
2. The kernel sends destruction-related control messages to the daemon (e.g., `DESTROY` path)
3. The connection is finally released, and recycling is completed after the `/dev/fuse` fd is closed

## Key Points for Linux Semantic Alignment

- Mount type support normalizes `fuse.<subtype>` to `fuse` handling
- `allow_other` controls access policies for "non-mount owners"
- `default_permissions` controls the switch between VFS local DAC checks and remote permission models
- `INIT` negotiates capability bits and parameter boundaries (e.g., write size), handled uniformly by the connection layer

## Demos & Getting Started

If you want to quickly "see examples and start writing a FUSE filesystem," it is recommended to read in the following order:

- Minimal implementation without `libfuse` (directly reading/writing `/dev/fuse`)  
  `user/apps/fuse_demo/README.md`
- Example based on `libfuse3` (closer to common user-space development practices)  
  `user/apps/fuse3_demo/README.md`
- FUSE kernel/protocol regression test samples (covering more semantic boundaries)  
  `user/apps/tests/dunitest/suites/fuse/`

You can also check the corresponding DADK build entries:

- `user/dadk/config/all/fuse_demo.toml`
- `user/dadk/config/all/fuse3_demo.toml`
