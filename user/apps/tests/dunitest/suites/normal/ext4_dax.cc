#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/ptrace.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {
constexpr size_t kPage = 4096;
constexpr char kMarker[] = "DragonOS-ext4-dax-fixture-v1\n";

unsigned char Pattern(size_t offset) { return (offset * 17 + 3) % 251; }

class Mapping {
 public:
    Mapping(int fd, size_t len, int prot = PROT_READ, int flags = MAP_PRIVATE,
            off_t offset = 0)
        : data(mmap(nullptr, len, prot, flags, fd, offset)), size(len) {}
    ~Mapping() { if (data != MAP_FAILED) munmap(data, size); }
    Mapping(const Mapping&) = delete;
    Mapping& operator=(const Mapping&) = delete;
    unsigned char* bytes() { return static_cast<unsigned char*>(data); }
    void* data;
    size_t size;
};

class Child {
 public:
    explicit Child(pid_t value) : pid(value) {}
    ~Child() {
        if (pid > 0) { kill(pid, SIGKILL); while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) {} }
    }
    pid_t pid;
};

class Ext4Dax : public ::testing::Test {
 protected:
    std::string device;
    std::string root;
    bool mounted = false;

    void SetUp() override {
        const char* configured = getenv("EXT4_DAX_DEVICE");
        if (!configured || !*configured) {
            GTEST_SKIP() << "attach the disposable ext4-dax-fixture image and set EXT4_DAX_DEVICE";
        }
        device = configured;
        ASSERT_TRUE(device == "/dev/pmem0" || device == "/dev/pmem0p1")
            << "suite only admits the explicitly configured pmem0 fixture or its first partition";
        ASSERT_EQ(sysconf(_SC_PAGESIZE), static_cast<long>(kPage));
        char temporary[] = "/tmp/ext4-dax-XXXXXX";
        char* directory = mkdtemp(temporary);
        ASSERT_NE(directory, nullptr) << strerror(errno);
        root = directory;
        Mount("dax=never");
        if (HasFatalFailure()) return;
        int fd = Open("fixture-marker");
        ASSERT_GE(fd, 0) << "not the dedicated DAX fixture";
        char marker[sizeof(kMarker)] = {};
        ssize_t count = read(fd, marker, sizeof(marker));
        close(fd);
        ASSERT_EQ(count, static_cast<ssize_t>(sizeof(kMarker) - 1));
        ASSERT_EQ(memcmp(marker, kMarker, sizeof(kMarker) - 1), 0)
            << "fixture marker mismatch; refusing this filesystem";
        Remount("dax=always");
    }

    void TearDown() override {
        if (mounted) {
            EXPECT_EQ(umount2(root.c_str(), MNT_DETACH), 0) << strerror(errno);
        }
        if (!root.empty()) rmdir(root.c_str());
    }

    void Mount(const char* options) {
        ASSERT_EQ(mount(device.c_str(), root.c_str(), "ext4", MS_RDONLY, options), 0)
            << strerror(errno);
        mounted = true;
    }
    void Remount(const char* options) {
        ASSERT_TRUE(mounted);
        ASSERT_EQ(umount(root.c_str()), 0) << strerror(errno);
        mounted = false;
        Mount(options);
    }
    int Open(const char* name, int flags = O_RDONLY) {
        return open((root + "/" + name).c_str(), flags | O_CLOEXEC);
    }
    bool DaxAttribute(int fd) {
        alignas(8) unsigned char value[256] = {};
        long result = syscall(SYS_statx, fd, "", AT_EMPTY_PATH, 0x7ff, value);
        EXPECT_EQ(result, 0) << strerror(errno);
        uint64_t attributes = 0, mask = 0;
        memcpy(&attributes, value + 8, sizeof(attributes));
        memcpy(&mask, value + 56, sizeof(mask));
        EXPECT_NE(mask & 0x200000, 0u) << "effective DAX attribute must have a supported mask";
        return (attributes & 0x200000) != 0;
    }
    void ExpectOriginal() {
        int fd = Open("full-pattern");
        ASSERT_GE(fd, 0);
        unsigned char bytes[64];
        ASSERT_EQ(pread(fd, bytes, sizeof(bytes), 0), static_cast<ssize_t>(sizeof(bytes)));
        close(fd);
        for (size_t i = 0; i < sizeof(bytes); ++i) EXPECT_EQ(bytes[i], Pattern(i));
    }
};

