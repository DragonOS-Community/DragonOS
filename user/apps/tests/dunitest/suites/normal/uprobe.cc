// uprobe 断点探针端到端测试（issue #2150 阶段一）。
//
// 验证用户态经 perf_event_open 挂载 uprobe、触发被探测函数后进程存活
// （#BP → XOL 单步 → #DB → 恢复 的命中路径不崩溃），并覆盖错误入参路径。
//
// 内核侧接口（kernel/src/perf/uprobe.rs）：
//   - perf_event_attr.type 由 /sys/bus/event_source/devices/uprobe/type 提供
//   - perf_event_attr.config1 = 目标二进制路径
//   - perf_event_attr.config2 = 文件偏移
//   - syscall 参数 pid/cpu 决定 task 或 per-CPU scope

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <limits>
#include <setjmp.h>
#include <signal.h>
#include <string>
#include <thread>
#include <vector>

#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <linux/perf_event.h>
#include <linux/bpf.h>

#ifndef SYS_perf_event_open
#include <asm/unistd.h>
#define SYS_perf_event_open __NR_perf_event_open
#endif

namespace {

constexpr const char* UPROBE_TYPE_PATH =
    "/sys/bus/event_source/devices/uprobe/type";

struct UprobePerfEventOptions {
    pid_t pid = 0;
    int cpu = -1;
    bool disabled = false;
    bool inherit = false;
    bool enable_on_exec = false;
    bool remove_on_exec = false;
    __u64 config = 0;
    int group_fd = -1;
    unsigned long flags = 0;
};

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }

    int get() const { return fd_; }
    void close_now() {
        if (fd_ >= 0) close(fd_);
        fd_ = -1;
    }

  private:
    int fd_;
};

// 动态 PMU type 是用户态 ABI，测试不得依赖内核当前的分配顺序。
bool read_uprobe_perf_type(__u32& type) {
    std::ifstream type_file(UPROBE_TYPE_PATH);
    unsigned long long parsed = 0;
    if (!type_file.is_open()) {
        errno = ENOENT;
        return false;
    }
    if (!(type_file >> parsed) ||
        parsed > std::numeric_limits<__u32>::max()) {
        errno = EINVAL;
        return false;
    }
    type = static_cast<__u32>(parsed);
    return true;
}

// 一个简单的 noinline 目标函数，作为 uprobe 的挂载点。保持纯计算，避免
// RIP-relative 操作数，降低 XOL 重定位的不确定性。
__attribute__((noinline)) int uprobe_target(int x) {
    asm volatile("" : "+r"(x) : : "memory");  // 防止内联/优化掉
    return x * 2 + 1;
}

// 从 /proc/self/maps 解析 func 在所属可执行文件中的偏移。
// 返回 false 表示解析失败（如 procfs 不支持该格式）。
bool resolve_file_offset(const void* func, std::string& path,
                         unsigned long& offset) {
    char exe_buf[4096];
    ssize_t n = readlink("/proc/self/exe", exe_buf, sizeof(exe_buf) - 1);
    if (n <= 0) return false;
    exe_buf[n] = '\0';
    path.assign(exe_buf);

    std::ifstream maps("/proc/self/maps");
    if (!maps.is_open()) return false;

    const auto func_addr = reinterpret_cast<unsigned long>(func);
    std::string line;
    while (std::getline(maps, line)) {
        unsigned long start = 0, end = 0, mapoff = 0;
        char perms[8] = {};
        // 格式：start-end perms offset dev inode pathname
        if (std::sscanf(line.c_str(), "%lx-%lx %7s %lx", &start, &end, perms,
                        &mapoff) != 4)
            continue;
        // 可执行段且 func 落在其中
        if (perms[0] == 'r' && perms[2] == 'x' && func_addr >= start &&
            func_addr < end) {
            offset = mapoff + (func_addr - start);
            return true;
        }
    }
    return false;
}

