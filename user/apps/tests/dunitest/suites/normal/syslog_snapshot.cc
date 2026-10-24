#include <gtest/gtest.h>
#include <fcntl.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <atomic>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cstdint>
#include <string>
#include <thread>
#include <vector>

namespace {
constexpr int kReadAll = 3, kReadClear = 4, kClear = 5;
constexpr size_t kCapacity = 1024 * 1024;
const char* kWarning = "Failed to produce filesystem";

int Read(int action, char* buffer, int len) {
    return syscall(SYS_syslog, action, buffer, len);
}
std::string Snapshot(int action = kReadAll, size_t size = kCapacity) {
    std::vector<char> buffer(size);
    int count = Read(action, buffer.data(), buffer.size());
    EXPECT_GE(count, 0) << strerror(errno);
    if (count < 0) return {};
    EXPECT_LE(static_cast<size_t>(count), size);
    if (static_cast<size_t>(count) > size) return {};
    return std::string(buffer.data(), count);
}
class SyslogSnapshot : public ::testing::Test {
  protected:
    void SetUp() override {
        alarm(30);
        // The generator uses the existing tmpfs parse-error diagnostic; no
        // debug syscall, log injection interface, or kernel test hook is needed.
        char path[] = "/tmp/syslog-snapshot-XXXXXX";
        char* made = mkdtemp(path);
        ASSERT_NE(nullptr, made);
        directory = made;
        int policy = open("/proc/sys/kernel/printk", O_RDONLY | O_CLOEXEC);
        ASSERT_GE(policy, 0);
        char text[128];
        ssize_t size = read(policy, text, sizeof(text));
        EXPECT_EQ(0, close(policy));
        ASSERT_GT(size, 0);
        console_policy.assign(text, size);
    }
    void TearDown() override {
        if (restore_console) {
            int policy = open("/proc/sys/kernel/printk", O_WRONLY | O_CLOEXEC);
            EXPECT_GE(policy, 0);
            if (policy >= 0) {
                EXPECT_EQ(static_cast<ssize_t>(console_policy.size()),
                          write(policy, console_policy.data(), console_policy.size()));
                EXPECT_EQ(0, close(policy));
            }
        }
        if (!directory.empty()) rmdir(directory.c_str());
        alarm(0);
    }
    void Emit() {
        errno = 0;
        ASSERT_EQ(-1, mount("tmpfs", directory.c_str(), "tmpfs", 0,
                            "size=not-a-number"));
        ASSERT_EQ(EINVAL, errno);
    }
    std::string directory, console_policy;
    bool restore_console = false;
};
TEST_F(SyslogSnapshot, FirstReadDoesNotNeedProcOrSizeWarmup) {
    Emit();
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, LaterRecordsRefreshTheSnapshot) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Snapshot();
    Emit();
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ReadClearReturnsCurrentRecordsAndClearsVisibility) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    EXPECT_NE(std::string::npos, Snapshot(kReadClear).find(kWarning));
    EXPECT_EQ(std::string::npos, Snapshot().find(kWarning));
    Emit();
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ZeroLengthReadClearDoesNotConsumeRecords) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    char byte = 0;
    EXPECT_EQ(0, Read(kReadClear, &byte, 0));
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ShortBuffersContainOnlyCompleteNewestRecords) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    std::string full = Snapshot();
    ASSERT_FALSE(full.empty());
    EXPECT_TRUE(Snapshot(kReadAll, 1).empty());
    std::string tail = Snapshot(kReadAll, full.size());
    ASSERT_FALSE(tail.empty());
    EXPECT_EQ('\n', tail.back());
    EXPECT_EQ('<', tail.front());
    EXPECT_NE(std::string::npos, tail.find(kWarning));
}
TEST_F(SyslogSnapshot, ClearKeepsTheProcReadHistory) {
    Emit();
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    EXPECT_EQ(std::string::npos, Snapshot().find(kWarning));
    int fd = open("/proc/kmsg", O_RDONLY);
    ASSERT_GE(fd, 0);
    std::vector<char> buffer(kCapacity);
    ssize_t count = read(fd, buffer.data(), buffer.size());
    EXPECT_EQ(0, close(fd));
    ASSERT_GE(count, 0);
    EXPECT_NE(std::string::npos, std::string(buffer.data(), count).find(kWarning));
}
TEST_F(SyslogSnapshot, CopyFaultDoesNotClearTheSnapshot) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    void* page = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, page);
    errno = 0;
    EXPECT_EQ(-1, Read(kReadClear, static_cast<char*>(page), 4096));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, munmap(page, 4096));
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ReadActionsRejectNullEvenAtZeroLength) {
    for (int action : {2, kReadAll, kReadClear}) {
        errno = 0;
        EXPECT_EQ(-1, Read(action, nullptr, 0));
        EXPECT_EQ(EINVAL, errno);
    }
}
TEST_F(SyslogSnapshot, ReadActionsRejectNegativeIntLength) {
    char byte = 0;
    for (int action : {2, kReadAll, kReadClear}) {
        errno = 0;
        EXPECT_EQ(-1, Read(action, &byte, -1));
        EXPECT_EQ(EINVAL, errno);
    }
}
TEST_F(SyslogSnapshot, NonDataActionsIgnoreTheBufferArgument) {
    EXPECT_EQ(0, Read(0, reinterpret_cast<char*>(1), 4096));
    EXPECT_EQ(0, Read(1, reinterpret_cast<char*>(1), 4096));
    EXPECT_EQ(0, Read(kClear, reinterpret_cast<char*>(1), 4096));
}
TEST_F(SyslogSnapshot, NonzeroShortReadClearAdvancesTheVisibilityBoundary) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    char byte = 0;
    EXPECT_EQ(0, Read(kReadClear, &byte, 1));
    EXPECT_EQ(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ReadClearFaultCommitsOnlyCompleteEarlierRecords) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    std::string first = Snapshot();
    // A record may contain embedded newlines. Capture the whole first
    // record before appending another instead of parsing a line boundary.
    size_t boundary = first.size();
    ASSERT_GT(boundary, 0u);
    Emit();
    std::string before = Snapshot();
    ASSERT_EQ(first, before.substr(0, boundary));
    ASSERT_LT(boundary, before.size());
    ASSERT_LT(before.size(), 4096u);
    char* pages = static_cast<char*>(mmap(nullptr, 8192, PROT_READ | PROT_WRITE,
                                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, pages);
    ASSERT_EQ(0, mprotect(pages + 4096, 4096, PROT_NONE));
    errno = 0;
    EXPECT_EQ(-1, Read(kReadClear, pages + 4096 - boundary, before.size()));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(before.substr(0, boundary),
              std::string(pages + 4096 - boundary, boundary));
    EXPECT_EQ(0, munmap(pages, 8192));
    // Adjacent records can have identical timestamps and text. Compare the
    // complete retained suffix, rather than treating text as record identity.
    // Returning EFAULT may itself append a syscall-return DEBUG record.
    // Preserve the original suffix exactly, while requiring that the first
    // warning really was cleared even if the two warning texts are identical.
    const std::string suffix = before.substr(boundary);
    const std::string after = Snapshot();
    ASSERT_GE(after.size(), suffix.size());
    EXPECT_EQ(suffix, after.substr(0, suffix.size()));
    const size_t warning = after.find(kWarning);
    ASSERT_NE(std::string::npos, warning);
    EXPECT_EQ(std::string::npos, after.find(kWarning, warning + strlen(kWarning)));
}
TEST_F(SyslogSnapshot, OutOfRangeReadClearFailsBeforeAdvancingVisibility) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Emit();
    char* outside = reinterpret_cast<char*>(~uintptr_t{0} - 4095);
    errno = 0;
    EXPECT_EQ(-1, Read(kReadClear, outside, 1));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