// This is the minimal pre-fix baseline: old kernels parse dax but fail this
// effective attribute assertion, rather than falsely passing buffered mmap.
TEST_F(Ext4Dax, EffectivePolicyAndInodeFlag) {
    for (const char* mode : {"dax=always", "dax=never", "dax=inode", ""}) {
        Remount(mode);
        ASSERT_FALSE(HasFatalFailure());
        int plain = Open("full-pattern"), flagged = Open("inode-dax");
        ASSERT_GE(plain, 0);
        ASSERT_GE(flagged, 0);
        bool always = strcmp(mode, "dax=always") == 0;
        bool never = strcmp(mode, "dax=never") == 0;
        EXPECT_EQ(DaxAttribute(plain), always) << mode;
        EXPECT_EQ(DaxAttribute(flagged), !never) << mode;
        close(plain);
        close(flagged);
    }
}

TEST_F(Ext4Dax, PartitionGeometryControlsDaxEligibility) {
    // This describes the attached disposable fixture, not a kernel setting.
    // The ordinary page-aligned image remains the default positive case.
    const char* configured = getenv("EXT4_DAX_PAGE_ALIGNED");
    ASSERT_TRUE(configured == nullptr || strcmp(configured, "0") == 0 ||
                strcmp(configured, "1") == 0);
    const bool aligned = configured == nullptr || strcmp(configured, "1") == 0;
    for (const char* mode : {"dax=always", "dax=never", "dax=inode", ""}) {
        Remount(mode);
        ASSERT_FALSE(HasFatalFailure());
        for (const char* name : {"full-pattern", "inode-dax"}) {
            int fd = Open(name);
            ASSERT_GE(fd, 0) << strerror(errno);
            const bool expected_dax = aligned && strcmp(mode, "dax=never") != 0 &&
                (strcmp(mode, "dax=always") == 0 || strcmp(name, "inode-dax") == 0);
            EXPECT_EQ(DaxAttribute(fd), expected_dax) << mode << " " << name;
            unsigned char bytes[37];
            ASSERT_EQ(pread(fd, bytes, sizeof(bytes), kPage - 7),
                      static_cast<ssize_t>(sizeof(bytes)));
            for (size_t i = 0; i < sizeof(bytes); ++i)
                EXPECT_EQ(bytes[i], Pattern(kPage - 7 + i));

            // Contain a pre-fix SIGBUS so both the attribute and the mapping
            // failure are reported without terminating the whole suite.
            pid_t pid = fork();
            ASSERT_GE(pid, 0);
            if (pid == 0) {
                Mapping map(fd, 3 * kPage, PROT_READ | PROT_WRITE);
                if (map.data == MAP_FAILED) _exit(73);
                for (size_t i = 0; i < 3 * kPage; ++i)
                    if (map.bytes()[i] != Pattern(i)) _exit(74);
                map.bytes()[kPage] ^= 0xff;
                if (map.bytes()[kPage] != static_cast<unsigned char>(Pattern(kPage) ^ 0xff))
                    _exit(75);
                _exit(0);
            }
            Child child(pid);
            int status = 0;
            ASSERT_EQ(waitpid(pid, &status, 0), pid);
            child.pid = -1;
            EXPECT_TRUE(WIFEXITED(status)) << mode << " " << name << " status=" << status;
            if (WIFEXITED(status)) {
                EXPECT_EQ(WEXITSTATUS(status), 0);
            }
            unsigned char original = 0;
            EXPECT_EQ(pread(fd, &original, 1, kPage), 1);
            EXPECT_EQ(original, Pattern(kPage));
            close(fd);
        }
    }
}

