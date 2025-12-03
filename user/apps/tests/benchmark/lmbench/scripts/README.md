# Lmbench 测试脚本

本目录包含 lmbench 基准测试的所有测试用例脚本。

## 目录结构

```
scripts/
├── env.sh                                    # 环境变量配置脚本
├── README.md                                 # 本说明文件
├── ext2_*.sh                                 # ext2 文件系统测试
├── ramfs_*.sh                                # ramfs 文件系统测试
├── mem_*.sh                                  # 内存相关测试
├── pipe_*.sh                                 # 管道测试
├── process_*.sh                              # 进程相关测试
├── semaphore_lat.sh                          # 信号量测试
├── signal_*.sh                               # 信号相关测试
├── tcp_loopback_*.sh                         # TCP loopback 测试
├── tcp_virtio_*.sh                           # TCP virtio 测试
├── udp_*.sh                                  # UDP 测试
├── unix_*.sh                                 # Unix domain socket 测试
├── vfs_*.sh                                  # VFS 相关测试
└── fifo_lat.sh                               # FIFO 测试
```

## 使用方法

### 1. 配置环境

在运行任何测试前，必须先 source 环境配置脚本：

```bash
cd user/apps/tests/benchmark/lmbench/scripts
source env.sh
```

这会配置以下环境变量：
- `LMBENCH_BIN`: lmbench 二进制文件路径 (`/lib/lmbench/bin/x86_64-linux-gnu`)
- `LMBENCH_EXT2_DIR`: ext2 文件系统测试目录 (`/ext2`)
- `LMBENCH_TMP_DIR`: 临时文件目录 (`/tmp`)
- `LMBENCH_LOG_DIR`: 日志目录 (`/tmp/lmbench_logs`)
- `LMBENCH_TEST_FILE`: 测试文件名 (`test_file`)
- `LMBENCH_ZERO_FILE`: zero 文件名 (`zero_file`)

### 2. 运行单个测试

```bash
# 例如：运行内存拷贝带宽测试
./mem_copy_bw.sh

# 例如：运行管道延迟测试
./pipe_lat.sh
```

### 3. 运行所有测试

```bash
# 遍历所有测试脚本
for script in *.sh; do
    if [ "$script" != "env.sh" ]; then
        echo "Running $script..."
        ./"$script"
    fi
done
```

## 测试分类

### 文件系统测试
- **ext2_copy_files_bw.sh**: ext2 文件拷贝带宽
- **ext2_create_delete_files_0k_ops.sh**: ext2 创建/删除 0k 文件
- **ext2_create_delete_files_10k_ops.sh**: ext2 创建/删除 10k 文件
- **ramfs_copy_files_bw.sh**: ramfs 文件拷贝带宽
- **ramfs_create_delete_files_0k_ops.sh**: ramfs 创建/删除 0k 文件
- **ramfs_create_delete_files_10k_ops.sh**: ramfs 创建/删除 10k 文件

### 内存测试
- **mem_copy_bw.sh**: 内存拷贝带宽
- **mem_read_bw.sh**: 内存读带宽
- **mem_write_bw.sh**: 内存写带宽
- **mem_mmap_bw.sh**: 内存 mmap 带宽
- **mem_mmap_lat.sh**: 内存 mmap 延迟
- **mem_pagefault_lat.sh**: 页错误延迟

### IPC 测试
- **pipe_bw.sh**: 管道带宽
- **pipe_lat.sh**: 管道延迟
- **fifo_lat.sh**: FIFO 延迟
- **semaphore_lat.sh**: 信号量延迟

### 进程测试
- **process_fork_lat.sh**: fork 延迟
- **process_exec_lat.sh**: exec 延迟
- **process_shell_lat.sh**: shell 调用延迟
- **process_ctx_lat.sh**: 进程上下文切换延迟
- **process_getppid_lat.sh**: getppid 系统调用延迟

### 信号测试
- **signal_install_lat.sh**: 信号安装延迟
- **signal_catch_lat.sh**: 信号捕获延迟
- **signal_prot_lat.sh**: 信号保护延迟

### 网络测试 (TCP)
- **tcp_loopback_bw_128.sh**: TCP loopback 带宽 (128 字节)
- **tcp_loopback_bw_4k.sh**: TCP loopback 带宽 (4k)
- **tcp_loopback_bw_64k.sh**: TCP loopback 带宽 (64k)
- **tcp_loopback_lat.sh**: TCP loopback 延迟
- **tcp_loopback_connect_lat.sh**: TCP loopback 连接延迟
- **tcp_loopback_http_bw.sh**: TCP loopback HTTP 带宽
- **tcp_loopback_select_lat.sh**: TCP select 延迟
- **tcp_virtio_bw_128.sh**: TCP virtio 带宽 (128 字节)
- **tcp_virtio_bw_64k.sh**: TCP virtio 带宽 (64k)
- **tcp_virtio_lat.sh**: TCP virtio 延迟
- **tcp_virtio_connect_lat.sh**: TCP virtio 连接延迟

### 网络测试 (UDP)
- **udp_loopback_lat.sh**: UDP loopback 延迟
- **udp_virtio_lat.sh**: UDP virtio 延迟

### Unix Domain Socket 测试
- **unix_bw.sh**: Unix socket 带宽
- **unix_lat.sh**: Unix socket 延迟
- **unix_connect_lat.sh**: Unix socket 连接延迟

### VFS 测试
- **vfs_open_lat.sh**: open 系统调用延迟
- **vfs_read_lat.sh**: read 系统调用延迟
- **vfs_write_lat.sh**: write 系统调用延迟
- **vfs_stat_lat.sh**: stat 系统调用延迟
- **vfs_fstat_lat.sh**: fstat 系统调用延迟
- **vfs_fcntl_lat.sh**: fcntl 系统调用延迟
- **vfs_select_lat.sh**: select 系统调用延迟
- **vfs_read_pagecache_bw.sh**: 页缓存读带宽

## 注意事项

### 1. 依赖文件准备
某些测试需要预先准备测试文件：
- ext2/ramfs 拷贝测试需要 `zero_file`
- mmap/pagefault 测试需要 `test_file`

### 2. 服务端进程
以下测试会自动启动服务端进程并在测试结束后清理：
- 所有 TCP loopback 测试
- UDP loopback 测试
- Unix socket 连接测试

### 3. Virtio 网络测试
`tcp_virtio_*` 和 `udp_virtio_*` 测试需要在 10.0.2.15 有对应的服务端运行。

### 4. HTTP 测试
`tcp_loopback_http_bw.sh` 需要当前目录存在 `file_list` 文件。

## 在宿主机 vs DragonOS 中运行

这些脚本设计为同时支持在宿主机和 DragonOS 中运行：

**宿主机 (Ubuntu)**:
```bash
cd user/apps/tests/benchmark/lmbench/scripts
source env.sh
./mem_copy_bw.sh
```

**DragonOS**:
```bash
cd /test/benchmark/lmbench/scripts  # 通过 /test 符号链接访问
source env.sh
./mem_copy_bw.sh
```

两个环境使用相同的路径前缀 `/lib/lmbench/bin/x86_64-linux-gnu/`。

## 下一步计划

1. 创建配置文件来控制每个测试是否执行
2. 编写 Makefile 统一运行接口
3. 添加日志重定向和结果收集
4. 实现测试结果对比工具