int open_uprobe_perf_event(const std::string& path, unsigned long offset,
                           const UprobePerfEventOptions& options = {}) {
    __u32 type = 0;
    if (!read_uprobe_perf_type(type)) return -1;

    struct perf_event_attr pe;
    std::memset(&pe, 0, sizeof(pe));
    pe.size = sizeof(pe);
    pe.type = type;
    pe.config = options.config;
    pe.config1 = reinterpret_cast<__u64>(path.c_str());
    pe.config2 = offset;
    pe.disabled = options.disabled;
    pe.inherit = options.inherit;
    pe.enable_on_exec = options.enable_on_exec;
    pe.remove_on_exec = options.remove_on_exec;
    return static_cast<int>(syscall(SYS_perf_event_open, &pe, options.pid,
                                    options.cpu, options.group_fd,
                                    options.flags));
}

int load_kprobe_bpf_program(const std::vector<bpf_insn>& instructions) {
    const char license[] = "GPL";
    // DragonOS currently copies its complete generated bpf_attr binding.
    // Leave versioning coverage to the BPF syscall suite and provide enough
    // zero-filled tail bytes here so this uprobe test reaches SET_BPF.
    alignas(union bpf_attr) unsigned char attr_storage[512] = {};
    auto& attr = *reinterpret_cast<union bpf_attr*>(attr_storage);
    attr.prog_type = BPF_PROG_TYPE_KPROBE;
    attr.insn_cnt = instructions.size();
    attr.insns = reinterpret_cast<__u64>(instructions.data());
    attr.license = reinterpret_cast<__u64>(license);
    return static_cast<int>(
        syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof(attr_storage)));
}

bpf_insn bpf_mov64_imm(__s32 value) {
    bpf_insn insn = {};
    insn.code = BPF_ALU64 | BPF_MOV | BPF_K;
    insn.dst_reg = BPF_REG_0;
    insn.imm = value;
    return insn;
}

bpf_insn bpf_exit() {
    bpf_insn insn = {};
    insn.code = BPF_JMP | BPF_EXIT;
    return insn;
}

constexpr unsigned char RAW_TARGET_CODE[] = {
    0x8d, 0x44, 0x3f, 0x01,  // lea eax,[rdi+rdi+1]
    0xc3,                    // ret
};

int create_raw_code(char* path_template, const unsigned char* code,
                    size_t code_size) {
    int fd = mkstemp(path_template);
    if (fd < 0) return -1;
    if (write(fd, code, code_size) != static_cast<ssize_t>(code_size)) {
        const int saved_errno = errno;
        close(fd);
        unlink(path_template);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

int create_raw_target(char* path_template) {
    return create_raw_code(path_template, RAW_TARGET_CODE,
                           sizeof(RAW_TARGET_CODE));
}

sigjmp_buf divide_fault_jmp;
volatile sig_atomic_t divide_fault_seen = 0;
static_assert(std::atomic<uintptr_t>::is_always_lock_free);
std::atomic<uintptr_t> divide_fault_addr{0};

void capture_divide_fault(int, siginfo_t* info, void*) {
    divide_fault_seen = 1;
    divide_fault_addr.store(reinterpret_cast<uintptr_t>(info->si_addr),
                            std::memory_order_relaxed);
    siglongjmp(divide_fault_jmp, 1);
}

}  // namespace

// 挂载 uprobe 到当前进程的目标函数，触发它，验证进程不崩溃且函数返回正确。
// 这条用例是 uprobe 端到端的核心验证：#BP → XOL 单步 → #DB → 恢复。
TEST(UprobeTest, RegisterAndTriggerSurvivesHit) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法从 /proc/self/maps 解析目标函数偏移";

    int fd = open_uprobe_perf_event(path, offset);
    ASSERT_GE(fd, 0) << "perf_event_open(uprobe) 失败，errno=" << errno
                     << "（内核可能未启用 uprobe）";

    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_ENABLE, 0), 0)
        << "PERF_EVENT_IOC_ENABLE 失败，errno=" << errno;

    // 执行被探测函数：应命中 uprobe，经 XOL 单步原指令后正确返回。
    // 若 uprobe 命中路径有 bug，这里可能崩溃 / hang / SIGTRAP。
    volatile int result = uprobe_target(21);
    EXPECT_EQ(result, 43);

    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
    close(fd);
}

