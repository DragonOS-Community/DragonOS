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
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <linux/perf_event.h>
#include <linux/bpf.h>
#include <linux/capability.h>

#ifndef CAP_PERFMON
#define CAP_PERFMON 38
#endif

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
    __u64 read_format = 0;
    __u64 sample_period = 0;
    __u64 sample_type = 0;
    bool freq = false;
    bool pinned = false;
    bool exclusive = false;
    bool exclude_user = false;
    bool mmap_records = false;
    int clockid = 0;
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

#if defined(__x86_64__)
constexpr uint32_t TEST_RSEQ_SIG = 0x53053053;

struct alignas(32) TestRseqAbi {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    uint32_t padding;
};

struct alignas(32) TestRseqCs {
    uint32_t version;
    uint32_t flags;
    uint64_t start_ip;
    uint64_t post_commit_offset;
    uint64_t abort_ip;
};

extern "C" int rseq_uprobe_critical(TestRseqAbi*, const TestRseqCs*);
extern "C" unsigned char rseq_uprobe_start[];
extern "C" unsigned char rseq_uprobe_probe[];
extern "C" unsigned char rseq_uprobe_post_commit[];
extern "C" unsigned char rseq_uprobe_abort[];

struct RseqUprobeChildArgs {
    TestRseqAbi rseq = {};
    TestRseqCs critical_section = {};
    std::atomic<int> registration{0};
    std::atomic<bool> start{false};
    std::atomic<int> aborts{0};
    int attempts = 0;
};

int rseq_uprobe_child(void* opaque) {
    auto* args = static_cast<RseqUprobeChildArgs*>(opaque);
    cpu_set_t affinity;
    CPU_ZERO(&affinity);
    CPU_SET(0, &affinity);
    if (sched_setaffinity(0, sizeof(affinity), &affinity) != 0) {
        args->registration.store(-1, std::memory_order_release);
        return 2;
    }

    const int registered =
        syscall(SYS_rseq, &args->rseq, sizeof(args->rseq), 0, TEST_RSEQ_SIG) ==
                0
            ? 1
            : -1;
    args->registration.store(registered, std::memory_order_release);
    if (registered < 0) return 2;

    while (!args->start.load(std::memory_order_acquire)) asm volatile("pause");
    int aborts = 0;
    for (int i = 0; i < args->attempts; ++i) {
        aborts += rseq_uprobe_critical(&args->rseq,
                                       &args->critical_section) == 0;
    }
    args->aborts.store(aborts, std::memory_order_release);
    (void)syscall(SYS_rseq, &args->rseq, sizeof(args->rseq), 1,
                  TEST_RSEQ_SIG);
    return 0;
}

// Return child on a normal reap, zero when another waiter already reaped it,
// and -1 only when the child could not be proven gone. A caller must retain
// resources which could still be in use by the child in the last case.
pid_t reap_child(pid_t child, int* status) {
    auto wait_nointr = [&] {
        pid_t waited;
        do {
            waited = waitpid(child, status, 0);
        } while (waited < 0 && errno == EINTR);
        return waited;
    };

    pid_t waited = wait_nointr();
    if (waited == child) return child;
    if (waited < 0 && errno == ECHILD) return 0;

    const int wait_errno = errno;
    if (kill(child, SIGKILL) == 0 || errno == ESRCH) {
        waited = wait_nointr();
        if (waited == child) return child;
        if (waited < 0 && errno == ECHILD) return 0;
    }
    errno = wait_errno;
    return -1;
}

asm(
    ".pushsection .text\n"
    ".global rseq_uprobe_critical\n"
    ".type rseq_uprobe_critical,@function\n"
    "rseq_uprobe_critical:\n"
    "  movq %rsi, 8(%rdi)\n"
    ".global rseq_uprobe_start\n"
    "rseq_uprobe_start:\n"
    ".global rseq_uprobe_probe\n"
    "rseq_uprobe_probe:\n"
    "  nop\n"
    ".global rseq_uprobe_post_commit\n"
    "rseq_uprobe_post_commit:\n"
    "  movq $0, 8(%rdi)\n"
    "  movl $1, %eax\n"
    "  ret\n"
    "  .long 0x53053053\n"
    ".global rseq_uprobe_abort\n"
    "rseq_uprobe_abort:\n"
    "  movq $0, 8(%rdi)\n"
    "  xorl %eax, %eax\n"
    "  ret\n"
    ".size rseq_uprobe_critical, .-rseq_uprobe_critical\n"
    ".popsection\n");
#endif

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
    pe.read_format = options.read_format;
    pe.sample_period = options.sample_period;
    pe.sample_type = options.sample_type;
    pe.freq = options.freq;
    pe.pinned = options.pinned;
    pe.exclusive = options.exclusive;
    pe.exclude_user = options.exclude_user;
    pe.mmap = options.mmap_records;
    pe.clockid = options.clockid;
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

    __u64 count = 0;
    ASSERT_EQ(read(fd, &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);

    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);
    close(fd);
}

// 非法路径应被拒绝（返回负 errno）。
TEST(UprobeTest, InvalidPathIsRejected) {
    int fd = open_uprobe_perf_event("/nonexistent/path/to/binary", 0);
    EXPECT_LT(fd, 0) << "非法路径不应成功挂载 uprobe";
    if (fd >= 0) close(fd);
}

TEST(UprobeTest, PathWithoutNullWithinPathMaxIsE2big) {
    // Linux perf_uprobe_init() uses strndup_user(PATH_MAX): the terminator
    // must occur before byte PATH_MAX, otherwise perf_event_open returns
    // E2BIG instead of scanning an unbounded user mapping.
    std::string overlong(4096, 'a');
    errno = 0;
    FdGuard event(open_uprobe_perf_event(overlong, 0));
    EXPECT_LT(event.get(), 0);
    EXPECT_EQ(errno, E2BIG);
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

TEST(UprobeTest, ProcessVmWritevCannotBypassExecutableMappingPermissions) {
    char path[] = "/tmp/uprobe_process_vm_write_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* executable = mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE,
                            file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;

    unsigned char replacement = 0x90;
    iovec local = {};
    local.iov_base = &replacement;
    local.iov_len = 1;
    iovec remote = {};
    remote.iov_base = executable;
    remote.iov_len = 1;
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1,
                      0),
              -1);
    EXPECT_EQ(errno, EFAULT)
        << "process_vm_writev must use FOLL_WRITE-like VMA permission checks";

    auto target = reinterpret_cast<int (*)(int)>(executable);
    EXPECT_EQ(target(21), 43);
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);

    event.close_now();
    munmap(executable, 4096);
    unlink(path);
}

// disabled 会撤销该 consumer；若它是最后一个，应恢复原指令，且命中计数冻结。
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

    __u64 count = ~0ULL;
    ASSERT_EQ(read(fd, &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 0U);

    // 重新 enable：回调恢复（此处 noop_handler），函数结果仍须正确。
    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_ENABLE, 0), 0);
    volatile int r = uprobe_target(7);
    EXPECT_EQ(r, 15);
    ASSERT_EQ(read(fd, &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);

    close(fd);
}

TEST(UprobeTest, EventSourceTypeIsPublished) {
    __u32 type = 0;
    ASSERT_TRUE(read_uprobe_perf_type(type))
        << "无法读取 " << UPROBE_TYPE_PATH << "，errno=" << errno;
    EXPECT_GT(type, 0U);
}

