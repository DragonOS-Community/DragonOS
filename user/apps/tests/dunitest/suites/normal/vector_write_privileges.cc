#include "loop_ext4_test_support.h"

#include <signal.h>
#include <cstdint>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/inotify.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/statvfs.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>

namespace {

constexpr unsigned char kCapability[] = {
    0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};

class OverlayExt4 {
 public:
    ~OverlayExt4() {
        if (mounted_) {
            EXPECT_EQ(0, umount(merged.c_str())) << strerror(errno);
        }
    }
    void SetUp() {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp("ext4_inode_identity.img", "/tmp", "vector_write_overlay"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount());
        lower = disk_.mount_point() + "/lower";
        upper = disk_.mount_point() + "/upper";
        work = disk_.mount_point() + "/work";
        merged = disk_.mount_point() + "/merged";
        for (const std::string* path : {&lower, &upper, &work, &merged}) {
            ASSERT_EQ(0, mkdir(path->c_str(), 0755));
        }
    }
    void Mount() {
        const std::string options = "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + work;
        ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str())) << strerror(errno);
        mounted_ = true;
    }
    const std::string& backing_mount_point() const { return disk_.mount_point(); }
    void Unmount() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount(merged.c_str())) << strerror(errno);
        mounted_ = false;
    }
    void UnmountBacking() { ASSERT_NO_FATAL_FAILURE(disk_.Unmount()); }
    void MountBackingReadOnly() { ASSERT_NO_FATAL_FAILURE(disk_.Mount(MS_RDONLY)); }
    std::string lower, upper, work, merged;
 private:
    dunitest::LoopExt4 disk_;
    bool mounted_ = false;
};

enum class Operation { Writev, Pwritev, Pwritev2, Pwrite64 };
enum class Scenario {
    Fault, Prefix, Empty, ReadOnly, NegativeOffset, AppendFault, AppendPrefix,
    FileLimit, InvalidRange, InvalidMetadata, SingleIovPrefix, EmptyAppend
};
constexpr const char* kOperationNames[] = {"writev", "pwritev", "pwritev2", "pwrite64"};
constexpr const char* kScenarioNames[] = {
    "fault", "prefix", "empty", "read-only", "negative-offset", "append-fault", "append-prefix",
    "file-limit", "invalid-range", "invalid-metadata", "single-iov-prefix", "empty-append"};

ssize_t Write(Operation operation, int fd, const iovec* vectors, int count, off_t offset) {
    if (operation == Operation::Writev) return writev(fd, vectors, count);
    if (operation == Operation::Pwritev) return pwritev(fd, vectors, count, offset);
    if (operation == Operation::Pwrite64) return pwrite(fd, vectors[0].iov_base, vectors[0].iov_len, offset);
    // On all supported 64-bit architectures the low argument is a full word;
    // the high word is ignored. Flags are the sixth argument, including zero.
    return syscall(SYS_pwritev2, fd, vectors, count, offset, 0UL, 0U);
}