// 非法路径应被拒绝（返回负 errno）。
TEST(UprobeTest, InvalidPathIsRejected) {
    int fd = open_uprobe_perf_event("/nonexistent/path/to/binary", 0);
    EXPECT_LT(fd, 0) << "非法路径不应成功挂载 uprobe";
    if (fd >= 0) close(fd);
}

// 越界偏移应被拒绝。
TEST(UprobeTest, InvalidOffsetIsRejected) {
    std::string path;
    unsigned long offset = 0;
    if (!resolve_file_offset(reinterpret_cast<const void*>(&uprobe_target), path,
                             offset)) {
        GTEST_SKIP() << "无法解析目标函数偏移，跳过";
    }
    int fd = open_uprobe_perf_event(path, 0xFFFFFFFFFFFFULL);
    EXPECT_LT(fd, 0) << "越界偏移不应成功挂载";
    if (fd >= 0) close(fd);
}

// 同一 uprobe 挂载后多次触发，验证 XOL 单步可重复且每次都正确返回。
// 潜在 bug：XOL slot 内容被破坏、TF 未清导致单步循环、NEED_UPROBE 未清。
TEST(UprobeTest, MultipleTriggersAllReturnCorrect) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法从 /proc/self/maps 解析目标函数偏移";

    int fd = open_uprobe_perf_event(path, offset);
    ASSERT_GE(fd, 0) << "perf_event_open(uprobe) 失败，errno=" << errno;
    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_ENABLE, 0), 0) << "ENABLE 失败";

    // 连续触发 200 次：每次都必须经 #BP → XOL → #DB → 恢复并正确返回。
    for (int i = 0; i < 200; ++i) {
        volatile int result = uprobe_target(i);
        EXPECT_EQ(result, i * 2 + 1) << "第 " << i << " 次触发结果错误";
    }

    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
    close(fd);
}

// close(fd) 触发注销（恢复原页）。注销后再调用函数应完全正常（无 0xcc 残留）。
// 验证注销路径：移除表项 → 恢复断点页 → 回收 XOL slot。
TEST(UprobeTest, UnregisterRestoresNormalExecution) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    {
        int fd = open_uprobe_perf_event(path, offset);
        ASSERT_GE(fd, 0) << "perf_event_open(uprobe) 失败，errno=" << errno;
        ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_ENABLE, 0), 0);
        // 触发一次确认探针生效
        volatile int r = uprobe_target(5);
        ASSERT_EQ(r, 11);
        // close 触发 Drop → uprobe_unregister → 恢复原页
        close(fd);
    }

    // 注销后：函数应直接执行，无断点介入，结果仍正确。
    for (int i = 0; i < 50; ++i) {
        volatile int result = uprobe_target(i + 100);
        EXPECT_EQ(result, (i + 100) * 2 + 1) << "注销后第 " << i << " 次结果错误";
    }
}

// disabled 会撤销该 consumer；若它是最后一个，应恢复原指令。
// 计数 ABI 尚未实现，这里只验证 disable/enable 生命周期不破坏控制流。
TEST(UprobeTest, DisabledStillReturnsCorrectly) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    int fd = open_uprobe_perf_event(path, offset);
    ASSERT_GE(fd, 0) << "perf_event_open(uprobe) 失败，errno=" << errno;
    // 注册即 enable（perf 默认），先 disable 再测试
    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_DISABLE, 0), 0);

    // disabled 期间应直接执行原指令。
    for (int i = 0; i < 20; ++i) {
        volatile int result = uprobe_target(i + 200);
        EXPECT_EQ(result, (i + 200) * 2 + 1) << "disabled 第 " << i << " 次结果错误";
    }

    // 重新 enable：回调恢复（此处 noop_handler），函数结果仍须正确。
    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_ENABLE, 0), 0);
    volatile int r = uprobe_target(7);
    EXPECT_EQ(r, 15);

    close(fd);
}

