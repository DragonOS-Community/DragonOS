# DragonOS 发布助手能力说明（可复用模版）

面向未来版本发布的通用“发布助手”能力，帮助 Agent 自动调研自上个 Tag 以来的变更并产出可直接使用的发行日志。

---

## 目标
- 自动收集 `<PREV_TAG>..HEAD` 的提交，按模块分类梳理。
- 生成结构化发行日志（核心亮点 / 版本概览 / 详细变更 / 已知关注点 / 致谢）。
- 更新 ChangeLog 索引，准备好后续推文的要点。

## 输入
- 上一版本 Tag（例：`V0.3.0`）。如无 Tag，则需要明确基线提交。
- 目标版本号（例：`0.4.0`）和发布日期。

## 输出
- 提交快照：`dragonos_<VER>_commits_formatted.txt`、`dragonos_<VER>_commits_oneline.txt`
- 发行日志：`docs/community/ChangeLog/V0.<X>.x/V0.<X>.0.md`
- ChangeLog 索引更新：`docs/community/ChangeLog/index.rst`
- （可选）推文要点草稿

## 路径约定
- 发行日志目录：`docs/community/ChangeLog/V0.<X>.x/`
- 新增条目在 `index.rst` 的 toctree 中追加：
  ```
  V0.<X>.x/V0.<X>.0
  ```

## 工作流步骤
1) 确定基线  
   - `PREV_TAG=$(git tag | sort -V | grep -E "0\\.3\\.0|V0\\.3\\.0" | tail -1)`（示例）  
   - 差分范围：`$PREV_TAG..HEAD`

2) 导出提交  
   ```
   git log $PREV_TAG..HEAD --no-merges \
     --format="%H%nAuthor: %an <%ae>%nDate: %ad%nSubject: %s%n%n%b%n---" \
     > ./dragonos_<VER>_commits_formatted.txt

   git log $PREV_TAG..HEAD --no-merges --oneline \
     > ./dragonos_<VER>_commits_oneline.txt
   ```
   - 保存在仓库根目录，便于复查和附录。

3) 分类梳理（关键词/目录双管）  
   - I/O 多路复用/时间：`pselect|poll|epoll|timer|clock|nanosleep`  
   - 文件系统/VFS：`tmpfs|ext4|fat|vfs|symlink|truncate|preadv|pwritev|fadvise|copy_file_range|creat|umask`  
   - procfs/观测性：`/proc|stat|cmdline|maps|ns|printk`  
   - 进程/信号/执行：`exec|fork|wait|shebang|sig|rt_|tgkill|setresuid`  
   - 内存/页面：`slab|page_cache|mmap|reclaim|user_access|iovec`  
   - IPC/管道：`pipe|fifo|F_GETPIPE_SZ|F_SETPIPE_SZ`  
   - 网络/设备：`napi|udp|tty|ptmx|random`  
   - 工程/CI：`ci|workflow|container|devcontainer|build container|nightly`  
   - 文档/社区：`docs|README|Playground|translation`  
   - 测试/gVisor：`gvisor|whitelist|blocklist`

4) 生成发行日志草稿（套用当前 0.4.0 结构）  
   - 章节：核心亮点 → 版本概览 → 详细变更（分模块） → 已知关注点 → 贡献者鸣谢 → 参考资料  
   - 详细变更按上一步的分类填充，每条写“做了什么 + 为什么重要/影响”，附 PR/提交号。

5) 更新索引  
   - 在 `docs/community/ChangeLog/index.rst` toctree 中追加新版本路径。

6) 版本号与对外可见信息检查  
   - DragonOS 版本源：`kernel/Cargo.toml` 的 `version`（影响 uname / /proc/version / about）。  
   - 构建脚本：`build-scripts/kernel_build/src/version_gen.rs` 自动生成 `kernel/src/init/version_info.rs`。  
   - 校验：构建后在系统内执行 `uname -a`、`cat /proc/version`、`about`，确认 `dragonos-<VER>`。

7) 验证与质量  
   - 格式/静态检查：`make fmt`（或 `read_lints`）。  
   - 编译：`make kernel`。  
   - 重点回归：I/O 多路复用+信号；tmpfs/chroot/mount 传播；等待队列；POSIX timer/CPU time；gVisor 用例。

8) 推文要点（可选）  
   - 从“核心亮点”和“详细变更”摘取 3–5 条能力/场景化描述，避免罗列 commit。

## 发行日志写作要点（缩略模板）
```
# V0.<X>.0
> 发布日期: YYYY-MM-DD

## 核心亮点
- 3–6 条面向用户/场景的升级点

## 版本概览
- 按模块列一行概览（I/O、时间、文件系统、procfs、进程/信号、内存、IPC、网络、工程、文档/社区、gVisor…）

## 详细变更
- 模块 A：若干条（含 PR/提交号）
- 模块 B：…

## 已知关注点
- 升级/测试建议与潜在风险

## 贡献者鸣谢
- 可列主要作者，或指向 Contributors 页面

## 参考资料
- CI Dashboard / Playground / Repo / Docs 等链接
```

## 快速命令汇总（示例）
```
PREV_TAG=V0.3.0
VER=0.4.0

git log $PREV_TAG..HEAD --no-merges \
  --format="%H%nAuthor: %an <%ae>%nDate: %ad%nSubject: %s%n%n%b%n---" \
  > ./dragonos_${VER}_commits_formatted.txt

git log $PREV_TAG..HEAD --no-merges --oneline \
  > ./dragonos_${VER}_commits_oneline.txt
```

## 最小执行清单（复用时照抄即可）
1. 确认 PREV_TAG → 导出提交文件（formatted/oneline）。
2. 按关键词/目录分类，形成“模块 → 要点 + PR号”表。
3. 套模板写 `docs/community/ChangeLog/V0.<X>.x/V0.<X>.0.md`。
4. 更新 `docs/community/ChangeLog/index.rst` toctree。
5. 若发布：`kernel/Cargo.toml` 版本改为 `<VER>` → `make kernel` → 运行时用 `uname /proc/about` 校验。
6. 跑必要的 fmt/编译/回归测试。