// 事件以 disabled=1 创建后不计数，并可显式启用。
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

    __u64 count = ~0ULL;
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 0U);

    ASSERT_GE(ioctl(fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0)
        << "初始 disabled 事件 ENABLE 失败，errno=" << errno;
    volatile int result = uprobe_target(17);
    EXPECT_EQ(result, 35);
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);
    ASSERT_EQ(ioctl(fd.get(), PERF_EVENT_IOC_DISABLE, 0), 0);
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

TEST(UprobeTest, CounterReadUsesRawSingletonFormat) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset));
    FdGuard fd(open_uprobe_perf_event(path, offset));
    ASSERT_GE(fd.get(), 0);

    unsigned char short_buffer = 0;
    errno = 0;
    EXPECT_LT(read(fd.get(), &short_buffer, sizeof(short_buffer)), 0);
    EXPECT_EQ(errno, ENOSPC);

    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(uprobe_target(i), i * 2 + 1);
    }
    __u64 count = 0;
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 3U);
    count = 0;
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 3U) << "读取计数不应清零";

    ASSERT_EQ(ioctl(fd.get(), PERF_EVENT_IOC_DISABLE, 0), 0);
    ASSERT_EQ(ioctl(fd.get(), PERF_EVENT_IOC_RESET, 0), 0);
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 0U);
    ASSERT_EQ(ioctl(fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0);
    EXPECT_EQ(uprobe_target(4), 9);
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);
    ASSERT_EQ(ioctl(fd.get(), PERF_EVENT_IOC_DISABLE, 0), 0);
}

TEST(UprobeTest, TargetThreadExitDetachesTaskScopedSites) {
    char path[] = "/tmp/uprobe_task_exit_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* executable = mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE,
                            file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    auto target_fn = reinterpret_cast<int (*)(int)>(executable);
    const auto* target_byte = static_cast<const unsigned char*>(executable);
    const unsigned char original_byte = *target_byte;
    ASSERT_NE(original_byte, 0xcc);

    std::atomic<pid_t> target_tid{0};
    std::atomic<bool> run{false};
    std::atomic<int> target_result{0};
    std::thread target([&]() {
        target_tid.store(static_cast<pid_t>(syscall(SYS_gettid)),
                         std::memory_order_release);
        while (!run.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        target_result.store(target_fn(21), std::memory_order_release);
    });
    while (target_tid.load(std::memory_order_acquire) == 0) {
        std::this_thread::yield();
    }

    UprobePerfEventOptions options;
    options.pid = target_tid.load(std::memory_order_acquire);
    FdGuard event(open_uprobe_perf_event(path, 0, options));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    EXPECT_EQ(*target_byte, 0xcc);
    run.store(true, std::memory_order_release);
    target.join();
    EXPECT_EQ(target_result.load(std::memory_order_acquire), 43);

    EXPECT_EQ(*target_byte, original_byte)
        << "target exit must detach task-scoped breakpoints";
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);
    ASSERT_EQ(ioctl(event.get(), PERF_EVENT_IOC_ENABLE, 0), 0);
    EXPECT_EQ(*target_byte, original_byte)
        << "an exited task event must not be reactivated";
    event.close_now();
    munmap(executable, 4096);
    unlink(path);
}

TEST(UprobeTest, UnsupportedReadFormatIsRejected) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));
    UprobePerfEventOptions options;
    options.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED;
    errno = 0;
    FdGuard fd(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(fd.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options.read_format = 1ULL << 63;
    errno = 0;
    FdGuard unknown(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(unknown.get(), 0);
    EXPECT_EQ(errno, EINVAL);
}

TEST(UprobeTest, UnsupportedSamplingAttributesAreRejected) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));

    UprobePerfEventOptions options;
    options.sample_period = 1;
    errno = 0;
    FdGuard period(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(period.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.sample_period = 100;
    options.freq = true;
    errno = 0;
    FdGuard frequency(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(frequency.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.sample_type = PERF_SAMPLE_IP;
    errno = 0;
    FdGuard sample(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(sample.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.mmap_records = true;
    errno = 0;
    FdGuard sideband(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(sideband.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.pinned = true;
    errno = 0;
    FdGuard pinned(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(pinned.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.exclusive = true;
    errno = 0;
    FdGuard exclusive(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(exclusive.get(), 0);
    EXPECT_EQ(errno, EOPNOTSUPP);

    options = {};
    options.sample_type = 1ULL << 63;
    errno = 0;
    FdGuard unknown(open_uprobe_perf_event(path, offset, options));
    EXPECT_LT(unknown.get(), 0);
    EXPECT_EQ(errno, EINVAL);
}

TEST(UprobeTest, ExcludeUserMatchesLinuxTraceUprobeBehavior) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));
    UprobePerfEventOptions options;
    options.exclude_user = true;
    FdGuard fd(open_uprobe_perf_event(path, offset, options));
    ASSERT_GE(fd.get(), 0) << "errno=" << errno;

    EXPECT_EQ(uprobe_target(20), 41);
    __u64 count = 0;
    ASSERT_EQ(read(fd.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);
}

TEST(UprobeTest, DormantSamplingPayloadIsIgnored) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));
    UprobePerfEventOptions options;
    // clockid is only meaningful when use_clockid is set. Linux ignores the
    // dormant payload, so a plain counting uprobe must remain valid.
    options.clockid = 123;
    FdGuard fd(open_uprobe_perf_event(path, offset, options));
    ASSERT_GE(fd.get(), 0) << "errno=" << errno;
}

TEST(UprobeTest, PerfmonCapabilityAuthorizesOpen) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        __user_cap_header_struct header = {
            .version = _LINUX_CAPABILITY_VERSION_3,
            .pid = 0,
        };
        __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3] = {};
        constexpr unsigned int kPerfmonWord = CAP_PERFMON / 32;
        constexpr __u32 kPerfmonBit = 1U << (CAP_PERFMON % 32);
        caps[kPerfmonWord].effective = kPerfmonBit;
        caps[kPerfmonWord].permitted = kPerfmonBit;
        if (syscall(SYS_capset, &header, caps) != 0) {
            _exit(1);
        }
        const int fd = open_uprobe_perf_event(path, offset);
        if (fd < 0) {
            _exit(errno == EACCES ? 2 : 3);
        }
        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0)
        << "CAP_PERFMON-only perf_event_open child exit="
        << WEXITSTATUS(status);
}

TEST(UprobeTest, ReadOnlyAliasAndLaterExecutableMapAreProbed) {
    char path[] = "/tmp/uprobe_vma_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* read_only = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(read_only, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0)
        << "VM_MAYEXEC alias should retain the persistent consumer, errno="
        << errno;

    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    auto target = reinterpret_cast<int (*)(int)>(executable);
    EXPECT_EQ(target(21), 43);

    munmap(executable, 4096);
    munmap(read_only, 4096);
    unlink(path);
}

TEST(UprobeTest, NonExecutableMappingIsPatchedBeforeMprotectExec) {
    char path[] = "/tmp/uprobe_mprotect_exec_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* mapping =
        mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(mapping, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    ASSERT_EQ(mprotect(mapping, 4096, PROT_READ | PROT_EXEC), 0);
    auto target = reinterpret_cast<int (*)(int)>(mapping);
    EXPECT_EQ(target(12), 25);

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U)
        << "mprotect must expose an already-patched executable page";

    munmap(mapping, 4096);
    unlink(path);
}

TEST(UprobeTest, WritableMprotectWithdrawsProbeBeforePublishingWrite) {
    char path[] = "/tmp/uprobe_mprotect_write_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* mapping =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(mapping, MAP_FAILED);
    auto target = reinterpret_cast<int (*)(int)>(mapping);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    EXPECT_EQ(target(12), 25);

    ASSERT_EQ(mprotect(mapping, 4096, PROT_READ | PROT_WRITE), 0);
    EXPECT_EQ(*static_cast<unsigned char*>(mapping), RAW_TARGET_CODE[0])
        << "the breakpoint byte must be restored before write is published";
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U)
        << "a writable mapping is no longer eligible for the uprobe";

    munmap(mapping, 4096);
    unlink(path);
}

TEST(UprobeTest, DontNeedReconcilesPersistentProbe) {
    char path[] = "/tmp/uprobe_dontneed_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    auto target = reinterpret_cast<int (*)(int)>(executable);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    EXPECT_EQ(target(5), 11);
    ASSERT_EQ(madvise(executable, 4096, MADV_DONTNEED), 0);
    EXPECT_EQ(target(7), 15)
        << "DONTNEED refault must reinstall the persistent probe";

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 2U);
    event.close_now();
    EXPECT_EQ(target(9), 19) << "close must restore the refaulted instruction";

    munmap(executable, 4096);
    unlink(path);
}

