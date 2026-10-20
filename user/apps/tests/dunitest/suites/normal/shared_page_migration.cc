#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <string>

namespace {
constexpr const char* kCommand = "/sys/kernel/debug/mm/page_migration";
constexpr size_t kPage = 4096;
constexpr uint64_t kInitial = 0x123456789abcdefULL;
constexpr uint64_t kChildWrite = 0xfedcba987654321ULL;
constexpr uint32_t kGeneral = 0x100;

class Fd {
 public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
 private:
    int value_;
};

class Mapping {
 public:
    explicit Mapping(void* address = MAP_FAILED, size_t length = kPage) : address_(address), length_(length) {}
    ~Mapping() { if (address_ != MAP_FAILED) munmap(address_, length_); }
    Mapping(const Mapping&) = delete;
    Mapping& operator=(const Mapping&) = delete;
    void* get() const { return address_; }
    void* release() { void* value = address_; address_ = MAP_FAILED; return value; }
    volatile uint64_t& word() const { return *static_cast<volatile uint64_t*>(address_); }
 private:
    void* address_;
    size_t length_;
};

std::string ReadReport(int fd) {
    std::string report;
    char buffer[191];
    off_t offset = 0;
    for (;;) {
        const ssize_t count = pread(fd, buffer, sizeof(buffer), offset);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        report.append(buffer, static_cast<size_t>(count));
        offset += count;
    }
    return report;
}

struct Migration {
    bool ok = false;
    uint64_t old_phys = 0;
    uint64_t new_phys = 0;
    std::string report;
};

uint64_t PhysicalField(const std::string& report, const char* name) {
    const std::string key = std::string(name) + "=";
    const size_t position = report.find(key);
    if (position == std::string::npos) return 0;
    char* end = nullptr;
    const char* start = report.c_str() + position + key.size();
    const uint64_t result = strtoull(start, &end, 0);
    return end != start && *end == '\n' ? result : 0;
}

Migration Migrate(void* address, uint32_t stage = 0) {
    struct Command { uint64_t address; uint32_t mode; uint32_t reserved; };
    const Command command{reinterpret_cast<uint64_t>(address), kGeneral | stage, 0};
    static_assert(sizeof(command) == 16, "migration command ABI");
    Migration result;
    for (int attempt = 0; attempt < 128; ++attempt) {
        Fd fd(open(kCommand, O_RDWR | O_CLOEXEC));
        if (fd.get() < 0) {
            result.report = "open_error=" + std::to_string(errno);
            return result;
        }
        const ssize_t written = write(fd.get(), &command, sizeof(command));
        result.report = written == sizeof(command) ? ReadReport(fd.get()) :
            "write_error=" + std::to_string(errno);
        if (result.report.find("error=busy\n") == std::string::npos) break;
        sched_yield();
    }
    result.old_phys = PhysicalField(result.report, "old_phys");
    result.new_phys = PhysicalField(result.report, "new_phys");
    result.ok = result.report.find("status=ok\n") != std::string::npos &&
        result.old_phys != 0 && result.new_phys != 0 && result.old_phys != result.new_phys;
    return result;
}

bool Transfer(int fd, void* bytes, size_t size, bool write_bytes) {
    auto* cursor = static_cast<char*>(bytes);
    while (size != 0) {
        const ssize_t count = write_bytes ? write(fd, cursor, size) : read(fd, cursor, size);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return false;
        cursor += count;
        size -= static_cast<size_t>(count);
    }
    return true;
}

// A live peer retains its real mapping while the parent migrates it. Its own
// migration proves that both MMs observe the same new PFN, not merely bytes.
class ForkPeer {
 public:
    struct Reply { uint64_t ok; uint64_t old_phys; uint64_t new_phys; uint64_t value; };
    explicit ForkPeer(void* address) {
        int commands[2], replies[2];
        if (pipe(commands) != 0) return;
        if (pipe(replies) != 0) { close(commands[0]); close(commands[1]); return; }
        child_ = fork();
        if (child_ == 0) {
            close(commands[1]); close(replies[0]);
            char command;
            while (Transfer(commands[0], &command, 1, false) && command != 'q') {
                Reply reply{};
                if (command == 'm') {
                    const Migration migrated = Migrate(address);
                    reply.ok = migrated.ok;
                    reply.old_phys = migrated.old_phys;
                    reply.new_phys = migrated.new_phys;
                } else if (command == 'r' || command == 'w') {
                    if (command == 'w') *static_cast<volatile uint64_t*>(address) = kChildWrite;
                    reply.ok = 1;
                }
                reply.value = *static_cast<volatile uint64_t*>(address);
                if (!Transfer(replies[1], &reply, sizeof(reply), true)) _exit(2);
            }
            close(commands[0]); close(replies[1]);
            _exit(0);
        }
        close(commands[0]); close(replies[1]);
        if (child_ < 0) { close(commands[1]); close(replies[0]); return; }
        commands_ = commands[1];
        replies_ = replies[0];
    }
    ~ForkPeer() {
        if (child_ <= 0) return;
        char quit = 'q';
        Transfer(commands_, &quit, 1, true);
        close(commands_); close(replies_);
        int status = 0;
        pid_t waited;
        do { waited = waitpid(child_, &status, 0); } while (waited < 0 && errno == EINTR);
        EXPECT_EQ(child_, waited);
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0) << status;
    }
    bool valid() const { return child_ > 0; }
    bool Request(char command, Reply* reply) {
        return Transfer(commands_, &command, 1, true) &&
            Transfer(replies_, reply, sizeof(*reply), false);
    }
 private:
    pid_t child_ = -1;
    int commands_ = -1;
    int replies_ = -1;
};

