:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: agents/release_agent_capability.md

- Translation time: 2025-12-22 11:53:32

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Release Assistant Capability Description (Reusable Template)

A universal "Release Assistant" capability for future version releases, helping Agents automatically research changes since the last Tag and generate directly usable release notes.

---

## Objectives
- Automatically collect commits from `<PREV_TAG>..HEAD`, categorized by module.
- Generate structured release notes (Key Highlights / Version Overview / Detailed Changes / Known Concerns / Acknowledgments).
- Update the ChangeLog index and prepare key points for subsequent tweets.

## Inputs
- Previous version Tag (e.g., `V0.3.0`). If no Tag is available, a baseline commit must be specified.
- Target version number (e.g., `0.4.0`) and release date.

## Outputs
- Commit snapshots: `dragonos_<VER>_commits_formatted.txt`, `dragonos_<VER>_commits_oneline.txt`
- Release notes: `docs/community/ChangeLog/V0.<X>.x/V0.<X>.0.md`
- ChangeLog index update: `docs/community/ChangeLog/index.rst`
- (Optional) Draft of tweet key points

## Path Conventions
- Release notes directory: `docs/community/ChangeLog/V0.<X>.x/`
- New entries should be appended to the toctree in `index.rst`:
  ```_translated_label__
  V0.<X>.x/V0.<X>.0
  _en```_translated_label__

## Workflow Steps
1) Determine the Baseline  
   - `PREV_TAG=$(git tag | sort -V | grep -E "0\\.3\\.0|V0\\.3\\.0" | tail -1)` (Example)  
   - Differential range: `$PREV_TAG..HEAD`

2) Export Commits  
   _en```_translated_label__
   git log $PREV_TAG..HEAD --no-merges \
     --format="%H%nAuthor: %an <%ae>%nDate: %ad%nSubject: %s%n%n%b%n---" \
     > ./dragonos_<VER>_commits_formatted.txt

   git log $PREV_TAG..HEAD --no-merges --oneline \
     > ./dragonos_<VER>_commits_oneline.txt
   _en```
   - Saved in the repository root for review and appendices.

3) Categorize and Organize (Using Keywords/Directory Dual Approach)  
   - I/O Multiplexing/Time: `pselect|poll|epoll|timer|clock|nanosleep`  
   - File System/VFS: `tmpfs|ext4|fat|vfs|symlink|truncate|preadv|pwritev|fadvise|copy_file_range|creat|umask`  
   - procfs/Observability: `/proc|stat|cmdline|maps|ns|printk`  
   - Process/Signal/Execution: `exec|fork|wait|shebang|sig|rt_|tgkill|setresuid`  
   - Memory/Page: `slab|page_cache|mmap|reclaim|user_access|iovec`  
   - IPC/Pipe: `pipe|fifo|F_GETPIPE_SZ|F_SETPIPE_SZ`  
   - Network/Device: `napi|udp|tty|ptmx|random`  
   - Engineering/CI: `ci|workflow|container|devcontainer|build container|nightly`  
   - Documentation/Community: `docs|README|Playground|translation`  
   - Testing/gVisor: `gvisor|whitelist|blocklist`

4) Generate Draft Release Notes (Using the Current 0.4.0 Structure)  
   - Sections: Key Highlights → Version Overview → Detailed Changes (by Module) → Known Concerns → Contributor Acknowledgments → References  
   - Fill in detailed changes based on the previous categorization, with each entry stating "what was done + why it's important/impactful," along with PR/commit numbers.

5) Update the Index  
   - Append the new version path to the toctree in `docs/community/ChangeLog/index.rst`.

6) Version Number and Publicly Visible Information Check  
   - DragonOS version source: `kernel/Cargo.toml`'s `version` (affects uname / /proc/version / about).  
   - Build script: `build-scripts/kernel_build/src/version_gen.rs` automatically generates `kernel/src/init/version_info.rs`.  
   - Verification: After building, execute `uname -a`, `cat /proc/version`, `about` within the system to confirm `dragonos-<VER>`.

7) Validation and Quality  
   - Format/Static Checks: `make fmt` (or `read_lints`).  
   - Compilation: `make kernel`.  
   - Key Regression Tests: I/O Multiplexing + Signal; tmpfs/chroot/mount propagation; wait queues; POSIX timer/CPU time; gVisor use cases.

8) Tweet Key Points (Optional)  
   - Extract 3–5 capability/scenario descriptions from "Key Highlights" and "Detailed Changes," avoiding a mere list of commits.

## Release Notes Writing Guidelines (Abbreviated Template)
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

## Quick Command Summary (Example)
```
PREV_TAG=V0.3.0
VER=0.4.0

git log $PREV_TAG..HEAD --no-merges \
  --format="%H%nAuthor: %an <%ae>%nDate: %ad%nSubject: %s%n%n%b%n---" \
  > ./dragonos_${VER}_commits_formatted.txt

git log $PREV_TAG..HEAD --no-merges --oneline \
  > ./dragonos_${VER}_commits_oneline.txt
```

## Minimum Execution Checklist (Copy as-is for Reuse)
1. Confirm PREV_TAG → Export commit files (formatted/oneline).
2. Categorize by keywords/directory, forming a "Module → Key Points + PR Number" table.
3. Use the template to write `docs/community/ChangeLog/V0.<X>.x/V0.<X>.0.md`.
4. Update the `docs/community/ChangeLog/index.rst` toctree.
5. If releasing: Change the version in `kernel/Cargo.toml` to `<VER>` → `make kernel` → Verify at runtime using `uname /proc/about`.
6. Run necessary fmt/compilation/regression tests.