TEST(UprobeTest, ExecutableVmaMutationHasNoUnarmedWindow) {
    const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char path[] = "/tmp/uprobe_vma_publish_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* executable =
        mmap(nullptr, page_size, PROT_READ | PROT_EXEC, MAP_PRIVATE,
             file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);
    auto target = reinterpret_cast<int (*)(int)>(executable);

    std::atomic<pid_t> runner_tid{0};
    std::atomic<bool> run{false};
    std::atomic<bool> stop{false};
    std::atomic<int> bad_results{0};
    std::atomic<__u64> calls{0};
    std::thread runner([&] {
        runner_tid.store(static_cast<pid_t>(syscall(SYS_gettid)),
                         std::memory_order_release);
        while (!run.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        while (!stop.load(std::memory_order_acquire)) {
            if (target(3) != 7) {
                bad_results.fetch_add(1, std::memory_order_relaxed);
            }
            calls.fetch_add(1, std::memory_order_release);
        }
    });
    while (runner_tid.load(std::memory_order_acquire) == 0) {
        std::this_thread::yield();
    }

    UprobePerfEventOptions options;
    options.pid = runner_tid.load(std::memory_order_acquire);
    FdGuard event(open_uprobe_perf_event(path, 0, options));
    if (event.get() < 0) {
        const int open_errno = errno;
        stop.store(true, std::memory_order_release);
        run.store(true, std::memory_order_release);
        runner.join();
        FAIL() << "runner uprobe open failed, errno=" << open_errno;
        munmap(executable, page_size);
        unlink(path);
        return;
    }

    run.store(true, std::memory_order_release);
    while (calls.load(std::memory_order_acquire) < 256) {
        std::this_thread::yield();
    }

    int mutation_errno = 0;
    for (int i = 0; i < 32 && mutation_errno == 0; ++i) {
        if (mprotect(executable, page_size, PROT_EXEC) != 0 ||
            mprotect(executable, page_size, PROT_READ | PROT_EXEC) != 0 ||
            madvise(executable, page_size, MADV_DONTNEED) != 0) {
            mutation_errno = errno;
        }
    }
    const __u64 calls_after_mutation = calls.load(std::memory_order_acquire);
    while (calls.load(std::memory_order_acquire) <
           calls_after_mutation + 256) {
        std::this_thread::yield();
    }
    stop.store(true, std::memory_order_release);
    runner.join();

    EXPECT_EQ(mutation_errno, 0);
    EXPECT_EQ(bad_results.load(std::memory_order_relaxed), 0);
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, calls.load(std::memory_order_relaxed))
        << "mprotect/MADV_DONTNEED exposed executable original bytes before "
           "the probe was reinstalled";

    munmap(executable, page_size);
    unlink(path);
}

TEST(UprobeTest, MremapMoveReconcilesSourceAndDestination) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    char path[] = "/tmp/uprobe_mremap_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* source = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                        MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(source, reservation);
    auto source_target = reinterpret_cast<int (*)(int)>(source);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    EXPECT_EQ(source_target(3), 7);

    void* destination = static_cast<char*>(reservation) + page_size;
    void* moved = mremap(source, page_size, page_size,
                         MREMAP_MAYMOVE | MREMAP_FIXED, destination);
    ASSERT_EQ(moved, destination) << "errno=" << errno;
    auto moved_target = reinterpret_cast<int (*)(int)>(moved);
    EXPECT_EQ(moved_target(4), 9);

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 2U);
    event.close_now();
    EXPECT_EQ(moved_target(6), 13)
        << "close after mremap must not leave an ownerless INT3";

    munmap(moved, page_size);
    unlink(path);
}

TEST(UprobeTest, MremapInPlaceGrowthPublishesProbeBeforeReturn) {
    const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char path[] = "/tmp/uprobe_mremap_grow_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(ftruncate(file.get(), static_cast<off_t>(page_size * 2)), 0);
    ASSERT_EQ(pwrite(file.get(), RAW_TARGET_CODE, sizeof(RAW_TARGET_CODE),
                     static_cast<off_t>(page_size)),
              static_cast<ssize_t>(sizeof(RAW_TARGET_CODE)));

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* source = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                        MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(source, reservation);
    ASSERT_EQ(munmap(static_cast<char*>(reservation) + page_size, page_size),
              0);

    FdGuard event(open_uprobe_perf_event(path, page_size));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    void* grown = mremap(source, page_size, page_size * 2, 0);
    ASSERT_EQ(grown, source) << "errno=" << errno;
    auto grown_target = reinterpret_cast<int (*)(int)>(
        static_cast<char*>(grown) + page_size);
    EXPECT_EQ(grown_target(8), 17);

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U)
        << "the new executable offset must be probed before mremap returns";

    munmap(grown, page_size * 2);
    unlink(path);
}

