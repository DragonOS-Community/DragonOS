# DragonOS 安装 CodeBuddy 时出现 `Invalid character: '\0'` 的根因分析

分析日期：2026-09-07。DragonOS 源码基线：`ecef888198af1d7861e61171f5ebd482cf921185`。
Linux 语义对照：本机 `/home/jinlong/code/linux-6.6.21/`；运行实验的宿主内核为 `7.0.0-30-generic`，并非 Linux 6.6.21。

## 1. 结论与证据强度

**根因是 DragonOS 的 ELF 加载器将可写 `PT_LOAD` 段的文件内容装入匿名 VMA，丢失了文件 backing。Bun 对其中的内嵌源码调用 `MADV_DONTNEED` 后，DragonOS 后续缺页返回零页，不能像 Linux 一样从可执行文件重新读取源码，最终 JavaScript 解析器遇到 NUL 字节。**

这不是仅凭报错猜测：

1. 下载到了与用户 SHA-256 完全一致的 CodeBuddy 2.146.0 发布包。
2. 实际 ELF 的 `.bun` 位于可写 `PT_LOAD` 段，恰好落入 DragonOS 的匿名加载分支。
3. 在 Linux 上实际跟踪到该程序向 `.bun` 源码区域发出的 `MADV_DONTNEED`。
4. 仅在这一范围模拟“丢弃后读取零页”，就重现了用户的全部错误，包括 `/$bunfs/root/codebuddy:5875`、六次后续异常和 `Bun v1.4.1 (Linux x64)`。
5. 启用 Bun 自带的禁用该建议操作开关后，即使加载同一故障注入库，安装仍成功。

**边界：本次未连接用户的 DragonOS guest，未在 guest 上跑 A/B，也未获知 guest 对应的内核提交。上述结论由当前仓库源码、同一发布包和宿主故障注入实验建立，不能表述为已经完成 guest 修复验证。**

## 2. 样本与安装阶段定位