class SharedPageMigration : public testing::Test {
 protected:
    void SetUp() override {
        struct stat info;
        if (stat(kCommand, &info) == 0) return;
        if (errno == ENOENT) GTEST_SKIP() << "requires x86_64 KERNEL_FEATURES=mm_selftest";
        FAIL() << "stat migration command errno=" << errno;
    }
};

TEST_F(SharedPageMigration, SharedAnonymousAcrossForkUsesOneNewFrame) {
    Mapping shared(mmap(nullptr, kPage, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, shared.get());
    shared.word() = kInitial;
    ForkPeer peer(shared.get());
    ASSERT_TRUE(peer.valid());
    ForkPeer::Reply reply{};
    ASSERT_TRUE(peer.Request('r', &reply));
    ASSERT_EQ(kInitial, reply.value);
    const Migration first = Migrate(shared.get());
    ASSERT_TRUE(first.ok) << first.report;
    ASSERT_TRUE(peer.Request('m', &reply));
    ASSERT_EQ(1u, reply.ok);
    EXPECT_EQ(first.new_phys, reply.old_phys);
    EXPECT_EQ(kInitial, reply.value);
    const Migration second = Migrate(shared.get());
    ASSERT_TRUE(second.ok) << second.report;
    EXPECT_EQ(reply.new_phys, second.old_phys);
    ASSERT_TRUE(peer.Request('w', &reply));
    EXPECT_EQ(kChildWrite, shared.word());
    const Migration last = Migrate(shared.get());
    EXPECT_TRUE(last.ok) << last.report;
    EXPECT_EQ(kChildWrite, shared.word());
}

TEST_F(SharedPageMigration, MemfdAliasesRollbackAndReadonlyPermissions) {
    Fd fd(static_cast<int>(syscall(SYS_memfd_create, "mm006-alias", 0)));
    ASSERT_GE(fd.get(), 0) << errno;
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    Mapping first(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    Mapping alias(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, first.get());
    ASSERT_NE(MAP_FAILED, alias.get());
    ASSERT_NE(first.get(), alias.get());
    first.word() = kInitial;
    ASSERT_EQ(kInitial, alias.word());
    Migration previous = Migrate(first.get());
    ASSERT_TRUE(previous.ok) << previous.report;
    for (uint32_t stage = 1; stage <= 5; ++stage) {
        const Migration failed = Migrate(first.get(), stage);
        ASSERT_NE(std::string::npos, failed.report.find("error=injected\n")) << failed.report;
        EXPECT_EQ(kInitial, first.word());
        EXPECT_EQ(kInitial, alias.word());
        const Migration next = Migrate(alias.get());
        ASSERT_TRUE(next.ok) << next.report;
        EXPECT_EQ(previous.new_phys, next.old_phys) << "failed stage=" << stage;
        previous = next;
    }
    ASSERT_EQ(0, mprotect(alias.get(), kPage, PROT_READ));
    const Migration readonly = Migrate(alias.get());
    ASSERT_TRUE(readonly.ok) << readonly.report;
    EXPECT_EQ(previous.new_phys, readonly.old_phys);
    first.word() = kChildWrite;
    EXPECT_EQ(kChildWrite, alias.word());
    // Child writing the protected alias must still fault after migration.
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) { alias.word() = 1; _exit(3); }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV) << status;
    ASSERT_EQ(0, mprotect(alias.get(), kPage, PROT_READ | PROT_WRITE));
    alias.word() = kInitial;
    EXPECT_EQ(kInitial, first.word());
}