TEST(UprobeTest, MremapDontUnmapReconcilesRetainedSourceAndDestination) {
    const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char path[] = "/tmp/uprobe_mremap_dontunmap_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);

    void* source = mmap(nullptr, page_size, PROT_READ | PROT_EXEC, MAP_PRIVATE,
                        file.get(), 0);
    ASSERT_NE(source, MAP_FAILED);
    // MREMAP_DONTUNMAP clears VM_LOCKED on the retained source. This forces
    // the kernel through the set_flags() path which used to overwrite the
    // temporary NX publication barrier before the source probe was rearmed.
    ASSERT_EQ(mlock(source, page_size), 0) << "errno=" << errno;
    auto source_target = reinterpret_cast<int (*)(int)>(source);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    EXPECT_EQ(source_target(2), 5);

    std::atomic<pid_t> source_tid{0};
    std::atomic<bool> run{false};
    std::atomic<bool> stop{false};
    std::atomic<int> bad_results{0};
    std::atomic<__u64> source_calls{0};
    std::thread source_runner([&] {
        source_tid.store(static_cast<pid_t>(syscall(SYS_gettid)),
                         std::memory_order_release);
        while (!run.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        while (!stop.load(std::memory_order_acquire)) {
            if (source_target(3) != 7) {
                bad_results.fetch_add(1, std::memory_order_relaxed);
            }
            source_calls.fetch_add(1, std::memory_order_release);
        }
    });
    while (source_tid.load(std::memory_order_acquire) == 0) {
        std::this_thread::yield();
    }
    UprobePerfEventOptions runner_options;
    runner_options.pid = source_tid.load(std::memory_order_acquire);
    FdGuard runner_event(
        open_uprobe_perf_event(path, 0, runner_options));
    if (runner_event.get() < 0) {
        const int open_errno = errno;
        stop.store(true, std::memory_order_release);
        run.store(true, std::memory_order_release);
        source_runner.join();
        FAIL() << "runner task uprobe failed, errno=" << open_errno;
        return;
    }
    run.store(true, std::memory_order_release);
    while (source_calls.load(std::memory_order_acquire) < 256) {
        std::this_thread::yield();
    }

    void* moved = mremap(source, page_size, page_size,
                         MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
    const __u64 calls_after_mremap =
        source_calls.load(std::memory_order_acquire);
    while (source_calls.load(std::memory_order_acquire) <
           calls_after_mremap + 256) {
        std::this_thread::yield();
    }
    stop.store(true, std::memory_order_release);
    source_runner.join();
    EXPECT_EQ(bad_results.load(std::memory_order_relaxed), 0);
    ASSERT_NE(moved, MAP_FAILED) << "errno=" << errno;
    ASSERT_NE(moved, source);
    auto moved_target = reinterpret_cast<int (*)(int)>(moved);

    EXPECT_EQ(source_target(3), 7)
        << "the retained source must be reprobed before mremap returns";
    EXPECT_EQ(moved_target(4), 9)
        << "the moved destination must be probed before it becomes executable";

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 3U);
    ASSERT_EQ(read(runner_event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, source_calls.load(std::memory_order_relaxed))
        << "every concurrent retained-source execution must remain probed";
    event.close_now();
    EXPECT_EQ(source_target(5), 11);
    EXPECT_EQ(moved_target(6), 13);

    munmap(source, page_size);
    munmap(moved, page_size);
    unlink(path);
}

TEST(UprobeTest, RejectedMprotectDoesNotWithdrawProbe) {
    const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char path[] = "/tmp/uprobe_mprotect_reject_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* executable = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                            MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(executable, reservation);
    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    // Keep the adjacent page reserved until after the event creates its XOL
    // mapping; otherwise the XOL page can legitimately occupy the intended
    // hole and make the mprotect request valid.
    ASSERT_EQ(munmap(static_cast<char*>(reservation) + page_size, page_size),
              0);
    auto target = reinterpret_cast<int (*)(int)>(executable);
    std::atomic<bool> start{false};
    std::atomic<bool> done{false};
    std::atomic<int> syscall_failures{0};
    std::atomic<__u64> calls{0};
    std::thread mutator([&] {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        // Keep issuing rejected mutations until the executing thread has
        // completed a substantial number of calls. This prevents the test
        // from passing with an empty overlap window on a fast scheduler.
        while (calls.load(std::memory_order_acquire) < 256) {
            errno = 0;
            if (mprotect(executable, page_size * 2,
                         PROT_READ | PROT_EXEC) == -1 && errno == ENOMEM) {
                syscall_failures.fetch_add(1, std::memory_order_relaxed);
            }
            std::this_thread::yield();
        }
        done.store(true, std::memory_order_release);
    });
    start.store(true, std::memory_order_release);
    do {
        EXPECT_EQ(target(1), 3);
        calls.fetch_add(1, std::memory_order_relaxed);
    } while (!done.load(std::memory_order_acquire));
    mutator.join();
    EXPECT_GT(syscall_failures.load(std::memory_order_relaxed), 0);
    EXPECT_GE(calls.load(std::memory_order_relaxed), 256U);

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, calls.load(std::memory_order_relaxed))
        << "rejected mprotect must not create an unarmed execution window";
    munmap(executable, page_size);
    unlink(path);
}

TEST(UprobeTest, RejectedMremapDoesNotWithdrawSourceProbe) {
    const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char path[] = "/tmp/uprobe_mremap_reject_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* executable = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                            MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(executable, reservation);
    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    ASSERT_EQ(munmap(static_cast<char*>(reservation) + page_size, page_size),
              0);
    auto target = reinterpret_cast<int (*)(int)>(executable);
    std::atomic<bool> start{false};
    std::atomic<bool> done{false};
    std::atomic<int> syscall_failures{0};
    std::atomic<__u64> calls{0};
    std::thread mutator([&] {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        while (calls.load(std::memory_order_acquire) < 256) {
            errno = 0;
            if (mremap(executable, page_size * 2, page_size * 3,
                       MREMAP_MAYMOVE) == MAP_FAILED && errno == EFAULT) {
                syscall_failures.fetch_add(1, std::memory_order_relaxed);
            }
            std::this_thread::yield();
        }
        done.store(true, std::memory_order_release);
    });
    start.store(true, std::memory_order_release);
    do {
        EXPECT_EQ(target(2), 5);
        calls.fetch_add(1, std::memory_order_relaxed);
    } while (!done.load(std::memory_order_acquire));
    mutator.join();
    EXPECT_GT(syscall_failures.load(std::memory_order_relaxed), 0);
    EXPECT_GE(calls.load(std::memory_order_relaxed), 256U);

    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, calls.load(std::memory_order_relaxed))
        << "rejected mremap must not create an unarmed source window";
    munmap(executable, page_size);
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

TEST(UprobeTest, AdjacentUnrelatedMappingDoesNotChangeInstruction) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    constexpr unsigned char file_tail[] = {
        0xb8, 0x2a, 0x00, 0x00, 0x00,  // mov eax,42
        0xc3,                          // ret
    };
    constexpr unsigned char mapped_tail[] = {
        0x07, 0x00, 0x00, 0x00,  // immediate used by the cross-VMA mov
        0xc3,                    // ret
    };

    char path[] = "/tmp/uprobe_cross_vma_XXXXXX";
    FdGuard file(mkstemp(path));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(ftruncate(file.get(), static_cast<off_t>(page_size * 2)), 0);
    ASSERT_EQ(pwrite(file.get(), file_tail, sizeof(file_tail),
                     static_cast<off_t>(page_size - 1)),
              static_cast<ssize_t>(sizeof(file_tail)));

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* first = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                       MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(first, reservation);
    auto* second_addr = static_cast<unsigned char*>(reservation) + page_size;
    void* second = mmap(second_addr, page_size, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    ASSERT_EQ(second, second_addr);
    std::memcpy(second, mapped_tail, sizeof(mapped_tail));
    ASSERT_EQ(mprotect(second, page_size, PROT_READ | PROT_EXEC), 0);

    auto target = reinterpret_cast<int (*)()>(second_addr - 1);
    ASSERT_EQ(target(), 7) << "native execution must use the adjacent mapping";

    FdGuard event(open_uprobe_perf_event(path, page_size - 1));
    ASSERT_GE(event.get(), 0)
        << "an incompatible alias must be skipped without rejecting the consumer";
    EXPECT_EQ(target(), 7)
        << "XOL must not substitute bytes from a different file mapping";

    munmap(reservation, page_size * 2);
    unlink(path);
}

TEST(UprobeTest, PrivateTailByteMismatchIsRejected) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    constexpr unsigned char file_tail[] = {
        0xb8, 0x2a, 0x00, 0x00, 0x00,  // mov eax,42
        0xc3,                          // ret
    };
    constexpr unsigned char private_tail[] = {
        0x07, 0x00, 0x00, 0x00,  // private immediate
        0xc3,                    // ret
    };

    char path[] = "/tmp/uprobe_private_tail_XXXXXX";
    FdGuard file(mkstemp(path));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(ftruncate(file.get(), static_cast<off_t>(page_size * 2)), 0);
    ASSERT_EQ(pwrite(file.get(), file_tail, sizeof(file_tail),
                     static_cast<off_t>(page_size - 1)),
              static_cast<ssize_t>(sizeof(file_tail)));

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    ASSERT_EQ(mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                   MAP_PRIVATE | MAP_FIXED, file.get(), 0),
              reservation);
    auto* second_addr = static_cast<unsigned char*>(reservation) + page_size;
    void* second = mmap(second_addr, page_size, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_FIXED, file.get(),
                        static_cast<off_t>(page_size));
    ASSERT_EQ(second, second_addr);
    std::memcpy(second, private_tail, sizeof(private_tail));
    ASSERT_EQ(mprotect(second, page_size, PROT_READ | PROT_EXEC), 0);

    auto target = reinterpret_cast<int (*)()>(second_addr - 1);
    ASSERT_EQ(target(), 7);
    errno = 0;
    FdGuard event(open_uprobe_perf_event(path, page_size - 1));
    EXPECT_LT(event.get(), 0)
        << "a private instruction tail must not execute file bytes through XOL";
    EXPECT_EQ(errno, EINVAL);

    munmap(reservation, page_size * 2);
    unlink(path);
}