TEST(UprobeTest, EventSourceTypeIsPublished) {
    __u32 type = 0;
    ASSERT_TRUE(read_uprobe_perf_type(type))
        << "无法读取 " << UPROBE_TYPE_PATH << "，errno=" << errno;
    EXPECT_GT(type, 0U);
}

// perf fd 暂无可读计数，因此这里只验证 disabled 的可观察控制流语义：
// 事件以 disabled=1 创建后，被探测指令仍能正确执行，并可显式启用。
TEST(UprobeTest, InitiallyDisabledCanBeEnabled) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    UprobePerfEventOptions options;
    options.disabled = true;
    FdGuard fd(open_uprobe_perf_event(path, offset, options));
    ASSERT_GE(fd.get(), 0)
        << "disabled=1 的 perf_event_open 失败，errno=" << errno;

    for (int i = 0; i < 20; ++i) {
        volatile int result = uprobe_target(i + 300);
        EXPECT_EQ(result, (i + 300) * 2 + 1);
    }

    ASSERT_GE(ioctl(fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0)
        << "初始 disabled 事件 ENABLE 失败，errno=" << errno;
    volatile int result = uprobe_target(17);
    EXPECT_EQ(result, 35);
}

TEST(UprobeTest, InvalidPidCpuCombinationsAreRejected) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    auto expect_einval = [&](pid_t pid, int cpu) {
        UprobePerfEventOptions options;
        options.pid = pid;
        options.cpu = cpu;
        errno = 0;
        int fd = open_uprobe_perf_event(path, offset, options);
        int saved_errno = errno;
        if (fd >= 0) {
            close(fd);
            ADD_FAILURE() << "非法 pid/cpu 组合意外成功：pid=" << pid
                          << ", cpu=" << cpu;
            return;
        }
        EXPECT_EQ(saved_errno, EINVAL)
            << "pid=" << pid << ", cpu=" << cpu;
    };

    expect_einval(-1, -1);
    expect_einval(0, -2);
    expect_einval(-2, 0);
    expect_einval(0, std::numeric_limits<int>::max());
}

TEST(UprobeTest, UnsupportedInheritanceAndExecFlagsAreRejected) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    auto expect_eopnotsupp = [&](const char* flag_name,
                                 const UprobePerfEventOptions& options) {
        errno = 0;
        int fd = open_uprobe_perf_event(path, offset, options);
        int saved_errno = errno;
        if (fd >= 0) {
            close(fd);
            ADD_FAILURE() << flag_name << " 在 phase-1 不应被静默接受";
            return;
        }
        EXPECT_EQ(saved_errno, EOPNOTSUPP) << flag_name;
    };

    UprobePerfEventOptions options;
    options.inherit = true;
    expect_eopnotsupp("inherit", options);

    options = {};
    options.enable_on_exec = true;
    expect_eopnotsupp("enable_on_exec", options);

    options = {};
    options.remove_on_exec = true;
    expect_eopnotsupp("remove_on_exec", options);
}

TEST(UprobeTest, UnsupportedConfigAndPerfCoreOptionsAreRejected) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset));

    auto expect_eopnotsupp = [&](const char* name,
                                 const UprobePerfEventOptions& options) {
        errno = 0;
        FdGuard fd(open_uprobe_perf_event(path, offset, options));
        EXPECT_LT(fd.get(), 0) << name << " 不应被静默接受";
        EXPECT_EQ(errno, EOPNOTSUPP) << name;
    };

    UprobePerfEventOptions options;
    options.config = 1;  // retprobe
    expect_eopnotsupp("retprobe config", options);

    options = {};
    options.config = 1ULL << 32;  // USDT ref_ctr_offset
    expect_eopnotsupp("ref_ctr_offset config", options);

    options = {};
    options.group_fd = 0;
    expect_eopnotsupp("group_fd", options);

    options = {};
    options.flags = PERF_FLAG_PID_CGROUP;
    expect_eopnotsupp("PERF_FLAG_PID_CGROUP", options);
}

