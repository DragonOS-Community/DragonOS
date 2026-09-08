#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <atomic>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <linux/futex.h>
#include <signal.h>
#include <string>
#include <sys/mman.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

namespace {
size_t PageSize() { return static_cast<size_t>(sysconf(_SC_PAGESIZE)); }

struct Mapping {
    void* addr = MAP_FAILED;
    size_t size = 0;
    explicit Mapping(size_t length) : size(length) {}
    ~Mapping() { if (addr != MAP_FAILED) munmap(addr, size); }
    Mapping(const Mapping&) = delete;
    Mapping& operator=(const Mapping&) = delete;
    volatile unsigned char* bytes() const {
        return static_cast<volatile unsigned char*>(addr);
    }
};

struct MapInfo {
    unsigned long long inode = 0;
    unsigned long long offset = 0;
    std::string line;
};
bool ReadMap(void* address, MapInfo* info) {
    FILE* file = fopen("/proc/self/maps", "r");
    if (!file) return false;
    char line[1024];
    bool found = false;
    while (fgets(line, sizeof(line), file)) {
        unsigned long start, end;
        char perms[5], dev[32];
        unsigned long long offset, inode;
        if (sscanf(line, "%lx-%lx %4s %llx %31s %llu", &start, &end, perms,
                   &offset, dev, &inode) == 6 &&
            start <= reinterpret_cast<uintptr_t>(address) &&
            reinterpret_cast<uintptr_t>(address) < end) {
            *info = {inode, offset, line};
            found = true;
            break;
        }
    }
    fclose(file);
    return found;
}

// Fork keeps a fault assertion from terminating the test binary.
int ReadInChild(volatile unsigned char* address) {
    pid_t child = fork();
    if (child < 0) return -1;
    if (child == 0) {
        const unsigned char value = *address;
        _exit(value == 0 ? 0 : 1);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) return -1;
    return status;
}

class InternalShmem : public testing::TestWithParam<bool> {
  protected:
    int zero_fd_ = -1;
    void SetUp() override {
        if (GetParam()) {
            zero_fd_ = open("/dev/zero", O_RDWR);
            ASSERT_GE(zero_fd_, 0) << strerror(errno);
        }
    }
    void TearDown() override { if (zero_fd_ >= 0) close(zero_fd_); }
    void Map(Mapping& mapping, void* fixed = nullptr) {
        int flags = MAP_SHARED | (GetParam() ? 0 : MAP_ANONYMOUS);
        if (fixed) flags |= MAP_FIXED;
        mapping.addr = mmap(fixed, mapping.size, PROT_READ | PROT_WRITE, flags, zero_fd_, 0);
    }
};

TEST_P(InternalShmem, IndependentMappingsHaveDistinctDeletedInodes) {
    Mapping first(PageSize()), second(PageSize());
    Map(first); Map(second);
    ASSERT_NE(first.addr, MAP_FAILED); ASSERT_NE(second.addr, MAP_FAILED);
    MapInfo a, b;
    ASSERT_TRUE(ReadMap(first.addr, &a)); ASSERT_TRUE(ReadMap(second.addr, &b));
    EXPECT_NE(a.inode, 0u); EXPECT_NE(b.inode, 0u); EXPECT_NE(a.inode, b.inode);
    EXPECT_NE(a.line.find("/dev/zero (deleted)"), std::string::npos) << a.line;
    EXPECT_NE(b.line.find("/dev/zero (deleted)"), std::string::npos) << b.line;
    first.bytes()[0] = 31;
    EXPECT_EQ(second.bytes()[0], 0);
    if (GetParam()) {
        int another = open("/dev/zero", O_RDWR);
        ASSERT_GE(another, 0);
        Mapping third(PageSize());
        third.addr = mmap(nullptr, third.size, PROT_READ | PROT_WRITE, MAP_SHARED, another, 0);
        close(another);
        ASSERT_NE(third.addr, MAP_FAILED);
        MapInfo c;
        ASSERT_TRUE(ReadMap(third.addr, &c));
        EXPECT_NE(a.inode, c.inode); EXPECT_NE(b.inode, c.inode);
        EXPECT_EQ(third.bytes()[0], 0);
    }
}

TEST_P(InternalShmem, ChildFaultPublishesOneSparsePageAndSyncSucceeds) {
    const size_t page = PageSize();
    Mapping mapping(8 * page);
    Map(mapping); ASSERT_NE(mapping.addr, MAP_FAILED);
    unsigned char resident[8] = {};
    ASSERT_EQ(mincore(mapping.addr, mapping.size, resident), 0);
    for (auto value : resident) EXPECT_EQ(value & 1, 0);
    pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) {
        if (mapping.bytes()[3 * page] != 0) _exit(1);
        mapping.bytes()[3 * page] = 73;
        _exit(0);
    }
    int status = 0; ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status)); ASSERT_EQ(WEXITSTATUS(status), 0);
    ASSERT_EQ(mincore(mapping.addr, mapping.size, resident), 0);
    for (size_t i = 0; i < 8; ++i) EXPECT_EQ(resident[i] & 1, i == 3 ? 1 : 0);
    EXPECT_EQ(mapping.bytes()[3 * page], 73);
    EXPECT_EQ(msync(mapping.addr, mapping.size, MS_SYNC), 0) << strerror(errno);
}