TEST(UprobeTest, NonExecutableTailIsSkippedUntilExecutable) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    constexpr unsigned char file_tail[] = {
        0xb8, 0x2a, 0x00, 0x00, 0x00,  // mov eax,42
        0xc3,                          // ret
    };

    char path[] = "/tmp/uprobe_nx_tail_XXXXXX";
    FdGuard file(mkstemp(path));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(ftruncate(file.get(), static_cast<off_t>(page_size * 2)), 0);
    ASSERT_EQ(pwrite(file.get(), file_tail, sizeof(file_tail),
                     static_cast<off_t>(page_size - 1)),
              static_cast<ssize_t>(sizeof(file_tail)));

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    ASSERT_EQ(mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                   MAP_PRIVATE | MAP_FIXED, file.get(), 0),
              reservation);
    auto* second_addr = static_cast<unsigned char*>(reservation) + page_size;
    void* second = mmap(second_addr, page_size, PROT_READ,
                        MAP_PRIVATE | MAP_FIXED, file.get(),
                        static_cast<off_t>(page_size));
    ASSERT_EQ(second, second_addr);

    FdGuard event(open_uprobe_perf_event(path, page_size - 1));
    ASSERT_GE(event.get(), 0)
        << "an NX alias should be skipped without rejecting the consumer";
    ASSERT_EQ(mprotect(second, page_size, PROT_READ | PROT_EXEC), 0);
    auto target = reinterpret_cast<int (*)()>(second_addr - 1);
    EXPECT_EQ(target(), 42)
        << "making the complete instruction executable must allow late apply";

    munmap(reservation, page_size * 2);
    unlink(path);
}

TEST(UprobeTest, AdjacentContinuousFileMappingsCanBeProbed) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    constexpr unsigned char file_tail[] = {
        0xb8, 0x2a, 0x00, 0x00, 0x00,  // mov eax,42
        0xc3,                          // ret
    };
    constexpr unsigned char replacement_tail[] = {
        0x07, 0x00, 0x00, 0x00,  // mov immediate after the first byte
        0xc3,                    // ret
    };

    char path[] = "/tmp/uprobe_cross_file_vma_XXXXXX";
    FdGuard file(mkstemp(path));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(ftruncate(file.get(), static_cast<off_t>(page_size * 2)), 0);
    ASSERT_EQ(pwrite(file.get(), file_tail, sizeof(file_tail),
                     static_cast<off_t>(page_size - 1)),
              static_cast<ssize_t>(sizeof(file_tail)));

    void* reservation = mmap(nullptr, page_size * 2, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation, MAP_FAILED);
    void* first = mmap(reservation, page_size, PROT_READ | PROT_EXEC,
                       MAP_PRIVATE | MAP_FIXED, file.get(), 0);
    ASSERT_EQ(first, reservation);
    auto* second_addr = static_cast<unsigned char*>(reservation) + page_size;
    void* second = mmap(second_addr, page_size, PROT_READ | PROT_EXEC,
                        MAP_PRIVATE | MAP_FIXED, file.get(),
                        static_cast<off_t>(page_size));
    ASSERT_EQ(second, second_addr);

    auto target = reinterpret_cast<int (*)()>(second_addr - 1);
    ASSERT_EQ(target(), 42);
    FdGuard event(open_uprobe_perf_event(path, page_size - 1));
    ASSERT_GE(event.get(), 0)
        << "continuous offsets across adjacent file VMAs must remain eligible";
    EXPECT_EQ(target(), 42);

    ASSERT_EQ(munmap(second, page_size), 0);
    second = mmap(second_addr, page_size, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    ASSERT_EQ(second, second_addr);
    std::memcpy(second, replacement_tail, sizeof(replacement_tail));
    ASSERT_EQ(mprotect(second, page_size, PROT_READ | PROT_EXEC), 0);
    EXPECT_EQ(target(), 7)
        << "changing only the instruction tail VMA must disarm the probe";

    munmap(reservation, page_size * 2);
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

    for (int i = 0; i < 128; ++i) {
        ASSERT_GE(ioctl(first_fd.get(), PERF_EVENT_IOC_DISABLE, 0), 0);
        EXPECT_EQ(uprobe_target(i), i * 2 + 1);
        ASSERT_GE(ioctl(first_fd.get(), PERF_EVENT_IOC_ENABLE, 0), 0);
        EXPECT_EQ(uprobe_target(i + 1), (i + 1) * 2 + 1);
    }

    __u64 first_count = 0;
    __u64 second_count = 0;
    ASSERT_EQ(read(first_fd.get(), &first_count, sizeof(first_count)),
              static_cast<ssize_t>(sizeof(first_count)));
    ASSERT_EQ(read(second_fd.get(), &second_count, sizeof(second_count)),
              static_cast<ssize_t>(sizeof(second_count)));
    EXPECT_EQ(first_count, 129U);
    EXPECT_EQ(second_count, 257U);

    first_fd.close_now();
    EXPECT_EQ(uprobe_target(32), 65)
        << "关闭一个 consumer 不应破坏剩余 consumer 的 XOL 路径";

    second_fd.close_now();
    for (int i = 0; i < 50; ++i) {
        EXPECT_EQ(uprobe_target(i + 400), (i + 400) * 2 + 1)
            << "最后一个 consumer 关闭后第 " << i << " 次执行错误";
    }
}

