# 线程的直接子进程列表

`/proc/<pid>/task/<tid>/children` 是只读 procfs 文件，列出指定线程的自然子进程。
每个 ID 后有一个空格，无额外换行；没有子进程时为空。ID 按该 proc 挂载所属的
PID namespace 表示。尚未回收的僵尸仍在列表中，完成回收后移除。

语义参考 Linux 6.6 `fs/proc/array.c` 中的 `get_children_pid()` 与
`children_seq_show()`：
https://github.com/torvalds/linux/blob/v6.6/fs/proc/array.c#L670-L769

## 实现与生命周期

`ChildrenFileOps` 复用 `proc_read_seq`，每次最多固定 64 个 PID 对象并缓冲其输出，
支持短读、EOF 和 seek。任务消失时结束枚举，已经缓冲的字节仍可读完。
与 Linux 一样，并发退出或重新托管期间不保证整个列表是原子快照。

`ProcessControlBlock::child_pids_range()` 扫描同一线程组维护的 children 索引，
通过 `fork_parent_pcb` 按具体父线程过滤。关系检查与 PID 对象固定在现有
`PTRACE_RELATION_LOCK` 事务内完成，格式化发生在释放关系锁之后。
ptrace 的等待关系不参与自然子进程列表。

`CLONE_PARENT`/`CLONE_THREAD` 和非组长线程 exec 的身份交接必须分别继承
`wait_parent_pcb` 与 `fork_parent_pcb`，不能用组长级 `real_parent_pcb` 覆盖它们。
线程退出后，既有重新托管路径更新父指针和索引，列表随之反映新的归属。

## 回归与 LMbench

`user/apps/tests/dunitest/suites/normal/procfs_children.cc` 覆盖：

- 活跃、僵尸与已回收子进程。
- 70 个子进程跨输出分片，单字节读取与回卷。
- 非组长线程 fork 及退出后的重新托管。
- `CLONE_PARENT` 和非组长 exec 保持父线程归属。
- 私有 PID namespace 内重新挂载 proc 后的 ID 转换。
- 命名 FIFO unlink 后，其 FD 链接保留原路径并附带 `(deleted)`。

测试已加入 dunitest whitelist，也可独立执行 `procfs_children_test`。
需要具备创建 PID/mount namespace 和挂载 proc 的权限；缺少能力按失败处理。

LMbench 的 `fifo_cleanup.sh` 使用该接口查找本次测试的 worker/writer，再核对
两条已删除 FIFO 的 FD 链接后终止 writer。runner 启用与脚本打包由测试套件维护。
这是保留原始 `lat_fifo` binary 时的外部生命周期适配；接口本身不含 LMbench 特例。
EOF 诊断之外的测量错误、非零退出、无有效结果或超时不能被转换成成功。