TEST_F(SharedPageMigration, ProtNoneAliasRetainsResidentIdentityAndProtection) {
    Fd fd(static_cast<int>(syscall(SYS_memfd_create, "mm006-protnone", 0)));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    Mapping first(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    Mapping alias(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, first.get());
    ASSERT_NE(MAP_FAILED, alias.get());
    first.word() = kInitial;
    ASSERT_EQ(kInitial, alias.word());
    ASSERT_EQ(0, mprotect(alias.get(), kPage, PROT_NONE));
    const Migration initial = Migrate(first.get());
    ASSERT_TRUE(initial.ok) << initial.report;
    const Migration hidden = Migrate(alias.get());
    ASSERT_TRUE(hidden.ok) << hidden.report;
    EXPECT_EQ(initial.new_phys, hidden.old_phys);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) { volatile uint64_t value = alias.word(); (void)value; _exit(3); }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV) << status;
    ASSERT_EQ(0, mprotect(alias.get(), kPage, PROT_READ | PROT_WRITE));
    EXPECT_EQ(kInitial, alias.word());
    alias.word() = kChildWrite;
    EXPECT_EQ(kChildWrite, first.word());
    const Migration restored = Migrate(alias.get());
    ASSERT_TRUE(restored.ok) << restored.report;
    EXPECT_EQ(hidden.new_phys, restored.old_phys);
}