TEST(UprobeTest, PartialUnmapPreservesProbeMappingIdentity) {
    char path[] = "/tmp/uprobe_split_identity_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);
    constexpr size_t kPageSize = 4096;
    constexpr size_t kMappingSize = 3 * kPageSize;
    void* mapping = mmap(nullptr, kMappingSize, PROT_READ | PROT_EXEC,
                         MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(mapping, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    auto target = reinterpret_cast<int (*)(int)>(mapping);
    EXPECT_EQ(target(12), 25);

    // Removing an unrelated middle page splits the original VMA and replaces
    // the retained prefix with a new VMA object. The persistent probe still
    // belongs to the same file mapping and must survive disable/re-enable.
    ASSERT_EQ(munmap(static_cast<char*>(mapping) + kPageSize, kPageSize), 0);
    ASSERT_EQ(ioctl(event.get(), PERF_EVENT_IOC_DISABLE, 0), 0);
    ASSERT_EQ(ioctl(event.get(), PERF_EVENT_IOC_ENABLE, 0), 0)
        << "errno=" << errno;
    EXPECT_EQ(target(13), 27);

    munmap(mapping, kPageSize);
    munmap(static_cast<char*>(mapping) + 2 * kPageSize, kPageSize);
    unlink(path);
}

TEST(UprobeTest, DifferentAddressesOnSamePageRemainIndependent) {
    constexpr unsigned char two_targets[] = {
        0x8d, 0x44, 0x3f, 0x01,  // lea eax,[rdi+rdi+1]
        0xc3,                    // ret
        0x90, 0x90, 0x90,       // padding
        0x8d, 0x47, 0x03,  // lea eax,[rdi+3]
        0xc3,              // ret
    };
    constexpr unsigned long second_offset = 8;
    char path[] = "/tmp/uprobe_same_page_XXXXXX";
    FdGuard file(create_raw_code(path, two_targets, sizeof(two_targets)));
    ASSERT_GE(file.get(), 0);
    void* executable =
        mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);

    FdGuard first(open_uprobe_perf_event(path, 0));
    ASSERT_GE(first.get(), 0);
    FdGuard second(open_uprobe_perf_event(path, second_offset));
    ASSERT_GE(second.get(), 0);
    auto first_target = reinterpret_cast<int (*)(int)>(executable);
    auto second_target = reinterpret_cast<int (*)(int)>(
        static_cast<unsigned char*>(executable) + second_offset);
    EXPECT_EQ(first_target(9), 19);
    EXPECT_EQ(second_target(9), 12);

    second.close_now();
    EXPECT_EQ(first_target(10), 21);
    first.close_now();
    EXPECT_EQ(second_target(10), 13);

    munmap(executable, 4096);
    unlink(path);
}

TEST(UprobeTest, MoreThanOneXolPageOfDistinctSitesCanBeRegistered) {
    const long page_size_raw = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size_raw, 0);
    const size_t page_size = static_cast<size_t>(page_size_raw);
    constexpr size_t kProbeCount = 257;
    ASSERT_LT(kProbeCount, page_size);

    // Each NOP is a distinct valid instruction start. The old implementation
    // retained one XOL slot per site but provided only one 256-slot page, so
    // the final perf_event_open deterministically failed with ENOMEM.
    std::vector<unsigned char> code(page_size, 0x90);
    code[kProbeCount] = 0xc3;  // ret after the first slot on the second XOL page
    char path[] = "/tmp/uprobe_xol_growth_XXXXXX";
    FdGuard file(create_raw_code(path, code.data(), code.size()));
    ASSERT_GE(file.get(), 0);
    void* executable = mmap(nullptr, page_size, PROT_READ | PROT_EXEC,
                            MAP_PRIVATE, file.get(), 0);
    ASSERT_NE(executable, MAP_FAILED);

    std::vector<int> events;
    events.reserve(kProbeCount);
    int registration_errno = 0;
    for (size_t offset = 0; offset < kProbeCount; ++offset) {
        const int fd = open_uprobe_perf_event(path, offset);
        if (fd < 0) {
            registration_errno = errno;
            break;
        }
        events.push_back(fd);
    }

    EXPECT_EQ(events.size(), kProbeCount)
        << "registered=" << events.size() << ", errno=" << registration_errno;
    if (events.size() == kProbeCount) {
        auto second_page_target = reinterpret_cast<void (*)()>(
            static_cast<unsigned char*>(executable) + kProbeCount - 1);
        second_page_target();

        __u64 count = 0;
        const ssize_t count_bytes = read(events.back(), &count, sizeof(count));
        EXPECT_EQ(count_bytes, static_cast<ssize_t>(sizeof(count)));
        if (count_bytes == static_cast<ssize_t>(sizeof(count))) {
            EXPECT_EQ(count, 1U);
        }
    }
    for (int fd : events) close(fd);
    munmap(executable, page_size);
    unlink(path);
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

TEST(UprobeTest, FastSystemCallInstructionsAreRejected) {
    constexpr unsigned char fast_system_calls[] = {
        0x0f, 0x34,  // sysenter
        0x0f, 0x35,  // sysexit
    };
    constexpr unsigned long offsets[] = {0, 2};
    char path[] = "/tmp/uprobe_fast_syscall_XXXXXX";
    FdGuard file(
        create_raw_code(path, fast_system_calls, sizeof(fast_system_calls)));
    ASSERT_GE(file.get(), 0);

    for (unsigned long offset : offsets) {
        errno = 0;
        FdGuard event(open_uprobe_perf_event(path, offset));
        EXPECT_LT(event.get(), 0)
            << "XOL must reject SYSENTER/SYSEXIT at offset " << offset;
        EXPECT_EQ(errno, EINVAL) << "unexpected errno at offset " << offset;
    }
    unlink(path);
}

TEST(UprobeTest, RipRelativeAliasesUseReachableXolPages) {
    constexpr unsigned char code[] = {
        0x8b, 0x05, 0x02, 0x00, 0x00, 0x00,  // mov eax,[rip+2]
        0xc3,                                // ret
        0x90,                                // padding
        0x2a, 0x00, 0x00, 0x00,              // value 42
    };
    constexpr uintptr_t kFirstAddress = 0x2000000000ULL;
    constexpr uintptr_t kSecondAddress = 0x4000000000ULL;
    char path[] = "/tmp/uprobe_reachable_xol_XXXXXX";
    FdGuard file(create_raw_code(path, code, sizeof(code)));
    ASSERT_GE(file.get(), 0);

    void* first = mmap(reinterpret_cast<void*>(kFirstAddress), 4096,
                       PROT_READ | PROT_EXEC,
                       MAP_PRIVATE | MAP_FIXED_NOREPLACE, file.get(), 0);
    ASSERT_EQ(first, reinterpret_cast<void*>(kFirstAddress)) << "errno=" << errno;
    void* second = mmap(reinterpret_cast<void*>(kSecondAddress), 4096,
                        PROT_READ | PROT_EXEC,
                        MAP_PRIVATE | MAP_FIXED_NOREPLACE, file.get(), 0);
    if (second != reinterpret_cast<void*>(kSecondAddress)) {
        const int mapping_errno = errno;
        munmap(first, 4096);
        FAIL() << "second fixed alias failed, errno=" << mapping_errno;
    }

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0)
        << "each distant alias needs a disp32-reachable XOL slot, errno="
        << errno;
    EXPECT_EQ(reinterpret_cast<int (*)()>(first)(), 42);
    EXPECT_EQ(reinterpret_cast<int (*)()>(second)(), 42);
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 2U);

    munmap(second, 4096);
    munmap(first, 4096);
    unlink(path);
}