TEST(UprobeTest, RelativePathUsesCurrentWorkingDirectory) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset));
    const auto slash = path.find_last_of('/');
    ASSERT_NE(slash, std::string::npos);

    FdGuard old_cwd(open(".", O_RDONLY | O_DIRECTORY));
    ASSERT_GE(old_cwd.get(), 0);
    ASSERT_EQ(chdir(path.substr(0, slash).c_str()), 0);
    FdGuard event(open_uprobe_perf_event(path.substr(slash + 1), offset));
    const int saved_errno = errno;
    EXPECT_GE(event.get(), 0) << "相对路径应从 cwd 解析，errno=" << saved_errno;
    ASSERT_EQ(fchdir(old_cwd.get()), 0);
}

TEST(UprobeTest, UnsupportedReadReturnsErrorInsteadOfPanicking) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset));
    FdGuard fd(open_uprobe_perf_event(path, offset));
    ASSERT_GE(fd.get(), 0);

    __u64 count = 0;
    errno = 0;
    EXPECT_LT(read(fd.get(), &count, sizeof(count)), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);
}

TEST(UprobeTest, ReadOnlyAliasIsSkippedAndLaterExecutableMapIsProbed) {
    char path[] = "/tmp/uprobe_vma_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* read_only = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(read_only, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0)
        << "非可执行 alias 应被跳过而不是拒绝 consumer，errno=" << errno;

    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    auto target = reinterpret_cast<int (*)(int)>(executable);
    EXPECT_EQ(target(21), 43);

    munmap(executable, 4096);
    munmap(read_only, 4096);
    unlink(path);
}

TEST(UprobeTest, HardlinkAliasUsesCanonicalFileIdentity) {
    char path[] = "/tmp/uprobe_alias_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    const std::string alias = std::string(path) + ".link";
    ASSERT_EQ(link(path, alias.c_str()), 0);

    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    FdGuard event(open_uprobe_perf_event(alias, 0));
    ASSERT_GE(event.get(), 0) << "hardlink alias 应命中同一 page cache，errno="
                              << errno;
    auto target = reinterpret_cast<int (*)(int)>(executable);
    EXPECT_EQ(target(9), 19);

    munmap(executable, 4096);
    unlink(alias.c_str());
    unlink(path);
}

TEST(UprobeTest, TmpfsExecutableUsesPageCacheInstructionBytes) {
    char path[] = "/dev/shm/uprobe_tmpfs_XXXXXX";
    FdGuard file(create_raw_target(path));
    if (file.get() < 0 && (errno == ENOENT || errno == ENOSYS)) {
        GTEST_SKIP() << "当前 rootfs 未提供 /dev/shm tmpfs";
    }
    ASSERT_GE(file.get(), 0);
    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    if (executable == MAP_FAILED) {
        // Some DragonOS tmpfs mounts are noexec. Registration must still be
        // able to prepare the persistent definition from its page cache and
        // wait for a future eligible mapping.
        FdGuard event(open_uprobe_perf_event(path, 0));
        EXPECT_GE(event.get(), 0)
            << "tmpfs definition 应从 page cache 读取，errno=" << errno;
        unlink(path);
        return;
    }
    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "tmpfs uprobe 应从 page cache 读取，errno=" << errno;
    auto target = reinterpret_cast<int (*)(int)>(executable);
    EXPECT_EQ(target(13), 27);

    munmap(executable, 4096);
    unlink(path);
}

