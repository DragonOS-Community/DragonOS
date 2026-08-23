// /proc/<pid>/mem 对 MAP_SHARED 页写入的可见性回归测试。
//
// 覆盖语义：
// - 临时文件 ftruncate 一页后 MAP_SHARED 映射；
// - tracee 在 SIGSTOP 前完成映射，父进程（root tracer）通过
//   PTRACE_POKEDATA 改映射区偏移 A、/proc/<pid>/mem pwrite 改偏移 B；
// - 用独立 fd（不经映射、经页缓存）pread 两处偏移均应读到新值——
//   证明两条远程写路径都落到页缓存脏发布，而非 tracee 私有 COW 副本；
// - MAP_PRIVATE 对照：POKEDATA 只改 tracee 私有副本，文件内容不变。
//
// x86_64 user 结构偏移：offsetof(struct user, u_debugreg[n]) = 848 + 8*n
// （本测试不用调试寄存器，仅引用此约定作注释说明）。

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

// ptrace 请求号（编译环境头文件缺失时兜底，取值与 Linux x86_64 ABI 一致）
#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_POKEDATA
#define PTRACE_POKEDATA 5
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif

namespace {

constexpr size_t kPageSize = 4096;
// 偏移 A：由 PTRACE_POKEDATA 修改；偏移 B：由 /proc/<pid>/mem pwrite 修改。
constexpr size_t kOffsetA = 0;
constexpr size_t kOffsetB = 4096 - sizeof(uint64_t);

long ptrace_call(long request, long pid, unsigned long addr, unsigned long data) {
    errno = 0;
    return syscall(SYS_ptrace, request, pid, addr, data);
}

// tracee 把映射地址写回父进程（共享内存文件，同时充当结果载体）。
// 约定：映射文件第一页 word 0 留给父进程读取映射地址本身——因此
// 数据断言用独立文件 fd 而非映射内容，避免自举混淆。
struct SharedLayout {
    uint64_t shared_map_addr;   // tracee 填 MAP_SHARED 地址
    uint64_t private_map_addr;  // tracee 填 MAP_PRIVATE 地址
};

// 用独立 fd 读 8 字节。
bool pread_word(int fd, size_t offset, uint64_t* out) {
    ssize_t n = pread(fd, out, sizeof(uint64_t), (off_t)offset);
    return n == (ssize_t)sizeof(uint64_t);
}

}  // namespace

TEST(ProcMemShared, SharedMappingWritesReachPageCachePrivateDoesNot) {
    // 数据文件：被 MAP_SHARED / MAP_PRIVATE 映射的对象。
    char data_path[] = "/tmp/dunitest_proc_mem_shared_XXXXXX";
    int data_fd = mkstemp(data_path);
    ASSERT_GE(data_fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(data_path);
    ASSERT_EQ(0, ftruncate(data_fd, (off_t)kPageSize));

    // 同步文件：tracee 写回两个映射地址。
    char sync_path[] = "/tmp/dunitest_proc_mem_sync_XXXXXX";
    int sync_fd = mkstemp(sync_path);
    ASSERT_GE(sync_fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(sync_path);
    ASSERT_EQ(0, ftruncate(sync_fd, (off_t)kPageSize));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        void* shared_map = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, data_fd, 0);
        if (shared_map == MAP_FAILED) {
            _exit(40);
        }
        void* private_map =
            mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_PRIVATE, data_fd, 0);
        if (private_map == MAP_FAILED) {
            _exit(41);
        }
        // 映射地址写回父进程（sync 文件自身 MAP_SHARED）。
        SharedLayout layout;
        layout.shared_map_addr = reinterpret_cast<uint64_t>(shared_map);
        layout.private_map_addr = reinterpret_cast<uint64_t>(private_map);
        if (pwrite(sync_fd, &layout, sizeof(layout), 0) != (ssize_t)sizeof(layout)) {
            _exit(42);
        }
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
            _exit(43);
        }
        if (raise(SIGSTOP) != 0) {
            _exit(44);
        }
        // DETACH 后退出。
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status)) << "child status=" << status;

    // 读回 tracee 的映射地址。
    SharedLayout layout = {};
    ASSERT_EQ((ssize_t)sizeof(layout), pread(sync_fd, &layout, sizeof(layout), 0));
    ASSERT_NE(0u, layout.shared_map_addr);
    ASSERT_NE(0u, layout.private_map_addr);

    // ---- 路径一：PTRACE_POKEDATA 写 MAP_SHARED 区偏移 A ----
    const uint64_t value_a = 0xA5A5A5A55A5A5A5AULL;
    errno = 0;
    ASSERT_EQ(0L, ptrace_call(PTRACE_POKEDATA, child,
                              layout.shared_map_addr + kOffsetA, value_a))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    // ---- 路径二：/proc/<pid>/mem pwrite 写 MAP_SHARED 区偏移 B ----
    char mem_path[64];
    snprintf(mem_path, sizeof(mem_path), "/proc/%d/mem", (int)child);
    int mem_fd = open(mem_path, O_RDWR);
    ASSERT_GE(mem_fd, 0) << "open(" << mem_path << ") failed: errno=" << errno << " ("
                         << strerror(errno) << ")";
    const uint64_t value_b = 0x0BAD0BAD0BAD0BADULL;
    ASSERT_EQ((ssize_t)sizeof(value_b),
              pwrite(mem_fd, &value_b, sizeof(value_b), (off_t)(layout.shared_map_addr + kOffsetB)))
        << "errno=" << errno << " (" << strerror(errno) << ")";
    close(mem_fd);

    // ---- 对照：POKEDATA 写 MAP_PRIVATE 区，文件内容必须不变 ----
    const uint64_t value_p = 0xDEADBEEFDEADBEEFULL;
    ASSERT_EQ(0L, ptrace_call(PTRACE_POKEDATA, child,
                              layout.private_map_addr + kOffsetA, value_p));

    // ---- 断言：data 文件自身 fd 即页缓存事实源，两处偏移读回新值 ----

    uint64_t word_a = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetA, &word_a));
    EXPECT_EQ(value_a, word_a) << "POKEDATA 写 MAP_SHARED 应直达页缓存";

    uint64_t word_b = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetB, &word_b));
    EXPECT_EQ(value_b, word_b) << "/proc/mem pwrite 写 MAP_SHARED 应直达页缓存";

    // MAP_PRIVATE 对照：文件内容不得改变（COW 副本对文件不可见）。
    uint64_t word_p = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetA, &word_p));
    // 注意 kOffsetA 与上面 word_a 是同一偏移——MAP_PRIVATE 写的是另一个映射，
    // 检查同一文件偏移仍为 value_a（未被私有副本污染）即可。
    EXPECT_EQ(value_a, word_p) << "POKEDATA 写 MAP_PRIVATE 不得改动文件内容";

    // ---- 清理 ----
    ASSERT_EQ(0L, ptrace_call(PTRACE_DETACH, child, 0, 0));
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(data_fd);
    close(sync_fd);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