TEST_F(SyslogSnapshot, ClearPreservesTheBufferCapacity) {
    Emit();
    int before = Read(10, nullptr, 1);
    ASSERT_GT(before, 0);
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    EXPECT_GE(Read(10, nullptr, 1), before);
}
TEST_F(SyslogSnapshot, ConsoleLevelDoesNotFilterProcRead) {
    restore_console = true;
    Emit();
    int fd = open("/proc/kmsg", O_RDONLY);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(0, Read(8, nullptr, 5));
    char byte = 0;
    EXPECT_EQ(1, read(fd, &byte, 1));
    EXPECT_EQ('<', byte);
    EXPECT_EQ(0, close(fd));
}
TEST_F(SyslogSnapshot, ConcurrentReadersAndWritersKeepCompleteSnapshots) {
    ASSERT_EQ(0, Read(kClear, nullptr, 0));
    Snapshot();
    std::atomic<bool> done{false};
    std::thread writer([&] {
        for (int i = 0; i != 64; ++i) Emit();
        done = true;
    });
    size_t reads = 0;
    do {
        std::string snapshot = Snapshot(kReadAll, 4096);
        if (!snapshot.empty()) {
            EXPECT_EQ('<', snapshot.front());
            EXPECT_EQ('\n', snapshot.back());
        }
        ++reads;
    } while (!done.load() || reads < 4);
    writer.join();
    EXPECT_NE(std::string::npos, Snapshot().find(kWarning));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
