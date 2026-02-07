# fuse_demo（DragonOS 最小 FUSE daemon，无 libfuse）

`fuse_demo` 是 DragonOS 内核 FUSE 功能的**用户态回归/演示程序**，直接读写 `/dev/fuse` 协议，不依赖 `libfuse`。

它提供一个极简的内存文件系统：

- `/hello.txt`：内容为 `hello from fuse\n`

## 用法

```
fuse_demo <mountpoint> [--rw] [--allow-other] [--default-permissions] [--threads N]
```

参数说明：

- `<mountpoint>`：挂载点目录（需已存在，或可被创建）
- `--rw`：启用写相关 opcode（create/write/truncate/rename/unlink/mkdir/rmdir）
- `--allow-other`：允许非挂载者/非同 uid 进程访问（对齐 Linux FUSE 的 `allow_other` 行为）
- `--default-permissions`：启用内核侧 DAC 权限检查（对齐 Linux FUSE 的 `default_permissions`）
- `--threads N`：启动 N 个 worker 线程（`N>=1`）。当 `N>1` 时会使用 `FUSE_DEV_IOC_CLONE` 复制连接到新的 `/dev/fuse` fd。

调试：

- `FUSE_TEST_LOG=1`：输出更详细的 request/reply 日志到 stderr

## 典型示例

### 只读演示（ls/stat/cat）

```
mkdir -p /mnt/fuse
FUSE_TEST_LOG=1 fuse_demo /mnt/fuse
```

另一个终端（或后台运行后）：

```
ls -l /mnt/fuse
cat /mnt/fuse/hello.txt
```

停止：

- 前台运行时按 `Ctrl-C`，程序会 best-effort `umount` 并退出
- 若仍残留挂载：`umount /mnt/fuse`

### 读写演示（创建/写入/重命名/删除）

```
mkdir -p /mnt/fuse
fuse_demo /mnt/fuse --rw

echo hi > /mnt/fuse/new.txt
cat /mnt/fuse/new.txt
mv /mnt/fuse/new.txt /mnt/fuse/renamed.txt
rm /mnt/fuse/renamed.txt
```

### 多线程 / clone 演示

```
mkdir -p /mnt/fuse
fuse_demo /mnt/fuse --threads 4
```

如果内核尚未支持 `FUSE_DEV_IOC_CLONE`，`--threads > 1` 会在 clone 阶段失败并提前退出（或只跑单线程，取决于当时实现）。

## 权限语义备注（对应 Phase E）

- 未指定 `--allow-other`：内核会限制“非挂载者允许的进程”调用到该 FUSE 挂载（更安全，类似 Linux 默认行为）。
- 未指定 `--default-permissions`：内核会绕过大部分本地 DAC 权限检查（remote model），把权限决策交给用户态 daemon。
  - 本 demo daemon **不做权限拒绝**，因此 remote model 下通常会更“宽松”。