TEST_F(SharedPageMigration, ForkPrivateCowRemainsPrivateAfterMigration) {
    Mapping private_page(mmap(nullptr, kPage, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, private_page.get());
    private_page.word() = kInitial;
    ForkPeer peer(private_page.get());
    ASSERT_TRUE(peer.valid());
    ForkPeer::Reply reply{};
    ASSERT_TRUE(peer.Request('r', &reply));
    ASSERT_EQ(kInitial, reply.value);
    const Migration shared = Migrate(private_page.get());
    ASSERT_TRUE(shared.ok) << shared.report;
    ASSERT_TRUE(peer.Request('m', &reply));
    ASSERT_EQ(1u, reply.ok);
    EXPECT_EQ(shared.new_phys, reply.old_phys);
    const uint64_t child_frame = reply.new_phys;
    private_page.word() = kChildWrite;  // Must take the ordinary COW path.
    ASSERT_TRUE(peer.Request('r', &reply));
    EXPECT_EQ(kInitial, reply.value);
    const Migration copied = Migrate(private_page.get());
    ASSERT_TRUE(copied.ok) << copied.report;
    EXPECT_NE(child_frame, copied.old_phys);
    EXPECT_EQ(kChildWrite, private_page.word());
    ASSERT_TRUE(peer.Request('m', &reply));
    ASSERT_EQ(1u, reply.ok);
    EXPECT_EQ(child_frame, reply.old_phys);
    EXPECT_EQ(kInitial, reply.value);
    ASSERT_EQ(0, mprotect(private_page.get(), kPage, PROT_READ));
    const Migration readonly = Migrate(private_page.get());
    EXPECT_TRUE(readonly.ok) << readonly.report;
    EXPECT_EQ(kChildWrite, private_page.word());
}

class SysvSegment {
 public:
    SysvSegment() : id_(shmget(IPC_PRIVATE, kPage, IPC_CREAT | 0600)) {
        if (id_ < 0) return;
        first_ = shmat(id_, nullptr, 0);
        second_ = shmat(id_, nullptr, 0);
    }
    ~SysvSegment() {
        if (first_ != reinterpret_cast<void*>(-1)) shmdt(first_);
        if (second_ != reinterpret_cast<void*>(-1)) shmdt(second_);
        if (id_ >= 0) shmctl(id_, IPC_RMID, nullptr);
    }
    bool valid() const {
        return id_ >= 0 && first_ != reinterpret_cast<void*>(-1) &&
            second_ != reinterpret_cast<void*>(-1);
    }
    void* first() const { return first_; }
    void* second() const { return second_; }
 private:
    int id_;
    void* first_ = reinterpret_cast<void*>(-1);
    void* second_ = reinterpret_cast<void*>(-1);
};

TEST_F(SharedPageMigration, SysvAttachmentsRetainOneBackingPage) {
    SysvSegment segment;
    ASSERT_TRUE(segment.valid()) << errno;
    auto* first = static_cast<volatile uint64_t*>(segment.first());
    auto* second = static_cast<volatile uint64_t*>(segment.second());
    *first = kInitial;
    ASSERT_EQ(kInitial, *second);
    const Migration migrated = Migrate(segment.first());
    ASSERT_TRUE(migrated.ok) << migrated.report;
    const Migration alias = Migrate(segment.second());
    ASSERT_TRUE(alias.ok) << alias.report;
    EXPECT_EQ(migrated.new_phys, alias.old_phys);
    *second = kChildWrite;
    EXPECT_EQ(kChildWrite, *first);
}

TEST_F(SharedPageMigration, FileCleanAndDirtyMigrationSurviveWritebackAndReread) {
    struct statfs root_fs{};
    ASSERT_EQ(0, statfs("/", &root_fs));
    ASSERT_NE(0x01021994L, root_fs.f_type) << "disk-backed fixture required";
    ASSERT_NE(0x858458f6L, root_fs.f_type) << "disk-backed fixture required";
    dunitest::LoopExt4 filesystem;
    ASSERT_NO_FATAL_FAILURE(filesystem.SetUp("ext4_inode_identity.img", "/", "mm006_migration"));
    ASSERT_NO_FATAL_FAILURE(filesystem.Mount());
    const std::string path = filesystem.mount_point() + "/migration-data";
    std::array<uint8_t, kPage> expected{};
    for (size_t index = 0; index < kPage; ++index) expected[index] = (index * 29) ^ (index >> 4);
    {
        Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
        ASSERT_GE(fd.get(), 0) << errno;
        ASSERT_EQ(static_cast<ssize_t>(kPage), pwrite(fd.get(), expected.data(), kPage, 0));
        ASSERT_EQ(0, fsync(fd.get()));
        {
            Mapping first(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
            Mapping alias(mmap(nullptr, kPage, PROT_READ, MAP_SHARED, fd.get(), 0));
            ASSERT_NE(MAP_FAILED, first.get());
            ASSERT_NE(MAP_FAILED, alias.get());
            EXPECT_EQ(expected[0], *static_cast<volatile uint8_t*>(first.get()));
            EXPECT_EQ(expected[0], *static_cast<volatile uint8_t*>(alias.get()));
            const Migration clean = Migrate(alias.get());
            ASSERT_TRUE(clean.ok) << clean.report;
            auto* bytes = static_cast<volatile uint8_t*>(first.get());
            bytes[7] = expected[7] = 0xe3;
            bytes[kPage - 9] = expected[kPage - 9] = 0x16;
            const Migration dirty = Migrate(first.get());
            ASSERT_TRUE(dirty.ok) << dirty.report;
            EXPECT_EQ(clean.new_phys, dirty.old_phys);
            EXPECT_EQ(0, memcmp(expected.data(), alias.get(), kPage));
            ASSERT_EQ(0, fsync(fd.get()));
            ASSERT_EQ(0, madvise(first.get(), kPage, MADV_DONTNEED));
            EXPECT_EQ(0, memcmp(expected.data(), first.get(), kPage));
        }
        ASSERT_EQ(0, posix_fadvise(fd.get(), 0, kPage, POSIX_FADV_DONTNEED));
        std::array<uint8_t, kPage> reread{};
        ASSERT_EQ(static_cast<ssize_t>(kPage), pread(fd.get(), reread.data(), kPage, 0));
        EXPECT_EQ(expected, reread);
    }
    ASSERT_NO_FATAL_FAILURE(filesystem.Unmount());
    ASSERT_NO_FATAL_FAILURE(filesystem.Rebind());
    ASSERT_NO_FATAL_FAILURE(filesystem.Mount());
    Fd reopened(open(path.c_str(), O_RDONLY));
    ASSERT_GE(reopened.get(), 0);
    std::array<uint8_t, kPage> persisted{};
    ASSERT_EQ(static_cast<ssize_t>(kPage), pread(reopened.get(), persisted.data(), kPage, 0));
    EXPECT_EQ(expected, persisted);
    EXPECT_EQ(0, unlink(path.c_str()));
}

void CheckShmemAdvice(bool relocate) {
    Fd fd(static_cast<int>(syscall(SYS_memfd_create, "mm142-preserve", 0)));
    ASSERT_GE(fd.get(), 0);
    std::array<uint8_t, kPage> expected{};
    for (size_t index = 0; index < kPage; ++index) expected[index] = (index * 29) ^ (index >> 4);
    ASSERT_EQ(static_cast<ssize_t>(kPage), pwrite(fd.get(), expected.data(), kPage, 0));
    {
        Mapping page(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
        ASSERT_NE(MAP_FAILED, page.get());
        static_cast<volatile uint8_t*>(page.get())[7] = expected[7] = 0xe3;
        if (relocate) {
            const Migration moved = Migrate(page.get());
            ASSERT_TRUE(moved.ok) << moved.report;
        }
        ASSERT_EQ(0, fsync(fd.get()));
        ASSERT_EQ(0, memcmp(expected.data(), page.get(), kPage));
    }
    ASSERT_EQ(0, posix_fadvise(fd.get(), 0, kPage, POSIX_FADV_DONTNEED));
    std::array<uint8_t, kPage> reread{};
    ASSERT_EQ(static_cast<ssize_t>(kPage), pread(fd.get(), reread.data(), kPage, 0));
    EXPECT_EQ(expected, reread);
}

// This data-integrity regression also runs on default kernels without the
// optional migration debug interface.
TEST(ShmemFadvise, DontNeedPreservesSyncedBackingWithoutMigration) {
    ASSERT_NO_FATAL_FAILURE(CheckShmemAdvice(false));
}
TEST_F(SharedPageMigration, ShmemAdvicePreservesRelocatedBacking) {
    ASSERT_NO_FATAL_FAILURE(CheckShmemAdvice(true));
}

TEST_F(SharedPageMigration, ManyMappingsSurviveSplitMoveAndFork) {
    constexpr size_t length = 3 * kPage;
    Fd fd(static_cast<int>(syscall(SYS_memfd_create, "mm006-many", 0)));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), length));
    Mapping first(mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0), length);
    Mapping alias(mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0), length);
    Mapping third(mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0), length);
    ASSERT_NE(MAP_FAILED, first.get()); ASSERT_NE(MAP_FAILED, alias.get()); ASSERT_NE(MAP_FAILED, third.get());
    auto at = [](void* base, size_t page) { return static_cast<char*>(base) + page * kPage; };
    for (size_t page = 0; page < 3; ++page) {
        *reinterpret_cast<volatile uint64_t*>(at(first.get(), page)) = kInitial + page;
        ASSERT_EQ(kInitial + page, *reinterpret_cast<volatile uint64_t*>(at(alias.get(), page)));
        ASSERT_EQ(kInitial + page, *reinterpret_cast<volatile uint64_t*>(at(third.get(), page)));
    }
    ASSERT_EQ(0, mprotect(at(first.get(), 1), kPage, PROT_READ));
    Mapping reserved(mmap(nullptr, length, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0), length);
    ASSERT_NE(MAP_FAILED, reserved.get());
    void* destination = mremap(alias.get(), length, length, MREMAP_MAYMOVE | MREMAP_FIXED, reserved.get());
    ASSERT_EQ(reserved.get(), destination) << errno;
    alias.release(); reserved.release();
    Mapping moved(destination, length);
    Migration middle{};
    for (size_t page = 0; page < 3; ++page) {
        const Migration initial = Migrate(at(first.get(), page));
        ASSERT_TRUE(initial.ok) << initial.report;
        const Migration changed = Migrate(at(moved.get(), page));
        ASSERT_TRUE(changed.ok) << changed.report;
        EXPECT_EQ(initial.new_phys, changed.old_phys);
        const Migration last = Migrate(at(third.get(), page));
        ASSERT_TRUE(last.ok) << last.report;
        EXPECT_EQ(changed.new_phys, last.old_phys);
        EXPECT_EQ(kInitial + page, *reinterpret_cast<volatile uint64_t*>(at(first.get(), page)));
        if (page == 1) middle = last;
    }
    ForkPeer peer(at(third.get(), 1));
    ASSERT_TRUE(peer.valid());
    ForkPeer::Reply reply{};
    ASSERT_TRUE(peer.Request('r', &reply));
    EXPECT_EQ(kInitial + 1, reply.value);
    ASSERT_TRUE(peer.Request('m', &reply));
    ASSERT_EQ(1u, reply.ok);
    EXPECT_EQ(middle.new_phys, reply.old_phys);
    ASSERT_TRUE(peer.Request('w', &reply));
    EXPECT_EQ(kChildWrite, *reinterpret_cast<volatile uint64_t*>(at(first.get(), 1)));
    EXPECT_EQ(kChildWrite, *reinterpret_cast<volatile uint64_t*>(at(moved.get(), 1)));
}

