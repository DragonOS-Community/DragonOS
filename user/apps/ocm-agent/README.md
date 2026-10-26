# ocm-agent (DragonOS 集成)

本目录只保存 DragonOS 侧的构建、打包和测试文件，**不保存 ocm-agent 源码**。

- 上游：<https://gitee.com/OpenCloudOS/ocm-agent>
- GitHub 镜像（本仓库引用 `dragonos` 分支）：<https://github.com/sparkzky/ocm-agent>
  - `master`：上游 Gitee 的纯净镜像
  - `dragonos`：上游 + 两个 DragonOS 可移植性修复
- 源码获取：DADK 通过 git source 直接克隆镜像仓库的 `dragonos` 分支；
  本地直接编译时先执行 `make -C user/apps/ocm-agent fetch-src`（克隆到
  未跟踪的 `upstream/`）。

```bash
make -C user/apps/ocm-agent fetch-src   # 仅本地直接编译需要
make user                                # DADK 自动 git clone 到缓存
```

- 移植说明、构建方式、冒烟测试和已知限制：[PORTING.md](./PORTING.md)
- 端到端验收（运行方式、引用关系、实际产出、分析）：[ACCEPTANCE.md](./ACCEPTANCE.md)
- 2026-10-25 验收原始产物：[test-results/2026-10-25/](./test-results/2026-10-25/)
