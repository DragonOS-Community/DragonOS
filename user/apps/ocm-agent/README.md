# ocm-agent (DragonOS 集成)

本目录只保存 DragonOS 侧的构建、打包和测试文件，**不保存 ocm-agent 源码**。

- 上游：<https://gitee.com/OpenCloudOS/ocm-agent>
- GitHub 镜像（本仓库引用 `dragonos` 分支）：<https://github.com/sparkzky/ocm-agent>
  - `master`：上游 Gitee 的纯净镜像
  - `dragonos`：上游 + 两个 DragonOS 可移植性修复
- 源码位置：`upstream/`（git submodule，固定在镜像仓库的一个 commit）

首次检出后初始化子模块：

```bash
git submodule update --init user/apps/ocm-agent/upstream
```

- 移植说明、构建方式、冒烟测试和已知限制：[PORTING.md](./PORTING.md)
- 端到端验收（运行方式、引用关系、实际产出、分析）：[ACCEPTANCE.md](./ACCEPTANCE.md)
- 2026-10-25 验收原始产物：[test-results/2026-10-25/](./test-results/2026-10-25/)