TEST_P(InternalShmem, PopulateAndProtectionChangesKeepTheSameBacking) {
    const size_t page = PageSize();
    Mapping source(8 * page), alias(8 * page);
    const int flags = MAP_SHARED | MAP_POPULATE | (GetParam() ? 0 : MAP_ANONYMOUS);
    source.addr = mmap(nullptr, source.size, PROT_READ | PROT_WRITE, flags, zero_fd_, 0);
    ASSERT_NE(source.addr, MAP_FAILED) << strerror(errno);
    unsigned char resident[8] = {};
    ASSERT_EQ(mincore(source.addr, source.size, resident), 0);
    for (auto value : resident) EXPECT_EQ(value & 1, 1);
    for (size_t i = 0; i < 8; ++i) source.bytes()[i * page] = 21 + i;

    alias.addr = mremap(source.addr, 0, alias.size, MREMAP_MAYMOVE);
    ASSERT_NE(alias.addr, MAP_FAILED) << strerror(errno);
    // The alias has not faulted yet: residency must come from the backing.
    ASSERT_EQ(mincore(alias.addr, alias.size, resident), 0);
    for (auto value : resident) EXPECT_EQ(value & 1, 1);
    ASSERT_EQ(mprotect(source.addr, source.size, PROT_NONE), 0);
    for (size_t i = 0; i < 8; ++i) {
        EXPECT_EQ(alias.bytes()[i * page], 21 + i);
        alias.bytes()[i * page + 1] = 81 + i;
    }
    ASSERT_EQ(mprotect(source.addr, source.size, PROT_READ | PROT_WRITE), 0);
    for (size_t i = 0; i < 8; ++i) {
        EXPECT_EQ(source.bytes()[i * page], 21 + i);
        EXPECT_EQ(source.bytes()[i * page + 1], 81 + i);
    }
}

