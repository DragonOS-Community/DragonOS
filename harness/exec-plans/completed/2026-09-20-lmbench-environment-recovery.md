# LMbench 剩余用例恢复

## 目标与约束

在当前已提交 c453800b 基础上，不新增 DragonOS 本体改动，尽可能提高一次完整48项运行中的有效成功数量。此前用户授权的timeval ABI修复保持，不回退；本阶段只处理harness、基准程序和隔离测试环境中有证据的根因。
- debug仅小白名单，最终验收才跑全部48；不隐藏失败、不改测量含义、不拼接多轮冒充单轮。
- 所有子代理 sol-medium（gpt-5.6-sol/medium），不派生代理。
- 保留用户config和dragonos-block-case；临时环境新建/tmp/lmbench-env-recovery-20260920，过程证据result/lmbench-env-recovery-20260920，refs仅最终精简数据。
- 沿用串口存8MiB/收32MiB停机、stderr存1MiB/收8MiB停机、行64KiB、guest180s/controller270s及总超时；不启savevm、不加高频日志。
- 不与Linux性能测试并发，不修改宿主全局网络；仅私有QEMU来宾网络。验收后本地提交，不推送。

## 阶段开始时的证据

上一轮单次full29ok/2failed/1panic无result/16未执行；跨3VM覆盖39wrapper-valid ok/8failed/1panic，非baseline。signal_prot先RCU panic，随后ramfs0k allocator OOM。5个virtio-labelled cases Network is unreachable，启动DHCP未验证就绪，目标10.0.2.15实际是guest自身地址，不代表外部virtio链路。unix_connect timeout而Linux同参数通过；tcp_loopback_select EAGAIN。ramfs10k独立VM通过。Linuxdash和BusyBox各48/48，当前无测试进程。上一轮临时文件按用户授权已清理，正式结果与有界失败现场均保留。

## 进展与结果

1. 网络前置条件已定位并在私有 guest 中恢复：按实际 MAC 选择 `eth2`，配置 `10.0.2.15/24` connected route，并补足 `127.0.0.1 localhost` 解析。独立 network-six 诊断 6/6 成功；该结果只覆盖 guest 自身非 loopback 地址上的协议路径，不代表 host 或外部 virtio 吞吐。
2. 首次 full48 尝试 dispatch/result 29/29，均为 ok，未观察到 panic；第29项结果产生后的私有全局 `sync` 卡住，19项未调度。该轮协议不完整，不具备 baseline 资格。
3. 将采集改为逐文件 fsync 并通过两项 checkpoint probe 后，独立 full48 尝试 dispatch 31、result 30：28 ok、2 failed、1 no-result、17未调度。第12项 `pipe_lat` 首次出现 `invalid RCU context transition` 和 cannot-unwind panic；仅前11条结果位于首次可见 panic 前，从第12项 `pipe_lat` 起共19条 result（含首次 panic 用例，17 ok、2 failed）均标记污染，其中严格位于 panic 用例之后的是18条。
4. RAMFS10k 在前述 panic 后出现 allocator OOM 且无 result；本轮不能证明它在无前序 panic 的 full 序列中独立 OOM。`vfs_fcntl_lat` failure 同样发生在 panic 后，不能作为健康环境下的独立结论。
5. 三轮结果已分别精简归档到 `harness/references/lmbench-dragonos/20260920-environment-recovery/`，没有拼接为 full48。过程日志、磁盘、脚本、源码和完整 readback 树仍位于 references 之外。

## 收尾状态

用户于 2026-09-20 选择按当前“不新增 DragonOS 本体改动”的范围收尾，RCU 缺陷转交独立 issue；本阶段结束，不再继续运行或修复内核。

- 本阶段文档、测试数据已提交为 `5b88d88d`，未推送；17 个本轮临时目标已清理，原始小型诊断证据、用户配置和历史材料保留。
- RCU bug 已公开记录为 [#2292](https://github.com/DragonOS-Community/DragonOS/issues/2292)，直接包含崩溃栈、测试条件、公开源码链接及测试内核差异，不依赖读者访问本地文件。
- 已加入 lmbench 主 issue [#2286](https://github.com/DragonOS-Community/DragonOS/issues/2286) 的问题清单，并从 #2292 回链主 issue。原生 sub-issue POST 返回 HTTP 404，当前账号没有 triage 权限；本次建立的是双向普通链接，不宣称原生父子关系已经建立。
- 本次收尾不表示完整 48 项健康验收通过。RCU panic、后续数据污染及未执行项保持原有记录；精简数据中的 `baseline_eligible=false` 不变。后续内核修复与重新验收由 #2292 继续跟踪。

## 用户分工约束

用户已在另一个分支处理 tcp_reuseport。若剩余失败确实归因该机制，本阶段仅记录证据与外部分支归属，不重复修复、不绕过、不自行引入其他分支改动。EADDRINUSE 与 ENETUNREACH 不能直接视为 SO_REUSEPORT 问题。