TEST_F(Ext4Dax, ByteReadsAndDirectIntent) {
    for (int flags : {O_RDONLY, O_RDONLY | O_DIRECT}) {
        int fd = Open("full-pattern", flags);
        ASSERT_GE(fd, 0) << strerror(errno);
        std::vector<unsigned char> data(kPage + 19, 0);
        for (off_t offset : {off_t(1), off_t(4095), off_t(8191)}) {
            size_t length = 37;
            ASSERT_EQ(pread(fd, data.data() + 1, length, offset), static_cast<ssize_t>(length))
                << flags << " " << offset << " " << strerror(errno);
            for (size_t i = 0; i < length; ++i) EXPECT_EQ(data[i + 1], Pattern(offset + i));
        }
        EXPECT_EQ(pread(fd, data.data() + 1, 37, 3 * kPage - 3), 3);
        EXPECT_EQ(pread(fd, data.data() + 1, 37, 3 * kPage), 0);
        close(fd);
    }
}

TEST_F(Ext4Dax, HolesUnwrittenAndPoisonedTail) {
    int tail = Open("tail"), sparse = Open("sparse"), unwritten = Open("unwritten");
    ASSERT_GE(tail, 0); ASSERT_GE(sparse, 0); ASSERT_GE(unwritten, 0);
    Mapping t(tail, 3 * kPage), s(sparse, 3 * kPage), u(unwritten, 3 * kPage);
    close(tail); close(sparse); close(unwritten);
    ASSERT_NE(t.data, MAP_FAILED); ASSERT_NE(s.data, MAP_FAILED); ASSERT_NE(u.data, MAP_FAILED);
    for (size_t i = 0; i < kPage + 17; ++i) EXPECT_EQ(t.bytes()[i], Pattern(i));
    for (size_t i = kPage + 17; i < 2 * kPage; ++i) EXPECT_EQ(t.bytes()[i], 0);
    for (size_t i = kPage; i < 2 * kPage; ++i) EXPECT_EQ(s.bytes()[i], 0);
    EXPECT_EQ(s.bytes()[0], Pattern(0));
    EXPECT_EQ(s.bytes()[2 * kPage], Pattern(2 * kPage));
    for (size_t i = 0; i < 3 * kPage; ++i) EXPECT_EQ(u.bytes()[i], 0);
    pid_t pid = fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) { volatile unsigned char byte = t.bytes()[2 * kPage]; (void)byte; _exit(70); }
    Child child(pid);
    int status = 0;
    ASSERT_EQ(waitpid(pid, &status, 0), pid);
    child.pid = -1;
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(WTERMSIG(status), SIGBUS);
}

TEST_F(Ext4Dax, PrivateCowForkProtectionAndRemap) {
    int fd = Open("full-pattern"); ASSERT_GE(fd, 0);
    Mapping map(fd, 3 * kPage); close(fd);
    ASSERT_NE(map.data, MAP_FAILED);
    EXPECT_EQ(map.bytes()[0], Pattern(0));
    ASSERT_EQ(mprotect(map.data, map.size, PROT_READ | PROT_WRITE), 0);
    map.bytes()[0] = 0x7b;
    pid_t pid = fork(); ASSERT_GE(pid, 0);
    if (pid == 0) {
        if (map.bytes()[0] != 0x7b || map.bytes()[kPage] != Pattern(kPage)) _exit(71);
        map.bytes()[0] = 0x6c;
        map.bytes()[kPage] = 0x5d;
        _exit(map.bytes()[0] == 0x6c && map.bytes()[kPage] == 0x5d ? 0 : 72);
    }
    Child child(pid); int status = 0;
    ASSERT_EQ(waitpid(pid, &status, 0), pid); child.pid = -1;
    ASSERT_TRUE(WIFEXITED(status)); EXPECT_EQ(WEXITSTATUS(status), 0);
    EXPECT_EQ(map.bytes()[0], 0x7b); EXPECT_EQ(map.bytes()[kPage], Pattern(kPage));
    void* moved = mremap(map.data, map.size, 4 * kPage, MREMAP_MAYMOVE);
    ASSERT_NE(moved, MAP_FAILED) << strerror(errno);
    map.data = moved; map.size = 4 * kPage;
    EXPECT_EQ(map.bytes()[0], 0x7b); EXPECT_EQ(map.bytes()[2 * kPage], Pattern(2 * kPage));
    ASSERT_EQ(mprotect(map.data, 3 * kPage, PROT_READ), 0);
    ExpectOriginal();
}