// 同址 consumer 必须共享 site 生命周期：关闭第一个不能拆除第二个仍需要的断点；
// 关闭最后一个后继续执行不能触发无归属的 #BP。
TEST(UprobeTest, SameAddressConsumersCloseIndependently) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    FdGuard first_fd(open_uprobe_perf_event(path, offset));
    ASSERT_GE(first_fd.get(), 0)
        << "第一个 consumer 创建失败，errno=" << errno;
    FdGuard second_fd(open_uprobe_perf_event(path, offset));
    ASSERT_GE(second_fd.get(), 0)
        << "同址第二个 consumer 创建失败，errno=" << errno;

    ASSERT_GE(ioctl(first_fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0);
    ASSERT_GE(ioctl(second_fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0);
    EXPECT_EQ(uprobe_target(31), 63);

    first_fd.close_now();
    EXPECT_EQ(uprobe_target(32), 65)
        << "关闭一个 consumer 不应破坏剩余 consumer 的 XOL 路径";

    second_fd.close_now();
    for (int i = 0; i < 50; ++i) {
        EXPECT_EQ(uprobe_target(i + 400), (i + 400) * 2 + 1)
            << "最后一个 consumer 关闭后第 " << i << " 次执行错误";
    }
}

TEST(UprobeTest, RepeatedStringInstructionIsRejected) {
    constexpr unsigned char rep_movsb[] = {
        0xf3, 0xa4,  // rep movsb
        0xc3,        // ret
    };
    char path[] = "/tmp/uprobe_rep_XXXXXX";
    FdGuard file(create_raw_code(path, rep_movsb, sizeof(rep_movsb)));
    ASSERT_GE(file.get(), 0);

    errno = 0;
    FdGuard event(open_uprobe_perf_event(path, 0));
    EXPECT_LT(event.get(), 0)
        << "phase-1 XOL must reject repeated string instructions";
    EXPECT_EQ(errno, EINVAL);
    unlink(path);
}

TEST(UprobeTest, PushfInstructionIsRejected) {
    constexpr unsigned char pushfq[] = {
        0x9c,  // pushfq would expose the XOL single-step TF bit
        0x58,  // pop rax
        0xc3,  // ret
    };
    char path[] = "/tmp/uprobe_pushf_XXXXXX";
    FdGuard file(create_raw_code(path, pushfq, sizeof(pushfq)));
    ASSERT_GE(file.get(), 0);

    errno = 0;
    FdGuard event(open_uprobe_perf_event(path, 0));
    EXPECT_LT(event.get(), 0)
        << "phase-1 XOL must reject PUSHF because TF is instrumentation state";
    EXPECT_EQ(errno, EINVAL);
    unlink(path);
}

TEST(UprobeTest, XbeginInstructionIsRejected) {
    constexpr unsigned char xbegin[] = {
        0xc7, 0xf8, 0x00, 0x00, 0x00, 0x00,  // xbegin rel32
        0xc3,                                // ret
    };
    char path[] = "/tmp/uprobe_xbegin_XXXXXX";
    FdGuard file(create_raw_code(path, xbegin, sizeof(xbegin)));
    ASSERT_GE(file.get(), 0);

    errno = 0;
    FdGuard event(open_uprobe_perf_event(path, 0));
    EXPECT_LT(event.get(), 0)
        << "phase-1 XOL must reject XBEGIN relative control flow";
    EXPECT_EQ(errno, EINVAL);
    unlink(path);
}

TEST(UprobeTest, DivideFaultReportsOriginalProbeAddress) {
    constexpr unsigned char divide_by_zero[] = {
        0x31, 0xd2,                          // xor edx,edx
        0xb8, 0x01, 0x00, 0x00, 0x00,        // mov eax,1
        0x31, 0xc9,                          // xor ecx,ecx
        0x48, 0xf7, 0xf1,                    // div rcx
        0xc3,                                // ret
    };
    constexpr unsigned long divide_offset = 9;
    char path[] = "/tmp/uprobe_divide_XXXXXX";
    FdGuard file(create_raw_code(path, divide_by_zero, sizeof(divide_by_zero)));
    ASSERT_GE(file.get(), 0);
    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, divide_offset));
    ASSERT_GE(event.get(), 0) << "failed to install divide uprobe, errno=" << errno;

    struct sigaction action = {};
    struct sigaction old_action = {};
    action.sa_sigaction = capture_divide_fault;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(sigaction(SIGFPE, &action, &old_action), 0);

    divide_fault_seen = 0;
    divide_fault_addr.store(0, std::memory_order_relaxed);
    if (sigsetjmp(divide_fault_jmp, 1) == 0) {
        reinterpret_cast<void (*)()>(executable)();
        ADD_FAILURE() << "divide by zero unexpectedly returned";
    }

    EXPECT_EQ(divide_fault_seen, 1);
    EXPECT_EQ(divide_fault_addr.load(std::memory_order_relaxed),
              reinterpret_cast<uintptr_t>(executable) + divide_offset)
        << "SIGFPE si_addr must identify the original probed instruction";
    EXPECT_EQ(sigaction(SIGFPE, &old_action, nullptr), 0);
    munmap(executable, 4096);
    unlink(path);
}

