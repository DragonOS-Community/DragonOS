// uprobe 断点探针端到端测试（issue #2150 阶段一）。
//
// 验证用户态经 perf_event_open 挂载 uprobe、触发被探测函数后进程存活
// （#BP → XOL 单步 → #DB → 恢复 的命中路径不崩溃），并覆盖错误入参路径。
//
// 内核侧接口（kernel/src/perf/uprobe.rs）：
//   - perf_event_attr.type  = PERF_TYPE_MAX(6)
//   - perf_event_attr.config1 = 目标二进制路径（含 '/' 触发 uprobe 分发）
//   - perf_event_attr.config2 = 文件偏移
//   - syscall 参数 pid：0=当前进程，>0=指定进程，-1=全量

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <string>

#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <linux/perf_event.h>

#ifndef SYS_perf_event_open
#include <asm/unistd.h>
#define SYS_perf_event_open __NR_perf_event_open
#endif

namespace {

// DragonOS 内核用 PERF_TYPE_MAX(=6) 分发 kprobe/uprobe；config1(name) 含 '/'
// 即视为 uprobe（路径），config2 为文件偏移。
constexpr __u64 UPROBE_PERF_TYPE = 6;  // PERF_TYPE_MAX

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
                           pid_t pid) {
    struct perf_event_attr pe;
    std::memset(&pe, 0, sizeof(pe));
    pe.size = sizeof(pe);
    pe.type = UPROBE_PERF_TYPE;
    pe.config1 = reinterpret_cast<__u64>(path.c_str());  // name = 路径（含 '/'）
    pe.config2 = offset;                                 // 文件偏移
    // pid 通过 syscall 参数传递（与 attr 一致）
    return static_cast<int>(
        syscall(SYS_perf_event_open, &pe, pid, -1, -1, 0));
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

    int fd = open_uprobe_perf_event(path, offset, 0);  // pid=0：当前进程
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
    int fd = open_uprobe_perf_event("/nonexistent/path/to/binary", 0, 0);
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
    int fd = open_uprobe_perf_event(path, 0xFFFFFFFFFFFFULL, 0);
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

    int fd = open_uprobe_perf_event(path, offset, 0);
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
        int fd = open_uprobe_perf_event(path, offset, 0);
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

// disabled 状态下 0xcc 仍在、XOL 单步仍执行原指令（仅跳过回调）。
// 因此函数仍应正确返回——验证 disable 不破坏原指令执行路径。
TEST(UprobeTest, DisabledStillReturnsCorrectly) {
    std::string path;
    unsigned long offset = 0;
    ASSERT_TRUE(resolve_file_offset(
                    reinterpret_cast<const void*>(&uprobe_target), path, offset))
        << "无法解析目标函数偏移";

    int fd = open_uprobe_perf_event(path, offset, 0);
    ASSERT_GE(fd, 0) << "perf_event_open(uprobe) 失败，errno=" << errno;
    // 注册即 enable（perf 默认），先 disable 再测试
    ASSERT_GE(ioctl(fd, PERF_EVENT_IOC_DISABLE, 0), 0);

    // disabled 期间：0xcc 仍命中，但 XOL 单步执行原指令，函数结果正确。
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