TEST_F(SharedPageMigration, SharedRemapAndRepeatedMigrationKeepAliasIdentity) {
    Fd fd(static_cast<int>(syscall(SYS_memfd_create, "mm006-remap", 0)));
    ASSERT_GE(fd.get(), 0) << errno;
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    Mapping current(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    Mapping alias(mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, current.get());
    ASSERT_NE(MAP_FAILED, alias.get());
    current.word() = kInitial;
    EXPECT_EQ(kInitial, alias.word());
    Migration previous = Migrate(current.get());
    EXPECT_TRUE(previous.ok) << previous.report;
    Mapping reservation(mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, reservation.get()) << errno;
    void* moved = mremap(current.get(), kPage, kPage, MREMAP_MAYMOVE | MREMAP_FIXED, reservation.get());
    ASSERT_NE(MAP_FAILED, moved) << errno;
    current.release();
    reservation.release();
    Mapping remapped(moved);
    for (int round = 0; round < 8; ++round) {
        const Migration next = Migrate(round % 2 == 0 ? remapped.get() : alias.get());
        ASSERT_TRUE(next.ok) << next.report;
        EXPECT_EQ(previous.new_phys, next.old_phys);
        alias.word() = kInitial + round;
        EXPECT_EQ(kInitial + round, remapped.word());
        previous = next;
    }
    ASSERT_EQ(0, madvise(remapped.get(), kPage, MADV_DONTNEED));
    EXPECT_EQ(alias.word(), remapped.word());
    const Migration refaulted = Migrate(remapped.get());
    EXPECT_TRUE(refaulted.ok) << refaulted.report;
    EXPECT_EQ(previous.new_phys, refaulted.old_phys);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