TEST(UprobeTest, DuplicateBpfAttachReturnsEexist) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));
    FdGuard event(open_uprobe_perf_event(path, offset));
    ASSERT_GE(event.get(), 0);

    const std::vector<bpf_insn> instructions = {bpf_mov64_imm(0), bpf_exit()};
    FdGuard program(load_kprobe_bpf_program(instructions));
    ASSERT_GE(program.get(), 0) << "BPF_PROG_LOAD failed, errno=" << errno;
    ASSERT_EQ(ioctl(event.get(), PERF_EVENT_IOC_SET_BPF, program.get()), 0)
        << "first SET_BPF failed, errno=" << errno;

    errno = 0;
    EXPECT_LT(ioctl(event.get(), PERF_EVENT_IOC_SET_BPF, program.get()), 0);
    EXPECT_EQ(errno, EEXIST);
}

TEST(UprobeTest, LargeBpfJitProgramAttachesSafely) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));
    FdGuard event(open_uprobe_perf_event(path, offset));
    ASSERT_GE(event.get(), 0);

    std::vector<bpf_insn> instructions(600, bpf_mov64_imm(0));
    instructions.push_back(bpf_exit());
    FdGuard program(load_kprobe_bpf_program(instructions));
    ASSERT_GE(program.get(), 0) << "large BPF_PROG_LOAD failed, errno=" << errno;
    EXPECT_EQ(ioctl(event.get(), PERF_EVENT_IOC_SET_BPF, program.get()), 0)
        << "large SET_BPF failed, errno=" << errno;
}

// Exercise the exact window where one CPU has executed INT3 while another
// CPU disables or closes the last consumer. A leaked ordinary SIGTRAP or a
// reused XOL slot terminates the test or produces a wrong result.
TEST(UprobeTest, ConcurrentTeardownDoesNotExposeRetiredBreakpoint) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    std::atomic<bool> stop{false};
    std::atomic<int> bad_results{0};
    std::atomic<unsigned long> completed_calls{0};
    std::thread runner([&]() {
        int value = 1;
        while (!stop.load(std::memory_order_acquire)) {
            if (uprobe_target(value) != value * 2 + 1) {
                bad_results.fetch_add(1, std::memory_order_relaxed);
            }
            completed_calls.fetch_add(1, std::memory_order_release);
            value = value == 1000 ? 1 : value + 1;
        }
    });

    int setup_failure_iteration = -1;
    int setup_failure_errno = 0;
    for (int i = 0; i < 200; ++i) {
        FdGuard event(open_uprobe_perf_event(path, offset));
        if (event.get() < 0 || ioctl(event.get(), PERF_EVENT_IOC_ENABLE, 0) < 0) {
            setup_failure_iteration = i;
            setup_failure_errno = errno;
            break;
        }
        const auto before = completed_calls.load(std::memory_order_acquire);
        bool made_progress = false;
        for (int spin = 0; spin < 10000; ++spin) {
            if (completed_calls.load(std::memory_order_acquire) != before) {
                made_progress = true;
                break;
            }
            std::this_thread::yield();
        }
        if (!made_progress ||
            ((i & 1) == 0 && ioctl(event.get(), PERF_EVENT_IOC_DISABLE, 0) < 0)) {
            setup_failure_iteration = i;
            setup_failure_errno = made_progress ? errno : ETIMEDOUT;
            break;
        }
        // FdGuard closes the enabled or disabled final consumer here while
        // the sibling continues to execute the target.
    }

    stop.store(true, std::memory_order_release);
    runner.join();
    EXPECT_EQ(setup_failure_iteration, -1)
        << "iteration=" << setup_failure_iteration
        << ", errno=" << setup_failure_errno;
    EXPECT_EQ(bad_results.load(std::memory_order_relaxed), 0);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