TEST(UprobeTest, PrefixedRipRelativeImm32RelocatesWithoutCorruption) {
    // cs; imul rax, qword ptr [rip+4], 5; ret; padding; qword 7
    // The decoder exposes the sign-extended immediate as 64-bit even though
    // its encoding is four bytes.  The displacement starts at byte four.
    const unsigned char code[] = {
        0x2e, 0x48, 0x69, 0x05, 0x04, 0x00, 0x00, 0x00,
        0x05, 0x00, 0x00, 0x00, 0xc3, 0x90, 0x90, 0x90,
        0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    };
    char path[] = "/tmp/uprobe_rip_imm32_XXXXXX";
    FdGuard file(create_raw_code(path, code, sizeof(code)));
    ASSERT_GE(file.get(), 0);
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE,
                         file.get(), 0);
    ASSERT_NE(mapping, MAP_FAILED);

    FdGuard event(open_uprobe_perf_event(path, 0));
    ASSERT_GE(event.get(), 0) << "errno=" << errno;
    auto target = reinterpret_cast<long (*)()>(mapping);
    EXPECT_EQ(target(), 35);
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U);

    munmap(mapping, 4096);
    unlink(path);
}

TEST(UprobeTest, LoopFamilyInstructionsAreRejected) {
    constexpr unsigned char loop_family[] = {
        0xe0, 0x00,        // loopnz rel8
        0xe1, 0x00,        // loopz rel8
        0xe2, 0x00,        // loop rel8
        0xe3, 0x00,        // jrcxz rel8
        0x67, 0xe3, 0x00,  // jecxz rel8 in 64-bit mode
    };
    constexpr unsigned long offsets[] = {0, 2, 4, 6, 8};
    char path[] = "/tmp/uprobe_loop_family_XXXXXX";
    FdGuard file(create_raw_code(path, loop_family, sizeof(loop_family)));
    ASSERT_GE(file.get(), 0);

    for (unsigned long offset : offsets) {
        errno = 0;
        FdGuard event(open_uprobe_perf_event(path, offset));
        EXPECT_LT(event.get(), 0)
            << "phase-1 XOL must reject LOOP-family control flow at offset "
            << offset;
        EXPECT_EQ(errno, EINVAL) << "unexpected errno at offset " << offset;
    }
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

TEST(UprobeTest, EipRelativeAddressingIsRejected) {
    constexpr unsigned char eip_relative[] = {
        0x67, 0x8b, 0x05, 0x00, 0x00, 0x00, 0x00,  // mov eax,[eip]
        0xc3,                                      // ret
    };
    char path[] = "/tmp/uprobe_eip_relative_XXXXXX";
    FdGuard file(create_raw_code(path, eip_relative, sizeof(eip_relative)));
    ASSERT_GE(file.get(), 0);

    errno = 0;
    FdGuard event(open_uprobe_perf_event(path, 0));
    EXPECT_LT(event.get(), 0);
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

TEST(UprobeTest, BpfReturnValueFiltersCounter) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));

    auto run_filtered_event = [&](int return_value) {
        FdGuard event(open_uprobe_perf_event(path, offset));
        EXPECT_GE(event.get(), 0);
        const std::vector<bpf_insn> instructions = {
            bpf_mov64_imm(return_value), bpf_exit()};
        FdGuard program(load_kprobe_bpf_program(instructions));
        EXPECT_GE(program.get(), 0);
        EXPECT_EQ(ioctl(event.get(), PERF_EVENT_IOC_SET_BPF, program.get()), 0);
        EXPECT_EQ(uprobe_target(11), 23);
        __u64 count = ~0ULL;
        EXPECT_EQ(read(event.get(), &count, sizeof(count)),
                  static_cast<ssize_t>(sizeof(count)));
        return count;
    };

    EXPECT_EQ(run_filtered_event(0), 0U);
    EXPECT_EQ(run_filtered_event(1), 1U);
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

#if defined(__x86_64__)
TEST(UprobeTest, RseqPreemptionUsesOriginalProbeIp) {
    static_assert(sizeof(TestRseqAbi) == 32);
    static_assert(sizeof(TestRseqCs) == 32);

    RseqUprobeChildArgs child_args;
    child_args.critical_section.start_ip =
        reinterpret_cast<uintptr_t>(rseq_uprobe_start);
    child_args.critical_section.post_commit_offset =
        reinterpret_cast<uintptr_t>(rseq_uprobe_post_commit) -
        child_args.critical_section.start_ip;
    child_args.critical_section.abort_ip =
        reinterpret_cast<uintptr_t>(rseq_uprobe_abort);
    child_args.attempts = 200000;

    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(rseq_uprobe_probe, path, offset));
    UprobePerfEventOptions options;
    options.pid = -1;
    options.cpu = 0;
    FdGuard event(open_uprobe_perf_event(path, offset, options));
    ASSERT_GE(event.get(), 0) << "rseq uprobe open failed, errno=" << errno;

    // Keep interrupts enabled in the #BP callback long enough for a scheduler
    // tick to request a context switch before the handler publishes XOL state.
    // The old implementation then exposed the XOL RIP to rseq and missed the
    // mandatory abort. Repetition avoids depending on one exact timer phase.
    std::vector<bpf_insn> instructions(600, bpf_mov64_imm(1));
    instructions.push_back(bpf_exit());
    FdGuard program(load_kprobe_bpf_program(instructions));
    ASSERT_GE(program.get(), 0) << "BPF_PROG_LOAD failed, errno=" << errno;
    ASSERT_EQ(ioctl(event.get(), PERF_EVENT_IOC_SET_BPF, program.get()), 0);

    std::atomic<bool> stop{false};
    std::atomic<int> competitor_ready{0};
    std::thread competitor([&] {
        cpu_set_t competitor_affinity;
        CPU_ZERO(&competitor_affinity);
        CPU_SET(0, &competitor_affinity);
        competitor_ready.store(
            sched_setaffinity(0, sizeof(competitor_affinity),
                              &competitor_affinity) == 0
                ? 1
                : -1,
            std::memory_order_release);
        while (!stop.load(std::memory_order_relaxed)) sched_yield();
    });
    while (competitor_ready.load(std::memory_order_acquire) == 0)
        sched_yield();
    if (competitor_ready.load(std::memory_order_acquire) < 0) {
        stop.store(true, std::memory_order_relaxed);
        competitor.join();
        GTEST_SKIP() << "CPU 0 is unavailable to the competitor";
    }

    constexpr size_t child_stack_size = 1 << 20;
    void* child_stack =
        mmap(nullptr, child_stack_size, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
    if (child_stack == MAP_FAILED) {
        const int mmap_errno = errno;
        stop.store(true, std::memory_order_relaxed);
        competitor.join();
        FAIL() << "child stack mmap failed, errno=" << mmap_errno;
    }
    const pid_t child =
        clone(rseq_uprobe_child,
              static_cast<unsigned char*>(child_stack) + child_stack_size,
              CLONE_VM | SIGCHLD, &child_args);
    if (child <= 0) {
        const int clone_errno = errno;
        munmap(child_stack, child_stack_size);
        stop.store(true, std::memory_order_relaxed);
        competitor.join();
        FAIL() << "CLONE_VM failed, errno=" << clone_errno;
    }
    while (child_args.registration.load(std::memory_order_acquire) == 0)
        sched_yield();
    if (child_args.registration.load(std::memory_order_acquire) < 0) {
        int status = 0;
        const pid_t waited = reap_child(child, &status);
        if (waited >= 0) munmap(child_stack, child_stack_size);
        stop.store(true, std::memory_order_relaxed);
        competitor.join();
        ASSERT_GE(waited, 0) << "failed to reap rseq child, errno=" << errno;
        GTEST_SKIP() << "raw CLONE_VM child could not register rseq";
    }

    child_args.start.store(true, std::memory_order_release);
    int status = 0;
    const pid_t waited = reap_child(child, &status);
    stop.store(true, std::memory_order_relaxed);
    competitor.join();
    if (waited >= 0) munmap(child_stack, child_stack_size);
    ASSERT_EQ(waited, child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);

    const __u64 aborts =
        child_args.aborts.load(std::memory_order_acquire);
    __u64 hits = 0;
    ASSERT_EQ(read(event.get(), &hits, sizeof(hits)),
              static_cast<ssize_t>(sizeof(hits)));
    // An attempt can finish normally (one hit), abort before reaching the
    // probe (no hit), or abort after #BP while XOL is active (one hit). Thus
    // hits + aborts exceeds attempts iff the rseq/XOL race was exercised.
    EXPECT_GT(hits + aborts, static_cast<__u64>(child_args.attempts))
        << "preemption between #BP and XOL must restart the rseq section";
}
#endif

