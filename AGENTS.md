# AGENTS.md — DragonOS 文档 Harness 地图

> DragonOS 是面向云计算轻量化场景、提供 Linux 二进制兼容性的 64 位自主内核操作系统，目标是为容器化工作负载提供轻量、高性能的运行环境。

本文件只负责路由 Harness 文档。源码布局、工程约束和操作命令分别由下列文档维护，按任务需要读取：

```text
AGENTS.md                              ← Harness 文档总地图
ARCHITECTURE.md                        ← 系统边界、目录职责与阅读路由
FORCE_CONSTRAIN.md                     ← 交互、兼容性、安全与开发强约束
KEY_INFO_REMINDER.md                   ← 构建、Nix 环境与 QEMU 关键命令
docs/                                  ← 深层 Harness 文档
├── constraints/                       ← 局部操作约束入口
│   └── COMMIT.md                      ← 提交上下文与权威规范入口
├── product-specs/                     ← 产品需求文档
│   └── index.md                       ← 问题、价值、范围与需求索引
├── design-docs/                       ← 设计意图文档
│   └── index.md                       ← 技术方案与权衡索引
├── exec-plans/                        ← 执行计划与债务跟踪
│   ├── active/                        ← 进行中的实施计划
│   ├── completed/                     ← 已完成的实施计划
│   ├── tech-debt-tracker.md           ← 技术债跟踪
│   └── product-debt-tracker.md        ← 产品债跟踪
├── references/                        ← 可复用参考资料
│   └── index.md                       ← 参考资料索引
└── generated/                         ← 生成型文档预留目录
```

以上路径均相对仓库根目录，按任务读取对应权威文档。