TEST_P(InternalShmem, ConcurrentFirstFaultsPreserveBothWriters) {
    const size_t page = PageSize();
    constexpr size_t rounds = 64;
    Mapping mapping(rounds * page);
    Map(mapping); ASSERT_NE(mapping.addr, MAP_FAILED);
    unsigned char resident[rounds] = {};
    ASSERT_EQ(mincore(mapping.addr, mapping.size, resident), 0);
    for (auto value : resident) ASSERT_EQ(value & 1, 0);
    int start[2], done[2];
    ASSERT_EQ(pipe(start), 0);
    if (pipe(done) != 0) {
        close(start[0]); close(start[1]);
        FAIL() << "pipe: " << strerror(errno);
    }
    const pid_t child = fork();
    if (child < 0) {
        close(start[0]); close(start[1]); close(done[0]); close(done[1]);
        FAIL() << "fork: " << strerror(errno);
    }
    if (child == 0) {
        close(start[1]); close(done[0]);
        char token = 0;
        for (size_t i = 0; i < rounds; ++i) {
            if (write(done[1], &token, 1) != 1 || read(start[0], &token, 1) != 1) _exit(1);
            mapping.bytes()[i * page + 1] = 101 + i;
            if (write(done[1], &token, 1) != 1) _exit(2);
        }
        _exit(0);
    }
    close(start[0]); close(done[1]);
    bool synchronized = true;
    char token = 0;
    for (size_t i = 0; i < rounds; ++i) {
        // Both processes are ready before either touches this cold page.
        if (read(done[0], &token, 1) != 1 || write(start[1], &token, 1) != 1) {
            synchronized = false;
            break;
        }
        mapping.bytes()[i * page] = 31 + i;
        if (read(done[0], &token, 1) != 1) {
            synchronized = false;
            break;
        }
        EXPECT_EQ(mapping.bytes()[i * page], 31 + i);
        EXPECT_EQ(mapping.bytes()[i * page + 1], 101 + i);
    }
    close(start[1]); close(done[0]);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    EXPECT_TRUE(synchronized);
    ASSERT_TRUE(WIFEXITED(status)); EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST_P(InternalShmem, SplitAndDuplicateKeepBackingOffsets) {
    const size_t page = PageSize();
    Mapping mapping(3 * page);
    Map(mapping); ASSERT_NE(mapping.addr, MAP_FAILED);
    mapping.bytes()[2 * page] = 42;
    MapInfo before, after;
    ASSERT_TRUE(ReadMap(mapping.addr, &before));
    ASSERT_EQ(mprotect(static_cast<char*>(mapping.addr) + page, page, PROT_READ), 0);
    ASSERT_TRUE(ReadMap(static_cast<char*>(mapping.addr) + 2 * page, &after));
    EXPECT_EQ(before.inode, after.inode); EXPECT_EQ(after.offset, 2 * page);
    Mapping alias(page);
    alias.addr = mremap(static_cast<char*>(mapping.addr) + 2 * page, 0, page, MREMAP_MAYMOVE);
    ASSERT_NE(alias.addr, MAP_FAILED) << strerror(errno);
    EXPECT_EQ(alias.bytes()[0], 42);
    alias.bytes()[0] = 87;
    EXPECT_EQ(mapping.bytes()[2 * page], 87);
    ASSERT_TRUE(ReadMap(alias.addr, &after));
    EXPECT_EQ(before.inode, after.inode); EXPECT_EQ(after.offset, 2 * page);
    ASSERT_EQ(munmap(mapping.addr, mapping.size), 0); mapping.addr = MAP_FAILED;
    EXPECT_EQ(alias.bytes()[0], 87);
}

TEST_P(InternalShmem, ShrinkThenMovePreservesBackingSizeAndContents) {
    const size_t page = PageSize();
    Mapping reservation(4 * page);
    reservation.addr = mmap(nullptr, reservation.size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(reservation.addr, MAP_FAILED);
    Mapping original(2 * page);
    Map(original, reservation.addr); ASSERT_NE(original.addr, MAP_FAILED);
    original.bytes()[0] = 11; original.bytes()[page] = 22;
    MapInfo initial, moved;
    ASSERT_TRUE(ReadMap(original.addr, &initial));
    ASSERT_EQ(mremap(original.addr, 2 * page, page, 0), original.addr);
    void* destination = static_cast<char*>(reservation.addr) + page;
    ASSERT_EQ(mremap(original.addr, page, 3 * page, MREMAP_MAYMOVE | MREMAP_FIXED,
                     destination), destination) << strerror(errno);
    original.addr = MAP_FAILED;  // The reservation owns the entire final range.
    auto* bytes = static_cast<volatile unsigned char*>(destination);
    EXPECT_EQ(bytes[0], 11); EXPECT_EQ(bytes[page], 22);
    ASSERT_TRUE(ReadMap(destination, &moved));
    EXPECT_EQ(initial.inode, moved.inode);
    const int status = ReadInChild(bytes + 2 * page);
    ASSERT_GE(status, 0); ASSERT_TRUE(WIFSIGNALED(status)); EXPECT_EQ(WTERMSIG(status), SIGBUS);
}

TEST_P(InternalShmem, FutexWakeUsesBackingIdentityAcrossAliases) {
    Mapping source(PageSize()), independent(PageSize()), alias(PageSize());
    Map(source); Map(independent);
    ASSERT_NE(source.addr, MAP_FAILED); ASSERT_NE(independent.addr, MAP_FAILED);
    alias.addr = mremap(source.addr, 0, alias.size, MREMAP_MAYMOVE);
    ASSERT_NE(alias.addr, MAP_FAILED);
    auto* word = static_cast<int*>(source.addr);
    *word = 0;
    std::atomic<bool> started{false};
    long waited = -2;
    int wait_errno = 0;
    std::thread waiter([&] {
        timespec timeout{2, 0};
        started.store(true, std::memory_order_release);
        waited = syscall(SYS_futex, word, FUTEX_WAIT, 0, &timeout, nullptr, 0);
        wait_errno = errno;
    });
    long woke = 0;
    long wrong_wakes = 0;
    for (unsigned i = 0; i < 1000 && woke == 0; ++i) {
        if (started.load(std::memory_order_acquire)) {
            const long wrong = syscall(SYS_futex, independent.addr, FUTEX_WAKE, 1, nullptr, nullptr, 0);
            if (wrong != 0) wrong_wakes = wrong;
            woke = syscall(SYS_futex, alias.addr, FUTEX_WAKE, 1, nullptr, nullptr, 0);
        }
        if (woke == 0) usleep(1000);
    }
    waiter.join();
    EXPECT_EQ(wrong_wakes, 0);
    EXPECT_EQ(woke, 1); EXPECT_EQ(waited, 0) << strerror(wait_errno);
}

// /proc/meminfo uses batched global counters on Linux. Use a sizeable mapping
// and tolerate one MiB of batching/background activity, rather than assert an
// unstable exact single-page delta. Dedicated guest runs provide the accounting
// evidence; the content assertion independently checks ownership after unmap.
long long ShmemBytes() {
    FILE* file = fopen("/proc/meminfo", "r");
    if (!file) return -1;
    char line[256];
    long long kb = -1;
    while (fgets(line, sizeof(line), file)) {
        if (sscanf(line, "Shmem: %lld kB", &kb) == 1) break;
    }
    fclose(file);
    return kb < 0 ? -1 : kb * 1024;
}

TEST_P(InternalShmem, LastAliasRetainsPagesThenReleasesShmemAccounting) {
    const size_t length = 8 * 1024 * 1024;
    const long long tolerance = 1024 * 1024;
    const long long before = ShmemBytes();
    ASSERT_GE(before, 0);
    Mapping source(length), alias(length);
    Map(source); ASSERT_NE(source.addr, MAP_FAILED);
    for (size_t i = 0; i < length; i += PageSize()) source.bytes()[i] = 53;
    const long long allocated = ShmemBytes();
    EXPECT_GE(allocated, before + static_cast<long long>(length) - tolerance);
    alias.addr = mremap(source.addr, 0, length, MREMAP_MAYMOVE);
    ASSERT_NE(alias.addr, MAP_FAILED);
    ASSERT_EQ(munmap(source.addr, source.size), 0); source.addr = MAP_FAILED;
    for (size_t i = 0; i < length; i += PageSize()) ASSERT_EQ(alias.bytes()[i], 53);
    EXPECT_GE(ShmemBytes(), allocated - tolerance);
    ASSERT_EQ(munmap(alias.addr, alias.size), 0); alias.addr = MAP_FAILED;
    EXPECT_LE(ShmemBytes(), before + tolerance);
}

INSTANTIATE_TEST_SUITE_P(AnonymousAndZero, InternalShmem, testing::Bool(),
                        [](const testing::TestParamInfo<bool>& info) {
                            return info.param ? "DevZero" : "Anonymous";
                        });

TEST(InternalShmemSysV, PrivateSegmentsHaveKeyNamesAndDistinctInodes) {
    const int first = shmget(IPC_PRIVATE, PageSize(), 0600);
    ASSERT_GE(first, 0) << strerror(errno);
    const int second = shmget(IPC_PRIVATE, PageSize(), 0600);
    if (second < 0) {
        const int error = errno;
        shmctl(first, IPC_RMID, nullptr);
        FAIL() << strerror(error);
        return;
    }
    void* a = shmat(first, nullptr, 0);
    void* b = shmat(second, nullptr, 0);
    // Mark both before assertions so a failure cannot leak persistent IPC ids.
    EXPECT_EQ(shmctl(first, IPC_RMID, nullptr), 0);
    EXPECT_EQ(shmctl(second, IPC_RMID, nullptr), 0);
    EXPECT_NE(a, MAP_FAILED); EXPECT_NE(b, MAP_FAILED);
    if (a != MAP_FAILED && b != MAP_FAILED) {
        MapInfo left, right;
        const bool read_left = ReadMap(a, &left), read_right = ReadMap(b, &right);
        EXPECT_TRUE(read_left); EXPECT_TRUE(read_right);
        if (read_left && read_right) {
            // Linux may expose shmid 0 as the first SysV inode number.
            EXPECT_NE(left.inode, right.inode);
            EXPECT_NE(left.line.find("/SYSV00000000 (deleted)"), std::string::npos) << left.line;
            EXPECT_NE(right.line.find("/SYSV00000000 (deleted)"), std::string::npos) << right.line;
        }
    }
    if (a != MAP_FAILED) { EXPECT_EQ(shmdt(a), 0); }
    if (b != MAP_FAILED) { EXPECT_EQ(shmdt(b), 0); }
}

TEST(InternalShmemZero, SharedOffsetUsesFixedBackingPrivateAndReadonlyDoNot) {
    const size_t page = PageSize();
    int fd = open("/dev/zero", O_RDWR); ASSERT_GE(fd, 0);
    Mapping shared(2 * page), private_map(2 * page);
    shared.addr = mmap(nullptr, shared.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, page);
    private_map.addr = mmap(nullptr, private_map.size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, page);
    close(fd);
    ASSERT_NE(shared.addr, MAP_FAILED); ASSERT_NE(private_map.addr, MAP_FAILED);
    EXPECT_EQ(shared.bytes()[0], 0);
    const int status = ReadInChild(shared.bytes() + page);
    ASSERT_GE(status, 0); ASSERT_TRUE(WIFSIGNALED(status)); EXPECT_EQ(WTERMSIG(status), SIGBUS);
    EXPECT_EQ(private_map.bytes()[page], 0); private_map.bytes()[page] = 9;
    fd = open("/dev/zero", O_RDONLY); ASSERT_GE(fd, 0);
    Mapping readonly(page);
    readonly.addr = mmap(nullptr, page, PROT_READ, MAP_SHARED, fd, page);
    close(fd); ASSERT_NE(readonly.addr, MAP_FAILED);
    const int read_status = ReadInChild(readonly.bytes());
    ASSERT_GE(read_status, 0); ASSERT_TRUE(WIFEXITED(read_status)); EXPECT_EQ(WEXITSTATUS(read_status), 0);
    MapInfo info; ASSERT_TRUE(ReadMap(readonly.addr, &info));
    EXPECT_NE(info.line.find("r--s"), std::string::npos) << info.line;
    EXPECT_NE(info.line.find("/dev/zero"), std::string::npos) << info.line;
    EXPECT_EQ(info.line.find("(deleted)"), std::string::npos) << info.line;
    errno = 0; EXPECT_EQ(mprotect(readonly.addr, page, PROT_READ | PROT_WRITE), -1);
    EXPECT_EQ(errno, EACCES);
}

TEST(InternalShmemZero, ReadonlyZeroSharedFutexReturnsEfault) {
    const int fd = open("/dev/zero", O_RDONLY);
    ASSERT_GE(fd, 0);
    for (int flags : {MAP_SHARED, MAP_PRIVATE}) {
        SCOPED_TRACE(flags);
        Mapping mapping(PageSize());
        mapping.addr = mmap(nullptr, mapping.size, PROT_READ, flags, fd, 0);
        if (mapping.addr == MAP_FAILED) {
            close(fd);
            FAIL() << strerror(errno);
        }
        EXPECT_EQ(mapping.bytes()[0], 0);
        const timespec timeout = {0, 0};
        errno = 0;
        EXPECT_EQ(syscall(SYS_futex, mapping.addr, FUTEX_WAIT, 0,
                          &timeout, nullptr, 0), -1);
        EXPECT_EQ(errno, EFAULT);
        errno = 0;
        EXPECT_EQ(syscall(SYS_futex, mapping.addr, FUTEX_WAKE, 1,
                          nullptr, nullptr, 0), -1);
        EXPECT_EQ(errno, EFAULT);
        // FUTEX_PRIVATE uses the address-space key without anonymous-page
        // write admission; a readable word can still time out normally.
        errno = 0;
        EXPECT_EQ(syscall(SYS_futex, mapping.addr, FUTEX_WAIT_PRIVATE, 0,
                          &timeout, nullptr, 0), -1);
        EXPECT_EQ(errno, ETIMEDOUT);
        EXPECT_EQ(syscall(SYS_futex, mapping.addr, FUTEX_WAKE_PRIVATE, 1,
                          nullptr, nullptr, 0), 0);
    }
    close(fd);
}

TEST(InternalShmemZero, WriteOnlyFdCannotMapEvenWithProtNone) {
    int fd = open("/dev/zero", O_WRONLY); ASSERT_GE(fd, 0);
    for (int flags : {MAP_SHARED, MAP_PRIVATE}) {
        Mapping mapping(PageSize());
        errno = 0; mapping.addr = mmap(nullptr, mapping.size, PROT_NONE, flags, fd, 0);
        const int error = errno;
        EXPECT_EQ(mapping.addr, MAP_FAILED); EXPECT_EQ(error, EACCES);
    }
    close(fd);
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