class VectorWritePrivileges : public testing::Test {
 protected:
    void SetUp() override {
        ASSERT_EQ(uid_t(0), geteuid()) << "requires the isolated root test guest";
        page_ = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page_, 0);
        memory_ = mmap(nullptr, 2 * page_, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, memory_);
        memset(memory_, 'V', page_);
        ASSERT_EQ(0, mprotect(static_cast<char*>(memory_) + page_, page_, PROT_NONE));
    }
    void TearDown() override {
        if (memory_ != MAP_FAILED) munmap(memory_, 2 * page_);
    }

    void Check(const std::string& directory, bool capabilities, bool privilege_before_limit = false) {
        for (Operation operation : {Operation::Writev, Operation::Pwritev, Operation::Pwritev2, Operation::Pwrite64}) {
            for (Scenario scenario : {Scenario::Fault, Scenario::Prefix, Scenario::Empty,
                                      Scenario::ReadOnly, Scenario::NegativeOffset,
                                      Scenario::AppendFault, Scenario::AppendPrefix, Scenario::FileLimit,
                                      Scenario::InvalidRange, Scenario::InvalidMetadata, Scenario::SingleIovPrefix, Scenario::EmptyAppend}) {
                if (operation == Operation::Writev && scenario == Scenario::NegativeOffset) continue;
                if (operation == Operation::Pwrite64 && scenario == Scenario::InvalidMetadata) continue;
                SCOPED_TRACE(kOperationNames[static_cast<size_t>(operation)]);
                SCOPED_TRACE(kScenarioNames[static_cast<size_t>(scenario)]);
                ASSERT_NO_FATAL_FAILURE(CheckOne(directory, capabilities, operation, scenario,
                                                 "write-privileges", false, privilege_before_limit));
            }
        }
    }

    void CheckOne(const std::string& directory, bool capabilities,
                  Operation operation, Scenario scenario,
                  const std::string& name = "write-privileges", bool prepared = false,
                  bool privilege_before_limit = false) {
        struct ScratchFile {
            std::string path;
            bool remove;
            int setup = -1;
            int writer = -1;
            int watcher = -1;
            ~ScratchFile() {
                if (watcher >= 0) close(watcher);
                if (writer >= 0) close(writer);
                if (setup >= 0) close(setup);
                if (remove) unlink(path.c_str());
            }
        } file{directory + "/" + name, !prepared};
        const std::string& path = file.path;
        int& setup = file.setup;
        setup = open(path.c_str(), prepared ? O_RDONLY : O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(setup, 0) << strerror(errno);
        if (!prepared) {
            ASSERT_EQ(0, ftruncate(setup, 16));
            ASSERT_EQ(0, fchmod(setup, 06777));
        }
        // Revision 2, CAP_CHOWN permitted, no effective flag. The test observes
        // invalidation only; it never executes a capability-bearing file.
        if (capabilities && !prepared) {
            ASSERT_EQ(0, fsetxattr(setup, "security.capability", kCapability,
                                   sizeof(kCapability), 0)) << strerror(errno);
        }
        const bool readonly = scenario == Scenario::ReadOnly;
        const bool append = scenario == Scenario::AppendFault || scenario == Scenario::AppendPrefix
            || scenario == Scenario::EmptyAppend;
        int& fd = file.writer;
        fd = open(path.c_str(), (readonly ? O_RDONLY : O_WRONLY) | (append ? O_APPEND : 0));
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(3, lseek(fd, 3, SEEK_SET));
        file.watcher = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
        ASSERT_GE(file.watcher, 0);
        ASSERT_GE(inotify_add_watch(file.watcher, path.c_str(), IN_ATTRIB | IN_MODIFY), 0);
        const bool prefix = scenario == Scenario::Prefix || scenario == Scenario::AppendPrefix
            || scenario == Scenario::SingleIovPrefix;
        const bool empty = scenario == Scenario::Empty || scenario == Scenario::EmptyAppend;
        const bool negative = scenario == Scenario::NegativeOffset;
        const bool limited = scenario == Scenario::FileLimit;
        const bool invalid_range = scenario == Scenario::InvalidRange;
        const bool invalid_metadata = scenario == Scenario::InvalidMetadata;
        const bool admitted = !empty && !negative && !readonly && (!limited || privilege_before_limit) && !invalid_range && !invalid_metadata;
        const ssize_t expected = prefix ? page_ : (empty ? 0 : -1);
        const int expected_errno = readonly ? EBADF : (negative ? EINVAL : (limited ? EFBIG : EFAULT));
        const off_t start = append ? 16 : 3;
        const off_t expected_position = operation == Operation::Writev && prefix ? start + page_ : 3;
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            alarm(10);
            if (setgid(65534) != 0 || setuid(65534) != 0) _exit(1);
            if (limited) {
                const rlimit limit{3, 3};
                if (signal(SIGXFSZ, SIG_IGN) == SIG_ERR || setrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(4);
            }
            iovec vectors[2] = {{static_cast<char*>(memory_) + page_, empty ? 0U : 1U}, {}};
            int count = 1;
            if (prefix) {
                vectors[0] = {memory_, static_cast<size_t>(page_)};
                vectors[1] = {static_cast<char*>(memory_) + page_, static_cast<size_t>(page_)};
                count = 2;
            }
            if (scenario == Scenario::SingleIovPrefix || (prefix && operation == Operation::Pwrite64)) {
                vectors[0].iov_len = 2 * page_;
                count = 1;
            }
            if (invalid_range) vectors[0].iov_base = reinterpret_cast<void*>(~uintptr_t(0));
            const iovec* argument = invalid_metadata
                ? reinterpret_cast<const iovec*>(~uintptr_t(0)) : vectors;
            errno = 0;
            ssize_t result = Write(operation, fd, argument, count, negative ? -2 : 3);
            if (result != expected || (result == -1 && errno != expected_errno)) _exit(2);
            if (lseek(fd, 0, SEEK_CUR) != expected_position) _exit(3);
            _exit(0);
        }
        int status = 0;
        pid_t waited;
        do { waited = waitpid(child, &status, 0); } while (waited == -1 && errno == EINTR);
        EXPECT_EQ(child, waited);
        EXPECT_TRUE(WIFEXITED(status)) << status;
        EXPECT_EQ(0, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        bool attrib_seen = false, modify_seen = false;
        alignas(inotify_event) char events[4096];
        for (;;) {
            ssize_t size = read(file.watcher, events, sizeof(events));
            if (size == -1 && errno == EINTR) continue;
            if (size == -1 && errno == EAGAIN) break;
            ASSERT_GT(size, 0) << strerror(errno);
            size_t cursor = 0;
            while (cursor < static_cast<size_t>(size)) {
                ASSERT_GE(static_cast<size_t>(size) - cursor, sizeof(inotify_event));
                const auto* event = reinterpret_cast<const inotify_event*>(events + cursor);
                const size_t event_size = sizeof(*event) + event->len;
                ASSERT_LE(event_size, static_cast<size_t>(size) - cursor);
                if (event->mask & IN_ATTRIB) {
                    EXPECT_FALSE(modify_seen) << "privilege ATTRIB must precede data MODIFY";
                    attrib_seen = true;
                }
                if (event->mask & IN_MODIFY) modify_seen = true;
                cursor += event_size;
            }
        }
        EXPECT_EQ(admitted, attrib_seen);
        EXPECT_EQ(prefix, modify_seen);
        struct stat metadata{};
        ASSERT_EQ(0, fstat(setup, &metadata));
        EXPECT_EQ(mode_t(admitted ? 0777 : 06777), metadata.st_mode & 07777);
        EXPECT_EQ(prefix ? start + page_ : 16, metadata.st_size);
        if (capabilities) {
            unsigned char actual[32];
            errno = 0;
            ssize_t size = fgetxattr(setup, "security.capability", actual, sizeof(actual));
            if (admitted) {
                EXPECT_EQ(-1, size);
                EXPECT_EQ(ENODATA, errno);
            } else {
                EXPECT_EQ(static_cast<ssize_t>(sizeof(kCapability)), size);
                if (size == sizeof(kCapability)) {
                    EXPECT_EQ(0, memcmp(actual, kCapability, size));
                }
            }
        }
        if (!prefix || append) {
            char original[16];
            ASSERT_EQ(ssize_t(sizeof(original)), pread(setup, original, sizeof(original), 0));
            EXPECT_EQ(std::string(sizeof(original), '\0'), std::string(original, sizeof(original)));
        }
        if (prefix) {
            std::string bytes(page_, '\0');
            ASSERT_EQ(page_, pread(setup, bytes.data(), bytes.size(), start));
            EXPECT_EQ(std::string(page_, 'V'), bytes);
        }

    }

    void CheckScalarZeroLimits(const std::string& directory, bool capabilities) {
        const std::string path = directory + "/empty-limit";
        for (bool positional : {false, true}) {
            for (bool append : {false, true}) {
                for (off_t position : {off_t(0), off_t(3)}) {
                    for (rlim_t limit : {rlim_t(0), rlim_t(3)}) {
                        SCOPED_TRACE(positional);
                        SCOPED_TRACE(append);
                        SCOPED_TRACE(position);
                        SCOPED_TRACE(limit);
                        int fd = open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY | (append ? O_APPEND : 0), 0600);
                        ASSERT_GE(fd, 0);
                        struct File {
                            int value;
                            const std::string& path;
                            ~File() { close(value); unlink(path.c_str()); }
                        } file{fd, path};
                        ASSERT_EQ(0, ftruncate(fd, 16));
                        ASSERT_EQ(0, fchmod(fd, 06777));
                        if (capabilities) {
                            ASSERT_EQ(0, fsetxattr(fd, "security.capability", kCapability, sizeof(kCapability), 0));
                        }
                        ASSERT_EQ(position, lseek(fd, position, SEEK_SET));
                        pid_t child = fork();
                        ASSERT_GE(child, 0);
                        if (child == 0) {
                            alarm(10);
                            if (setgid(65534) != 0 || setuid(65534) != 0) _exit(1);
                            const rlimit limits{limit, limit};
                            if (setrlimit(RLIMIT_FSIZE, &limits) != 0) _exit(2);
                            // A zero write must not raise SIGXFSZ. Exit from a
                            // handler so a failure never creates a core file.
                            if (signal(SIGXFSZ, [](int) { _exit(5); }) == SIG_ERR) _exit(3);
                            ssize_t result = positional ? syscall(SYS_pwrite64, fd, 0UL, 0UL, 3L)
                                                        : syscall(SYS_write, fd, 0UL, 0UL);
                            if (result != 0 || lseek(fd, 0, SEEK_CUR) != position) _exit(4);
                            _exit(0);
                        }
                        int status = 0;
                        pid_t waited;
                        do { waited = waitpid(child, &status, 0); } while (waited == -1 && errno == EINTR);
                        ASSERT_EQ(child, waited);
                        EXPECT_TRUE(WIFEXITED(status)) << status;
                        EXPECT_EQ(0, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
                        struct stat metadata{};
                        ASSERT_EQ(0, fstat(fd, &metadata));
                        EXPECT_EQ(mode_t(06777), metadata.st_mode & 07777);
                        EXPECT_EQ(16, metadata.st_size);
                        EXPECT_EQ(position, lseek(fd, 0, SEEK_CUR));
                        if (capabilities) {
                            EXPECT_EQ(ssize_t(sizeof(kCapability)), fgetxattr(fd, "security.capability", nullptr, 0));
                        }
                    }
                }
            }
        }
    }

    void CheckRamfsContentAndSetid(const std::string& directory) {
        const size_t length = 128 * 1024 + 37;
        const std::string bytes(length, 'R');
        const std::string path = directory + "/mapped-source";
        for (int mapping : {0, MAP_PRIVATE, MAP_SHARED}) {
            SCOPED_TRACE(mapping);
            for (Operation operation : {Operation::Writev, Operation::Pwritev,
                                        Operation::Pwritev2, Operation::Pwrite64}) {
                SCOPED_TRACE(kOperationNames[static_cast<size_t>(operation)]);
                int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
                ASSERT_GE(fd, 0) << strerror(errno);
                struct File {
                    int value;
                    const std::string& path;
                    ~File() { close(value); unlink(path.c_str()); }
                } file{fd, path};
                ASSERT_EQ(ssize_t(length), pwrite(fd, bytes.data(), length, 0));
                ASSERT_EQ(0, fchmod(fd, 06777));
                pid_t child = fork();
                ASSERT_GE(child, 0);
                if (child == 0) {
                    alarm(10);
                    void* source = const_cast<char*>(bytes.data());
                    if (mapping != 0) {
                        source = mmap(nullptr, length, PROT_READ, mapping, fd, 0);
                        if (source == MAP_FAILED || madvise(source, length, MADV_DONTNEED) != 0) _exit(1);
                        // Leave the source PTEs absent: the primary write must
                        // fault this same inode while retaining its content guard.
                    }
                    if (lseek(fd, length, SEEK_SET) != off_t(length)) _exit(2);
                    iovec vectors[] = {{source, 65519},
                                      {static_cast<char*>(source) + 65519, length - 65519}};
                    if (operation == Operation::Pwrite64) vectors[0].iov_len = length;
                    ssize_t written = Write(operation, fd, vectors, 2, length);
                    if (mapping != 0) munmap(source, length);
                    if (written != ssize_t(length)) _exit(3);
                    if (lseek(fd, 0, SEEK_CUR) != off_t((operation == Operation::Writev ? 2 : 1) * length)) _exit(4);
                    _exit(0);
                }
                int status = 0;
                pid_t waited;
                do { waited = waitpid(child, &status, 0); } while (waited == -1 && errno == EINTR);
                ASSERT_EQ(child, waited);
                ASSERT_TRUE(WIFEXITED(status)) << status;
                ASSERT_EQ(0, WEXITSTATUS(status));
                std::string actual(2 * length, '\0');
                ASSERT_EQ(ssize_t(actual.size()), pread(fd, actual.data(), actual.size(), 0));
                EXPECT_EQ(bytes + bytes, actual);
                struct stat metadata{};
                ASSERT_EQ(0, fstat(fd, &metadata));
                // The root caller retains CAP_FSETID. Successful normal and
                // mapped-source writes must not clear its setid bits.
                EXPECT_EQ(mode_t(06777), metadata.st_mode & 07777);
            }
        }
    }

    void CheckFifoCommunication(const std::string& path, bool copy_up = false,
                                const std::string& writer_path = "") {
        int reader = open(path.c_str(), O_RDONLY | O_NONBLOCK | O_CLOEXEC);
        ASSERT_GE(reader, 0);
        struct CloseFd { int value; ~CloseFd() { close(value); } } close_reader{reader};
        if (copy_up) {
            ASSERT_EQ(0, chmod(path.c_str(), 0640));
            // A sibling mutation invalidates directory lookup caches. The
            // writer must find the same FIFO owner after a fresh lookup.
            const std::string marker = path + ".marker";
            int fd = open(marker.c_str(), O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
            ASSERT_GE(fd, 0);
            EXPECT_EQ(0, close(fd));
            EXPECT_EQ(0, unlink(marker.c_str()));
        }
        int writer = open((writer_path.empty() ? path : writer_path).c_str(), O_WRONLY | O_NONBLOCK | O_CLOEXEC);
        ASSERT_GE(writer, 0);
        CloseFd close_writer{writer};
        ASSERT_EQ(4, write(writer, "pipe", 4));
        char actual[4];
        ASSERT_EQ(ssize_t(sizeof(actual)), read(reader, actual, sizeof(actual)));
        EXPECT_EQ(0, memcmp(actual, "pipe", sizeof(actual)));
    }

    long page_ = 0;
    void* memory_ = MAP_FAILED;
};

TEST(OverlayMountLifetime, OrdinaryUnmountReleasesBackingMount) {
    OverlayExt4 overlay;
    ASSERT_NO_FATAL_FAILURE(overlay.SetUp());
    ASSERT_NO_FATAL_FAILURE(overlay.Mount());
    errno = 0;
    EXPECT_EQ(-1, umount(overlay.backing_mount_point().c_str()));
    EXPECT_EQ(EBUSY, errno);

    const int fd = open(overlay.merged.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, umount(overlay.merged.c_str()));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, close(fd));
    ASSERT_NO_FATAL_FAILURE(overlay.Unmount());
    // No sleep/retry: successful ordinary unmount completes final shutdown.
    ASSERT_NO_FATAL_FAILURE(overlay.UnmountBacking());
}

TEST(OverlayMountLifetime, UnmountReclaimsCachedUnlinkedUpperInode) {
    OverlayExt4 overlay;
    ASSERT_NO_FATAL_FAILURE(overlay.SetUp());
    ASSERT_NO_FATAL_FAILURE(overlay.Mount());
    // Overlay mount initialization may create persistent work directories.
    // Measure after that, isolating the file inode allocated below.
    struct statvfs before {};
    ASSERT_EQ(0, statvfs(overlay.backing_mount_point().c_str(), &before));
    const std::string path = overlay.merged + "/cached-unlinked";
    const int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(1, write(fd, "x", 1));
    ASSERT_EQ(0, close(fd));
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(overlay.Unmount());
    ASSERT_NO_FATAL_FAILURE(overlay.UnmountBacking());
    // Read-only reopen must see completed reclaim, not rely on orphan recovery
    // during a later writable mount to finish Overlay's deferred cache drops.
    ASSERT_NO_FATAL_FAILURE(overlay.MountBackingReadOnly());
    struct statvfs after {};
    ASSERT_EQ(0, statvfs(overlay.backing_mount_point().c_str(), &after));
    EXPECT_EQ(before.f_ffree, after.f_ffree);
    ASSERT_NO_FATAL_FAILURE(overlay.UnmountBacking());
}

TEST_F(VectorWritePrivileges, TmpfsRemovesSetidBeforeUserCopy) {
    char directory[] = "/tmp/vector-write-privileges-XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory));
    int result = mount("none", directory, "tmpfs", 0, nullptr);
    if (result != 0) {
        const int error = errno;
        rmdir(directory);
        FAIL() << "private tmpfs mount: " << strerror(error);
    }
    Check(directory, false);
    CheckScalarZeroLimits(directory, false);
    EXPECT_EQ(0, umount(directory)) << strerror(errno);
    EXPECT_EQ(0, rmdir(directory));
}

TEST_F(VectorWritePrivileges, RamfsRemovesSetidBeforeUserCopy) {
    char directory[] = "/tmp/ramfs-write-privileges-XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory));
    ASSERT_EQ(0, mount("none", directory, "ramfs", 0, nullptr)) << strerror(errno);
    Check(directory, false);
    CheckScalarZeroLimits(directory, false);
    CheckRamfsContentAndSetid(directory);
    EXPECT_EQ(0, umount(directory)) << strerror(errno);
    EXPECT_EQ(0, rmdir(directory));
}

TEST_F(VectorWritePrivileges, Ext4RemovesSetidAndCapabilitiesBeforeUserCopy) {
    dunitest::LoopExt4 disk;
    ASSERT_NO_FATAL_FAILURE(disk.SetUp("ext4_inode_identity.img", "/tmp", "vector_write_privileges"));
    ASSERT_NO_FATAL_FAILURE(disk.Mount());
    Check(disk.mount_point(), true);
    CheckScalarZeroLimits(disk.mount_point(), true);
}

TEST_F(VectorWritePrivileges, OverlayUpperRetainsCallerPrivilegeStageBeforeCopy) {
    OverlayExt4 overlay;
    ASSERT_NO_FATAL_FAILURE(overlay.SetUp());
    ASSERT_NO_FATAL_FAILURE(overlay.Mount());
    // Linux ovl_write_iter removes privileges before the backing filesystem's
    // generic_write_checks applies RLIMIT_FSIZE; native files reverse those stages.
    Check(overlay.merged, true, true);
    CheckScalarZeroLimits(overlay.merged, true);
    // A directory returned by mkdir must keep the Overlay owner for children.
    const std::string nested = overlay.merged + "/nested";
    ASSERT_EQ(0, mkdir(nested.c_str(), 0755));
    Check(nested, true, true);

    // Exercise the public mknod(S_IFREG) path without duplicating the matrix.
    const std::string created = overlay.merged + "/mknod-file";
    ASSERT_EQ(0, mknod(created.c_str(), S_IFREG | 0600, 0));
    struct RemoveFile { const std::string& path; ~RemoveFile() { unlink(path.c_str()); } } cleanup{created};
    int fd = open(created.c_str(), O_RDWR | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
    ASSERT_EQ(0, ftruncate(fd, 16));
    ASSERT_EQ(0, fchmod(fd, 06777));
    ASSERT_EQ(0, fsetxattr(fd, "security.capability", kCapability, sizeof(kCapability), 0));
    CheckOne(overlay.merged, true, Operation::Writev, Scenario::FileLimit,
             "mknod-file", true, true);

    const std::string fifo = nested + "/namedpipe";
    ASSERT_EQ(0, mkfifo(fifo.c_str(), 0600));
    RemoveFile fifo_cleanup{fifo};
    ASSERT_NO_FATAL_FAILURE(CheckFifoCommunication(fifo));
    const std::string fifo_alias = nested + "/fifo-alias";
    ASSERT_EQ(0, link(fifo.c_str(), fifo_alias.c_str()));
    RemoveFile fifo_alias_cleanup{fifo_alias};
    ASSERT_NO_FATAL_FAILURE(CheckFifoCommunication(fifo, false, fifo_alias));

    const std::string null_node = nested + "/null";
    ASSERT_EQ(0, mknod(null_node.c_str(), S_IFCHR | 0600, makedev(1, 3)));
    RemoveFile null_cleanup{null_node};
    int null_fd = open(null_node.c_str(), O_RDWR | O_CLOEXEC);
    ASSERT_GE(null_fd, 0);
    CloseFd close_null{null_fd};
    EXPECT_EQ(4, write(null_fd, "null", 4));
    char byte;
    EXPECT_EQ(0, read(null_fd, &byte, 1));
}

TEST_F(VectorWritePrivileges, OverlayCopyUpFaultPreservesLowerAndRemovesUpperPrivileges) {
    OverlayExt4 overlay;
    ASSERT_NO_FATAL_FAILURE(overlay.SetUp());
    for (int operation = 0; operation < 4; ++operation) {
        for (int prefix = 0; prefix < 2; ++prefix) {
            std::string name = "lower-" + std::to_string(operation) + "-" + std::to_string(prefix);
            int fd = open((overlay.lower + "/" + name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
            ASSERT_GE(fd, 0);
            struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
            ASSERT_EQ(0, ftruncate(fd, 16));
            ASSERT_EQ(0, fchmod(fd, 06777));
            ASSERT_EQ(0, fsetxattr(fd, "security.capability", kCapability, sizeof(kCapability), 0));
        }
    }
    ASSERT_EQ(0, mkfifo((overlay.lower + "/namedpipe").c_str(), 0600));
    ASSERT_NO_FATAL_FAILURE(overlay.Mount());
    {
        int lower_reader = open((overlay.lower + "/namedpipe").c_str(), O_RDONLY | O_NONBLOCK);
        ASSERT_GE(lower_reader, 0);
        struct CloseFd { int value; ~CloseFd() { close(value); } } close_reader{lower_reader};
        errno = 0;
        int writer = open((overlay.merged + "/namedpipe").c_str(), O_WRONLY | O_NONBLOCK);
        const int error = errno;
        if (writer >= 0) close(writer);
        EXPECT_EQ(-1, writer) << "Overlay and lower inodes own separate FIFO endpoints";
        EXPECT_EQ(ENXIO, error);
    }
    ASSERT_NO_FATAL_FAILURE(CheckFifoCommunication(overlay.merged + "/namedpipe"));
    struct stat fifo_metadata{};
    ASSERT_EQ(0, stat((overlay.lower + "/namedpipe").c_str(), &fifo_metadata));
    EXPECT_TRUE(S_ISFIFO(fifo_metadata.st_mode));
    errno = 0;
    EXPECT_EQ(-1, stat((overlay.upper + "/namedpipe").c_str(), &fifo_metadata));
    EXPECT_EQ(ENOENT, errno);
    ASSERT_NO_FATAL_FAILURE(CheckFifoCommunication(overlay.merged + "/namedpipe", true));
    ASSERT_EQ(0, stat((overlay.upper + "/namedpipe").c_str(), &fifo_metadata));
    EXPECT_TRUE(S_ISFIFO(fifo_metadata.st_mode));
    EXPECT_EQ(mode_t(0640), fifo_metadata.st_mode & 0777);
    ASSERT_EQ(0, stat((overlay.lower + "/namedpipe").c_str(), &fifo_metadata));
    EXPECT_EQ(mode_t(0600), fifo_metadata.st_mode & 0777);
    for (int operation = 0; operation < 4; ++operation) {
        for (int prefix = 0; prefix < 2; ++prefix) {
            std::string name = "lower-" + std::to_string(operation) + "-" + std::to_string(prefix);
            SCOPED_TRACE(name);
            ASSERT_NO_FATAL_FAILURE(CheckOne(overlay.merged, true, static_cast<Operation>(operation),
                                              prefix ? Scenario::Prefix : Scenario::Fault, name, true));
            struct stat lower{}, upper{};
            ASSERT_EQ(0, stat((overlay.lower + "/" + name).c_str(), &lower));
            EXPECT_EQ(mode_t(06777), lower.st_mode & 07777);
            EXPECT_EQ(16, lower.st_size);
            unsigned char capability[sizeof(kCapability)]{};
            ASSERT_EQ(ssize_t(sizeof(kCapability)), getxattr((overlay.lower + "/" + name).c_str(),
                      "security.capability", capability, sizeof(capability)));
            EXPECT_EQ(0, memcmp(kCapability, capability, sizeof(kCapability)));
            ASSERT_EQ(0, stat((overlay.upper + "/" + name).c_str(), &upper));
            EXPECT_EQ(mode_t(0777), upper.st_mode & 07777);
            EXPECT_EQ(prefix ? 3 + page_ : 16, upper.st_size);
            errno = 0;
            EXPECT_EQ(-1, getxattr((overlay.upper + "/" + name).c_str(), "security.capability", nullptr, 0));
            EXPECT_EQ(ENODATA, errno);
        }
    }
}

TEST_F(VectorWritePrivileges, OverlaySameFileMappedSourceDoesNotReenterContentLocks) {
    OverlayExt4 overlay;
    ASSERT_NO_FATAL_FAILURE(overlay.SetUp());
    ASSERT_NO_FATAL_FAILURE(overlay.Mount());
    const size_t length = 3 * 64 * 1024 + 31;
    for (bool map_backing : {false, true}) {
        SCOPED_TRACE(map_backing ? "backing-vma" : "overlay-vma");
        for (Operation operation : {Operation::Writev, Operation::Pwritev, Operation::Pwritev2, Operation::Pwrite64}) {
            SCOPED_TRACE(kOperationNames[static_cast<size_t>(operation)]);
            const std::string path = overlay.merged + "/mapped-source";
            int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
            ASSERT_GE(fd, 0);
            struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
            const std::string bytes(length, 'M');
            ASSERT_EQ(ssize_t(length), pwrite(fd, bytes.data(), length, 0));
            int backing = open((overlay.upper + "/mapped-source").c_str(), O_RDONLY | O_CLOEXEC);
            ASSERT_GE(backing, 0);
            CloseFd close_backing{backing};
            pid_t child = fork();
            ASSERT_GE(child, 0);
            if (child == 0) {
                alarm(10);
                // Both mappings refer to the same actual source file: exercise
                // Overlay VMA delegation as well as a direct backing VMA.
                void* source = mmap(nullptr, length, PROT_READ, MAP_PRIVATE, map_backing ? backing : fd, 0);
                if (source == MAP_FAILED || madvise(source, length, MADV_DONTNEED) != 0) _exit(1);
                if (lseek(fd, length, SEEK_SET) != off_t(length)) _exit(2);
                iovec vectors[] = {{source, 65519}, {static_cast<char*>(source) + 65519, 65571},
                                  {static_cast<char*>(source) + 131090, length - 131090}};
                if (operation == Operation::Pwrite64) vectors[0].iov_len = length;
                ssize_t result = Write(operation, fd, vectors, 3, length);
                munmap(source, length);
                if (result != ssize_t(length)) _exit(3);
                if (lseek(fd, 0, SEEK_CUR) != off_t((operation == Operation::Writev ? 2 : 1) * length)) _exit(4);
                _exit(0);
            }
            int status;
            pid_t result;
            do { result = waitpid(child, &status, 0); } while (result == -1 && errno == EINTR);
            ASSERT_EQ(child, result);
            ASSERT_TRUE(WIFEXITED(status)) << status;
            ASSERT_EQ(0, WEXITSTATUS(status));
            std::string actual(2 * length, '\0');
            ASSERT_EQ(ssize_t(actual.size()), pread(fd, actual.data(), actual.size(), 0));
            EXPECT_EQ(bytes + bytes, actual);
            EXPECT_EQ(0, unlink(path.c_str()));
        }
    }
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