TEST_F(Ext4Dax, SharedWriteIsDenied) {
    int fd = Open("full-pattern"); ASSERT_GE(fd, 0);
    void* writable = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    EXPECT_EQ(writable, MAP_FAILED);
    if (writable != MAP_FAILED) munmap(writable, kPage);
    Mapping read(fd, kPage, PROT_READ, MAP_SHARED); close(fd);
    ASSERT_NE(read.data, MAP_FAILED);
    EXPECT_EQ(mprotect(read.data, kPage, PROT_READ | PROT_WRITE), -1);
    EXPECT_EQ(read.bytes()[0], Pattern(0));
}

TEST_F(Ext4Dax, DestinationFaultsIntoSameInode) {
    int fd = Open("full-pattern"); ASSERT_GE(fd, 0);
    Mapping destination(fd, 2 * kPage, PROT_READ | PROT_WRITE);
    ASSERT_NE(destination.data, MAP_FAILED);
    // Destination starts cold, and crosses a page into this same inode.
    ASSERT_EQ(pread(fd, destination.bytes() + kPage - 7, 31, 17), 31) << strerror(errno);
    for (size_t i = 0; i < 31; ++i) EXPECT_EQ(destination.bytes()[kPage - 7 + i], Pattern(17 + i));
    close(fd);
    ExpectOriginal();
}