#if defined(__x86_64__)
// A task-scoped event belongs to the original task and is not inherited by a
// fork child when perf inherit is disabled. The child nevertheless initially
// receives the parent's private breakpoint page through COW, so fork must
// sanitize that INT3 before allowing the child to execute without a hit-table
// entry. The parent's site must remain armed throughout the transaction.
TEST(UprobeTest, TaskScopedForkDoesNotInheritButSanitizes) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));

    FdGuard event(open_uprobe_perf_event(path, offset));
    ASSERT_GE(event.get(), 0) << "task uprobe failed, errno=" << errno;

    EXPECT_EQ(uprobe_target(40), 81);
    __u64 count = 0;
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    ASSERT_EQ(count, 1U);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed, errno=" << errno;
    if (child == 0) {
        for (int i = 0; i < 256; ++i) {
            if (uprobe_target(i) != i * 2 + 1) _exit(1);
        }
        _exit(0);
    }

    int status = 0;
    const pid_t waited = reap_child(child, &status);
    ASSERT_EQ(waited, child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0)
        << "fork child encountered an inherited task-scoped breakpoint";

    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 1U) << "task-scoped event counted fork-child hits";

    EXPECT_EQ(uprobe_target(41), 83);
    ASSERT_EQ(read(event.get(), &count, sizeof(count)),
              static_cast<ssize_t>(sizeof(count)));
    EXPECT_EQ(count, 2U) << "fork sanitization disarmed the parent's site";
}
#endif

// A system-wide observer must not turn a valid fork into an error when one
// private executable mapping no longer matches the backing-file definition.
// Ordinary VMA reconciliation already treats that alias as best-effort; fork
// must preserve the same Linux-visible behavior after sanitizing inherited
// breakpoint pages.
TEST(UprobeTest, PrivateInstructionMismatchDoesNotFailFork) {
    char path[] = "/tmp/uprobe_fork_private_XXXXXX";
    FdGuard file(create_raw_target(path));
    ASSERT_GE(file.get(), 0);

    UprobePerfEventOptions options;
    options.pid = -1;
    options.cpu = 0;
    FdGuard event(open_uprobe_perf_event(path, 0, options));
    ASSERT_GE(event.get(), 0) << "system-wide uprobe failed, errno=" << errno;

    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                         file.get(), 0);
    ASSERT_NE(mapping, MAP_FAILED);
    static_cast<unsigned char*>(mapping)[3] = 2;  // lea eax,[rdi+rdi+2]
    ASSERT_EQ(mprotect(mapping, 4096, PROT_READ | PROT_EXEC), 0);
    auto target = reinterpret_cast<int (*)(int)>(mapping);
    ASSERT_EQ(target(5), 12);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "a best-effort uprobe replay must not fail fork, errno="
                        << errno;
    if (child == 0) {
        _exit(target(7) == 16 ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);

    munmap(mapping, 4096);
    unlink(path);
}

// A forked child initially shares the parent's private breakpoint page.  A
// concurrent system-wide registration must never observe the child's VMA
// before the inherited INT3 has been reconciled with its empty hit table.
TEST(UprobeTest, ConcurrentSystemWideRegistrationDuringFork) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
        reinterpret_cast<const void*>(&uprobe_target), path, offset));

    FdGuard task_event(open_uprobe_perf_event(path, offset));
    ASSERT_GE(task_event.get(), 0) << "task uprobe failed, errno=" << errno;

    UprobePerfEventOptions system_options;
    system_options.pid = -1;
    system_options.cpu = 0;
    system_options.disabled = true;
    FdGuard system_event(
        open_uprobe_perf_event(path, offset, system_options));
    ASSERT_GE(system_event.get(), 0)
        << "disabled system-wide uprobe failed, errno=" << errno;

    std::atomic<bool> start{false};
    std::atomic<bool> ready{false};
    std::atomic<bool> stop{false};
    std::atomic<int> registration_errno{0};
    std::atomic<unsigned int> registrations{0};
    std::atomic<unsigned int> fork_epoch{0};
    std::thread registrar([&]() {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        ready.store(true, std::memory_order_release);
        unsigned int observed_epoch = 0;
        while (!stop.load(std::memory_order_acquire)) {
            const unsigned int current_epoch =
                fork_epoch.load(std::memory_order_acquire);
            if (current_epoch == observed_epoch) {
                std::this_thread::yield();
                continue;
            }
            observed_epoch = current_epoch;
            if (ioctl(system_event.get(), PERF_EVENT_IOC_ENABLE, 0) < 0) {
                registration_errno.store(errno, std::memory_order_release);
                break;
            }
            registrations.fetch_add(1, std::memory_order_relaxed);
            if (ioctl(system_event.get(), PERF_EVENT_IOC_DISABLE, 0) < 0) {
                registration_errno.store(errno, std::memory_order_release);
                break;
            }
        }
    });

    start.store(true, std::memory_order_release);
    while (!ready.load(std::memory_order_acquire)) {
        std::this_thread::yield();
    }
    int child_failure = 0;
    for (int i = 0; i < 200 &&
                    registration_errno.load(std::memory_order_acquire) == 0;
         ++i) {
        fork_epoch.fetch_add(1, std::memory_order_release);
        const pid_t child = fork();
        if (child < 0) {
            child_failure = errno;
            break;
        }
        if (child == 0) {
            _exit(uprobe_target(i) == i * 2 + 1 ? 0 : 1);
        }
        int status = 0;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            child_failure = ECHILD;
            break;
        }
    }
    stop.store(true, std::memory_order_release);
    registrar.join();

    EXPECT_GT(registrations.load(std::memory_order_relaxed), 0U);
    EXPECT_EQ(child_failure, 0) << "forked child failed";
    EXPECT_EQ(registration_errno.load(std::memory_order_acquire), 0)
        << "concurrent system-wide registration failed";
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