[官方安装脚本](https://www.codebuddy.cn/cli/install.sh)执行顺序为：读取 releases/latest → 下载压缩包 → 校验压缩包 SHA-256 → 解压 → 执行临时目录里的 `codebuddy install latest`。

本次查询到的版本和样本：

| 项目 | 值 |
| --- | --- |
| 发布版本 | 2.146.0 |
| 包内 CHANGELOG 发布日期 | 2026-09-05 |
| 包名 | `codebuddy-code_Linux_x86_64.tar.gz` |
| 压缩包 SHA-256 | `ed0b94d70690d812793b590d21d73ff35b05da800bea52d4af17fa647909f5d0` |
| 解压后 codebuddy SHA-256 | `db64bd5a1585bc6b7e63eff50bc9e536b148ea35a1064e64eb0b8e968db9a204` |

发布包和校验清单：[2.146.0 压缩包](https://acc-1258344699.cos.accelerate.myqcloud.com/@tencent-ai/codebuddy-code/releases/download/2.146.0/codebuddy-code_Linux_x86_64.tar.gz)、[checksums.txt](https://acc-1258344699.cos.accelerate.myqcloud.com/@tencent-ai/codebuddy-code/releases/download/2.146.0/checksums.txt)。

校验成功只覆盖压缩包字节，不验证 ELF 映射经过 `madvise` 后仍保持正确。错误来自已启动的 Bun 内嵌 JavaScript，`/$bunfs/` 是 Bun 的虚拟源码路径，并不是要求在 DragonOS 根目录创建的真实目录。参见 [Bun 1.4.1 StandaloneModuleGraph 源码](https://github.com/oven-sh/bun/blob/bun-v1.4.1/src/standalone_graph/StandaloneModuleGraph.rs)。

另一个容易影响回溯的细节：脚本参数 `version` 传给内层安装程序；脚本下载的引导程序始终使用 `latest`。因此给脚本传旧版本号，不等于使用旧版本引导程序。

## 3. 从 ELF 到零字符的完整链路

### 3.1 实际发布包的 ELF 布局

`readelf -lW codebuddy` 中的相关段：

```text
Type  Offset     VirtAddr   FileSiz   MemSiz    Flg
LOAD  0x4bb6b70  0x4dbfb70  0x31bf490 0x31bf490 RW
```

`readelf -SW codebuddy`：

```text
.bun PROGBITS Address=0x4f95000 Offset=0x4d8c000 Size=0x2fe97af Flags=WA
```

`.bun` 全部落在这个 RW 段内。不能因为它保存 JavaScript 源码，就假定它位于 ELF 的只读段。

### 3.2 DragonOS 把可写文件段变成了匿名映射

[`kernel/src/libs/elf.rs`](../../kernel/src/libs/elf.rs) 的 `load_elf_segment()`：

- 第 379 行附近检查 `PF_W`，只有非写段进入 `map_readonly_segment()`。
- 可写段在第 422、454 行附近调用 `map_anonymous(..., allocate_at_once=true)`。
- 然后 `do_load_file()` 把文件字节复制进已分配页面。

[`kernel/src/mm/ucontext/mmap.rs`](../../kernel/src/mm/ucontext/mmap.rs) 第 95 行附近明确把 `None, None` 传入 `VMA::zeroed()`，即没有 `vm_file` 和 `backing_pgoff`。复制文件内容并不会补上这两个字段。

[`kernel/src/mm/ucontext/vma.rs`](../../kernel/src/mm/ucontext/vma.rs) 的 `is_anonymous()`（第 555 行附近）直接以 `vm_file.is_none()` 判断匿名映射。这意味着问题不是页表 RW 权限错误，而是 VMA 的来源语义错误。

### 3.3 Bun 主动丢弃内嵌源码页面

[Bun 1.4.1 的 `hint_source_pages_dont_need()`](https://github.com/oven-sh/bun/blob/bun-v1.4.1/src/standalone_graph/StandaloneModuleGraph.rs#L2798)针对内嵌源码的完整页面调用 `MADV_DONTNEED`，并提供 `BUN_FEATURE_FLAG_DISABLE_STANDALONE_MADVISE` 开关。其设计依赖源码的文件映射在后续访问时恢复；字节码与源码的生命周期并不完全相同。

对实际程序运行 `strace -f -e trace=madvise codebuddy install latest`，捕获到：

```text
madvise(0x4f96000, 50180096, MADV_DONTNEED) = 0
```

范围是 `[0x4f96000, 0x7f71000)`，完全位于 `.bun` 内，长度为 12,251 个 4 KiB 页。这把上游实现与本次发布产物连接起来，无需假设二进制一定完整对应某份上游源码。

`codebuddy --version` 和 `codebuddy install --help` 在本次实验中正常退出，但没有观察到这次源码区 DONTNEED，因此只测这两个命令不足以验证安装路径。

### 3.4 DragonOS 丢弃页后无法恢复文件内容

[`kernel/src/mm/ucontext/vma_ops.rs`](../../kernel/src/mm/ucontext/vma_ops.rs) 的 `commit_madvise_dontneed()`（第 1210 行附近）使用 `unmap_range(..., UnmapMappingMode::EvenCow)` 移除指定范围映射，同时保留 VMA。

接着 [`kernel/src/mm/fault.rs`](../../kernel/src/mm/fault.rs)：

- `handle_pte_fault()` 第 583 行附近发现没有 PTE，且 `is_anonymous()` 为真。
- 进入 `do_anonymous_page()`，通过 `map_new_page()` 分配新匿名页。
- [`kernel/src/mm/page.rs`](../../kernel/src/mm/page.rs) 的 `create_pages()` 第 447 行附近显式将新物理页写零。

因此原本保存源码的位置变成 `0x00`。后续解析或延迟加载再次使用源码时，就产生 NUL 语法错误。重复的 `processTicksAndRejections` 是可在相同内容破坏下复现的用户态后续异常，不能据此归因于调度器或 Promise 实现。

## 4. Linux 6.6.21 的对应语义

| 阶段 | Linux 6.6.21 | 当前 DragonOS |
| --- | --- | --- |
| 文件内的可写 `PT_LOAD` 部分 | `elf_map()` 使用 `vm_mmap(filep, ..., prot, type, off)` | 分配匿名 VMA 后复制文件内容 |
| 文件身份与偏移 | 保留文件映射及偏移；写权限不等于匿名来源 | `vm_file=None`、`backing_pgoff=None` |
| `MADV_DONTNEED` | 清除范围内 PTE，保留映射身份 | 同样清除 PTE，但身份已经错误 |
| 下一次读取 | 文件缺页，从 page cache/文件恢复 | 匿名缺页，分配零页 |

本地对照位置：

- `/home/jinlong/code/linux-6.6.21/fs/binfmt_elf.c:370`：`elf_map()`；第 395、399 行将 `filep` 传给 `vm_mmap()`，不以可写权限排除文件映射。
- `/home/jinlong/code/linux-6.6.21/mm/madvise.c:822`：`madvise_dontneed_single_vma()` 调用 `zap_page_range_single()`。
- `/home/jinlong/code/linux-6.6.21/mm/filemap.c:3657`：`generic_file_vm_ops` 的 `.fault = filemap_fault`。

对于 `MAP_PRIVATE` 文件映射，写入可生成私有 COW 页，但 VMA 仍有文件 backing；`DONTNEED` 后应恢复底层文件内容，不能因为存在私有页就退化为匿名零页。真正超出文件内容的 BSS 完整页则应保持匿名零填充语义。

## 5. 因果实验：精确复现用户错误

所有下载和实验文件保留在 `/tmp/codebuddy-root-cause/`，未修改内核。正常安装实验将目标用户目录定向到该目录下的 `home/`。

| 实验 | 结果 |
| --- | --- |
| 原始 2.146.0 在 Linux 执行 `install latest` | 成功；安装位置 `/tmp/codebuddy-root-cause/home/.local/bin/codebuddy` |
| 同一程序，仅把 `.bun` 内的 DONTNEED 模拟为返回零内容 | 出现与用户完全相同的语法错误、行号和异常序列 |
| 同一注入库，加 `BUN_FEATURE_FLAG_DISABLE_STANDALONE_MADVISE=1` | 安装成功，退出码 0；源码区注入未触发 |

故障注入库只拦截已确认样本中 `.bun` 范围内的 `MADV_DONTNEED`，其他请求继续走系统调用：

```c
#define _GNU_SOURCE
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <stdint.h>
#include <string.h>
#include <stdio.h>
int madvise(void *addr, size_t len, int advice) {
    uintptr_t p = (uintptr_t)addr;
    /* 仅适用于本文 SHA-256 固定的 2.146.0 ELF。 */
    if (advice == MADV_DONTNEED && p >= 0x4f95000 &&
        p < 0x7f7e7af && len <= 0x7f7e7af - p) {
        fprintf(stderr, "INJECT: zero embedded source %p len=%zu\n", addr, len);
        memset(addr, 0, len);
        return 0;
    }
    return syscall(SYS_madvise, addr, len, advice);
}
```

编译为共享库，用 `LD_PRELOAD` 加载后执行固定样本的 `install latest`。注入是对“后续读到零”的模拟，并未模拟 DragonOS 的页表、缺页时序或所有内核行为；它的作用是验证该内容变化足以导致用户观察到的精确错误。不要将其作为修复或用于其他版本。

实测输出：

```text
INJECT: zero embedded source 0x4f96000 len=50180096
SyntaxError: Invalid character: '\0'
      at /$bunfs/root/codebuddy:5875
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)
SyntaxError: Invalid character: '\0'
      at processTicksAndRejections (native:1:1)

Bun v1.4.1 (Linux x64)
```

原始证据文件：`install.strace`、`install.out`、`injected.out`、`disabled.out`、`zero_bun.c`、`zero_bun.so`，均位于上述临时目录。临时目录可能被系统清理，因此关键结果和注入源码已写入本文。

## 6. 为什么以前能安装

现有证据支持“既有 ELF 兼容性缺口被应用的内存回收行为触发”，但不能在没有以前成功版本信息的情况下指定首次失败版本。

- DragonOS 提交 `510ad1be8`（2026-01-07，#1616）已经只将非写段转为文件映射，可写段仍使用匿名映射。该缺口不是当前 HEAD 才出现。
- 提交 `1e637b2e9`（2025-10-19，#1325）加入了实际释放页面的 DONTNEED 实现。后续实现有重构，不能仅凭提交主题认定最近的某次重构导致本次回归。
- 2.146.0 包内 CHANGELOG 明确记录 Bun 升级到 1.4.1，这是近期应用变化的证据，**但不是 DONTNEED 首次出现的证据**。
- 本次额外下载并校验了 2.145.0，压缩包 SHA-256 为 `69832bd63d5808aa794ca8c4b3a010f5a9cf6fae6963e40ec70bd7f45009ae2c`。其 `.bun` 同样为 WA，且执行 `install invalid-analysis-version` 已观察到 `madvise(0x5e88000, 51949568, MADV_DONTNEED) = 0`，随后正常进入版本下载错误路径。该无效版本实验只用于观察初始化行为，不表示安装成功。

因此不能声称“退回 2.145.0 一定解决”，也不能把第一次触发简单归因于 Bun 1.4.1。以前成功的包可能尚未执行此操作，或源码布局、延迟解析路径不同；也可能当时内核的行为不同。锁定历史分界需要保存以前成功的压缩包/二进制、SHA-256、内核提交，再做交叉运行；仅记录 CLI 目标版本还不够，因为引导程序默认随 latest 更新。

## 7. 正确修复方向与验收

修复应落在 ELF 映射来源语义上：**所有 `PT_LOAD` 的文件内容部分，包括 RW 段，都应保留私有文件映射身份及正确的页偏移。**

建议统一当前按只读/可写分流的文件段映射逻辑，复用已有文件 backing 和文件缺页路径；不要为 CodeBuddy、Bun 或 `.bun` 添加特殊判断。

实现时需要同时保证：

1. 文件偏移使用 `p_offset - ELF_PAGEOFFSET(p_vaddr)`，包括非页对齐的段起点。
2. 保持 `MAP_PRIVATE` 与 COW：用户写入不能污染 executable 的 page cache 或其他进程。
3. 区分 `p_filesz` 和 `p_memsz`，保留 BSS、尾页清零、段间空洞和权限语义。
4. 保持 ELF 初始加载时的锁约束。当前 `map_file_backed()` 使用 eager 分配和复制，并解释了持有地址空间写锁时不能直接触发缺页；不能只改成 lazy 映射后继续向用户地址复制。
5. 核对文件 VMA 的 rmap、truncate、fork 和 uprobe 交互。初始页面即使是私有物理页，VMA 仍必须保留文件来源。

不应把 DONTNEED 全局改成 no-op、禁止匿名页面清零，或把可写文件映射改成共享来规避错误；这些做法会破坏其他正常程序的语义。

建议在 `user/apps/c_unitest/` 增加独立于 Bun 的回归程序：创建至少一个完整页面的、非零初始化、4096 对齐的可写静态数组，确认它位于 ELF 文件内容范围；对该完整页面执行 DONTNEED 后，检查读取值恢复为 ELF 初始值。再覆盖“先改写该页、DONTNEED 后恢复原文件值”，并与匿名 mmap 丢弃后读零做对照。修复后还应验证 BSS、尾页、fork COW，以及同一 SHA-256 的 CodeBuddy 完整安装流程。

本次仅产出分析报告，没有修改内核，也没有执行 `make kernel` 或 guest 验收。实际修复后需要执行 `make kernel`，并在 DragonOS 上运行上述测试。

## 8. guest 上最小确认方法

保留本次固定版本的解压后二进制，在 guest 中分别执行：

```sh
./codebuddy install latest
BUN_FEATURE_FLAG_DISABLE_STANDALONE_MADVISE=1 ./codebuddy install latest
```

预期第二条不再出现本次 NUL 语法错误；是否完整安装成功还取决于后续路径是否存在其他兼容性问题。这个开关只用于因果确认，不作为最终内核解决方案。

若测试原始管道，环境变量应传给右侧 `bash` 才能由内层程序继承；只设置给左侧 `curl` 不会生效。为了保留失败样本和固定版本，优先使用解压后的二进制进行对照。