TEST_F(Ext4Dax, RemoteRxUnfaultedExternalAndForkedCow) {
    int fd = Open("full-pattern"); ASSERT_GE(fd, 0);
    for (int state = 0; state < 3; ++state) {
        Mapping map(fd, kPage, PROT_READ | PROT_EXEC);
        ASSERT_NE(map.data, MAP_FAILED);
        if (state >= 1) {
            EXPECT_EQ(map.bytes()[0], Pattern(0));
        }
        if (state == 2) {
            int memory = open("/proc/self/mem", O_RDWR | O_CLOEXEC); ASSERT_GE(memory, 0);
            unsigned char value = 0x55;
            ssize_t result = pwrite(memory, &value, 1, reinterpret_cast<uintptr_t>(map.data));
            close(memory); ASSERT_EQ(result, 1);
        }
        pid_t pid = fork(); ASSERT_GE(pid, 0);
        if (pid == 0) {
            if (ptrace(PTRACE_TRACEME, 0, nullptr, nullptr) != 0) _exit(73);
            raise(SIGSTOP);
            _exit(map.bytes()[0] == 0x63 && map.bytes()[1] == 0x64 ? 0 : 74);
        }
        Child child(pid); int status = 0;
        ASSERT_EQ(waitpid(pid, &status, 0), pid);
        if (WIFEXITED(status) || WIFSIGNALED(status)) child.pid = -1;
        ASSERT_TRUE(WIFSTOPPED(status));
        unsigned char value = 0x63;
        struct iovec local = {&value, 1}, remote = {map.data, 1};
        EXPECT_EQ(syscall(SYS_process_vm_writev, pid, &local, 1, &remote, 1, 0), -1);
        if (state == 1) {
            // Read a genuine external PTE before any forced write creates COW.
            unsigned char original = 0;
            struct iovec read_local = {&original, 1};
            ASSERT_EQ(syscall(SYS_process_vm_readv, pid, &read_local, 1, &remote, 1, 0), 1);
            EXPECT_EQ(original, Pattern(0));
            errno = 0;
            long peeked = ptrace(PTRACE_PEEKDATA, pid, map.data, nullptr);
            ASSERT_EQ(errno, 0) << strerror(errno);
            EXPECT_EQ(static_cast<unsigned char>(peeked), Pattern(0));
        }
        // Do not PEEK first in state 0: POKE must itself fault the cold RX page.
        // A complete word avoids depending on a prior remote read.
        unsigned long word = 0;
        for (size_t i = 0; i < sizeof(word); ++i)
            word |= static_cast<unsigned long>(i == 0 ? value : Pattern(i)) << (8 * i);
        ASSERT_EQ(ptrace(PTRACE_POKEDATA, pid, map.data, reinterpret_cast<void*>(word)), 0);
        unsigned char observed = 0;
        local = {&observed, 1};
        ASSERT_EQ(syscall(SYS_process_vm_readv, pid, &local, 1, &remote, 1, 0), 1);
        EXPECT_EQ(observed, value);
        std::string memory_path = "/proc/" + std::to_string(pid) + "/mem";
        int memory = open(memory_path.c_str(), O_RDWR | O_CLOEXEC); ASSERT_GE(memory, 0);
        value = 0x64;
        ssize_t result = pwrite(memory, &value, 1, reinterpret_cast<uintptr_t>(map.data) + 1);
        close(memory); ASSERT_EQ(result, 1);
        local = {&value, 1};
        EXPECT_EQ(syscall(SYS_process_vm_writev, pid, &local, 1, &remote, 1, 0), -1);
        ASSERT_EQ(ptrace(PTRACE_CONT, pid, nullptr, nullptr), 0);
        ASSERT_EQ(waitpid(pid, &status, 0), pid); child.pid = -1;
        ASSERT_TRUE(WIFEXITED(status)); EXPECT_EQ(WEXITSTATUS(status), 0);
        EXPECT_EQ(map.bytes()[0], state == 2 ? 0x55 : Pattern(0));
        ExpectOriginal();
    }
    // Check the hardware permission itself, not only process_vm's VMA gate.
    // Map and force COW inside this child: a subsequent fork would write-protect
    // the page again and could conceal an incorrectly writable COW PTE.
    pid_t pid = fork(); ASSERT_GE(pid, 0);
    if (pid == 0) {
        void* mapping = mmap(nullptr, kPage, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        if (mapping == MAP_FAILED) _exit(75);
        int memory = open("/proc/self/mem", O_RDWR | O_CLOEXEC);
        if (memory < 0) _exit(76);
        unsigned char value = 0x65;
        ssize_t count = pwrite(memory, &value, 1, reinterpret_cast<uintptr_t>(mapping));
        close(memory);
        if (count != 1 || *static_cast<volatile unsigned char*>(mapping) != value) _exit(77);
        *static_cast<volatile unsigned char*>(mapping) = 0x66;
        _exit(78);
    }
    Child child(pid); int status = 0;
    ASSERT_EQ(waitpid(pid, &status, 0), pid); child.pid = -1;
    ASSERT_TRUE(WIFSIGNALED(status)); EXPECT_EQ(WTERMSIG(status), SIGSEGV);
    close(fd);
    ExpectOriginal();
}

TEST_F(Ext4Dax, ClosedFdAndLazyUnmountKeepMappingAlive) {
    int fd = Open("full-pattern"); ASSERT_GE(fd, 0);
    Mapping map(fd, 3 * kPage, PROT_READ | PROT_WRITE); close(fd);
    ASSERT_NE(map.data, MAP_FAILED);
    ASSERT_EQ(umount2(root.c_str(), MNT_DETACH), 0) << strerror(errno);
    mounted = false;
    EXPECT_EQ(map.bytes()[0], Pattern(0));
    EXPECT_EQ(map.bytes()[2 * kPage], Pattern(2 * kPage));
    map.bytes()[kPage] = 0x76;
    EXPECT_EQ(map.bytes()[kPage], 0x76);
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
