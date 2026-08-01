# DragonOS 架构与源码地图

## 系统概览

DragonOS 是一个使用自主内核的 64 位操作系统，面向云计算轻量化场景并提供 Linux 二进制兼容性。系统以容器化工作负载的轻量、高性能运行需求为目标；具体实现必须同时遵守 [FORCE_CONSTRAIN.md](FORCE_CONSTRAIN.md) 中的 Linux 语义、轻量化和安全约束。

## 仓库目录职责

根目录的 `Makefile` 统一编排 `kernel`、`user`、`tools` 和 `build-scripts`。阅读或修改代码时，先按下列职责定位，避免把实现放入职责不相符的目录：

| 路径 | 职责 |
| --- | --- |
| `kernel/` | DragonOS 内核实现及其内核侧构建配置。 |
| `user/` | 用户空间应用、rootfs 组成和用户程序构建配置。 |
| `build-scripts/` | 构建过程使用的 Rust 工具和构建辅助程序。 |
| `config/` | 应用选择、rootfs manifest 等仓库级构建配置。 |
| `tools/` | 构建、镜像制作、运行、调试及开发环境辅助工具。 |
| `docs/` | 项目文档、开发指南、规格、设计说明、执行计划和参考资料。 |

## 测试代码路由

自行编写的单元测试程序位于 `user/apps/c_unitest/`。

分析 gVisor 系统调用测试时，优先读取用户提供的测试程序代码片段；用户未提供时，先在仓库及现有材料中查找。仍然找不到时，再从 [DragonOS-Community/gvisor 的指定测试目录](https://cnb.cool/DragonOS-Community/gvisor/-/tree/dragonos/release-20250616.0/test/syscalls/linux) 获取对应文件，下载后再读取和分析。这个顺序可以保证分析优先基于调用上下文和本地证据，同时保留外部测试源作为最后的权威补充。

## 高频阅读路由

| 任务 | 入口 | 局部上下文 |
| --- | --- | --- |
| 运行 DragonOS | [docs/introduction/develop_nix.md](docs/introduction/develop_nix.md) | Nix 开发环境和启动流程；常用命令另见 [KEY_INFO_REMINDER.md](KEY_INFO_REMINDER.md)。 |
| 理解 OOM Killer | `kernel/src/mm/oom.rs` | 全局状态机及缺页路径触发逻辑，细节以源码注释和对应内存管理文档为准。 |
| 理解进程管理重构后的状态与退出路径 | `kernel/src/process/state.rs`、`kernel/src/process/manager/exit.rs` | `ProcessFlags` 位于前者，退出路径位于后者；原 `mod.rs` 职责已拆分到子模块。 |

本文只提供稳定的阅读地图，不展开各子系统内部设计；进入具体模块后，应继续以对应源码和专题文档为依据。
