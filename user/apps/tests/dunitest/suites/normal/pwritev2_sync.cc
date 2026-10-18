#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#include "loop_ext4_test_support.h"

namespace {
constexpr unsigned kDsync = 0x02;
constexpr unsigned kSync = 0x04;
constexpr unsigned kAppend = 0x10;
constexpr const char* kInjectPath = "/sys/kernel/debug/errseq/inject";

ssize_t Pwritev2(int fd, const iovec* vectors, size_t count, int64_t offset,
                unsigned flags) {
    return syscall(SYS_pwritev2, fd, vectors, count, static_cast<uint64_t>(offset), 0UL,
                   flags);
}

class Descriptor {
 public:
    explicit Descriptor(int fd = -1) : fd_(fd) {}
    ~Descriptor() { if (fd_ >= 0) close(fd_); }
    Descriptor(const Descriptor&) = delete;
    Descriptor& operator=(const Descriptor&) = delete;
    int get() const { return fd_; }
 private:
    int fd_;
};

void InjectMappingError(int fd, const char* error) {
    Descriptor inject(open(kInjectPath, O_WRONLY | O_CLOEXEC));
    ASSERT_GE(inject.get(), 0) << "requires errseq injection interface: " << strerror(errno);
    char command[96];
    const int length = snprintf(command, sizeof(command), "mapping %d %s\n", fd, error);
    ASSERT_GT(length, 0);
    ASSERT_LT(static_cast<size_t>(length), sizeof(command));
    ASSERT_EQ(length, write(inject.get(), command, length)) << strerror(errno);
}

class Pwritev2Sync : public testing::Test {
 protected:
    void SetUp() override {
        // The existing errseq injector is available in default kernels too.
        // Missing infrastructure must fail rather than hide sync regressions.
        Descriptor inject(open(kInjectPath, O_WRONLY | O_CLOEXEC));
        ASSERT_GE(inject.get(), 0) << "requires errseq injection interface: " << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("ext4_inode_identity.img", "/tmp", "pwritev2_sync"));
        ASSERT_NO_FATAL_FAILURE(fs_.Mount());
    }
    dunitest::LoopExt4 fs_;
};

class Pwritev2TmpfsSync : public testing::Test {
 protected:
    void SetUp() override {
        Descriptor inject(open(kInjectPath, O_WRONLY | O_CLOEXEC));
        ASSERT_GE(inject.get(), 0) << "requires errseq injection interface: " << strerror(errno);
        char path[] = "/tmp/pwritev2-sync-tmpfs-XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(path)) << strerror(errno);
        path_ = path;
        ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
        mounted_ = true;
    }

    void TearDown() override {
        // TestBody's descriptors have already closed, including on a fatal
        // assertion. A normal unmount also detects accidental resource leaks.
        if (mounted_) {
            ASSERT_EQ(0, umount(path_.c_str())) << strerror(errno);
        }
        if (!path_.empty()) {
            EXPECT_EQ(0, rmdir(path_.c_str())) << strerror(errno);
        }
    }

    std::string path_;
    bool mounted_ = false;
};

TEST_F(Pwritev2Sync, Ext4PerOperationSyncReportsAndConsumesWritebackError) {
    for (unsigned sync : {kDsync, kSync}) {
        for (bool append : {false, true}) {
            for (int64_t offset : {int64_t(-1), int64_t(0)}) {
                SCOPED_TRACE(testing::Message() << "sync=" << sync << " append=" << append
                                               << " offset=" << offset);
                const std::string path = fs_.mount_point() + "/transaction";
                Descriptor fd(open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600));
                ASSERT_GE(fd.get(), 0) << strerror(errno);
                ASSERT_EQ(6, write(fd.get(), "abcdef", 6));
                ASSERT_EQ(0, fsync(fd.get()));
                ASSERT_EQ(0, fchmod(fd.get(), 06777));
                ASSERT_EQ(2, lseek(fd.get(), 2, SEEK_SET));
                const int original_flags = fcntl(fd.get(), F_GETFL);
                ASSERT_GE(original_flags, 0);
                Descriptor second(open(path.c_str(), O_RDWR | O_CLOEXEC));
                ASSERT_GE(second.get(), 0);
                Descriptor notify(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
                ASSERT_GE(notify.get(), 0);
                ASSERT_GE(inotify_add_watch(notify.get(), path.c_str(), IN_ATTRIB | IN_MODIFY), 0);
                ASSERT_NO_FATAL_FAILURE(InjectMappingError(fd.get(), "EIO"));

                const pid_t child = fork();
                ASSERT_GE(child, 0);
                if (child == 0) {
                    alarm(5);
                    if (setgid(65534) != 0 || setuid(65534) != 0) _exit(10);
                    char bytes[] = "XY";
                    iovec vectors[] = {{bytes, 1}, {bytes + 1, 1}};
                    errno = 0;
                    const ssize_t result = Pwritev2(fd.get(), vectors, 2, offset,
                                                   sync | (append ? kAppend : 0));
                    if (result != -1 || errno != EIO) _exit(11);
                    // do_writev publishes its local position only when the
                    // final syscall result is nonnegative, even though the
                    // filesystem has already committed data before sync EIO.
                    if (lseek(fd.get(), 0, SEEK_CUR) != 2) _exit(12);
                    if (fcntl(fd.get(), F_GETFL) != original_flags) _exit(13);
                    _exit(0);
                }
                int status = 0;
                pid_t result;
                do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
                ASSERT_EQ(child, result);
                ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
                EXPECT_EQ(0, WEXITSTATUS(status));
                EXPECT_EQ(2, lseek(fd.get(), 0, SEEK_CUR));

                const std::string expected = append ? "abcdefXY" :
                    (offset == -1 ? "abXYef" : "XYcdef");
                char contents[8] = {};
                ASSERT_EQ(static_cast<ssize_t>(expected.size()),
                          pread(fd.get(), contents, expected.size(), 0));
                EXPECT_EQ(expected, std::string(contents, expected.size()));
                struct stat metadata{};
                ASSERT_EQ(0, fstat(fd.get(), &metadata));
                EXPECT_EQ(0U, metadata.st_mode & (S_ISUID | S_ISGID));
                EXPECT_EQ(static_cast<off_t>(expected.size()), metadata.st_size);

                // ATTRIB reflects the completed privilege transaction. MODIFY
                // belongs to the successful final syscall boundary, not merely
                // the earlier positive filesystem write result.
                alignas(inotify_event) char events[1024];
                const ssize_t count = read(notify.get(), events, sizeof(events));
                ASSERT_GT(count, 0) << strerror(errno);
                unsigned mask = 0;
                for (ssize_t position = 0; position < count;) {
                    ASSERT_LE(position + static_cast<ssize_t>(sizeof(inotify_event)), count);
                    const auto* event = reinterpret_cast<const inotify_event*>(events + position);
                    mask |= event->mask;
                    position += sizeof(inotify_event) + event->len;
                    ASSERT_LE(position, count);
                }
                EXPECT_TRUE(mask & IN_ATTRIB);
                EXPECT_FALSE(mask & IN_MODIFY);
                // The operation consumed only its own OFD's mapping event.
                EXPECT_EQ(0, fsync(fd.get())) << strerror(errno);
                errno = 0;
                EXPECT_EQ(-1, fsync(second.get()));
                EXPECT_EQ(EIO, errno);
                EXPECT_EQ(0, fsync(second.get()));
                EXPECT_EQ(0, unlink(path.c_str()));
            }
        }
    }
}

TEST_F(Pwritev2Sync, Ext4OrdinaryVectorWriteLeavesMappingErrorForFsync) {
    const std::string path = fs_.mount_point() + "/ordinary";
    Descriptor fd(open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(6, write(fd.get(), "abcdef", 6));
    ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(InjectMappingError(fd.get(), "ENOSPC"));
    char bytes[] = "XY";
    iovec vector = {bytes, 2};
    ASSERT_EQ(2, Pwritev2(fd.get(), &vector, 1, -1, 0));
    EXPECT_EQ(8, lseek(fd.get(), 0, SEEK_CUR));
    errno = 0;
    EXPECT_EQ(-1, fsync(fd.get()));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(0, unlink(path.c_str()));
}

TEST_F(Pwritev2Sync, SyncErrorFollowsWholeLogicalWriteAcrossScratchChunks) {
    const std::string path = fs_.mount_point() + "/chunked";
    Descriptor fd(open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, fsync(fd.get()));
    std::vector<unsigned char> source(3 * 64 * 1024 + 137);
    for (size_t index = 0; index < source.size(); ++index) source[index] = index * 13 + 7;
    iovec vectors[] = {{source.data(), 17}, {source.data() + 17, 70003},
                      {source.data() + 70020, source.size() - 70020}};
    ASSERT_NO_FATAL_FAILURE(InjectMappingError(fd.get(), "EIO"));
    errno = 0;
    EXPECT_EQ(-1, Pwritev2(fd.get(), vectors, 3, -1, kSync));
    EXPECT_EQ(EIO, errno);
    EXPECT_EQ(0, lseek(fd.get(), 0, SEEK_CUR));
    struct stat metadata{};
    ASSERT_EQ(0, fstat(fd.get(), &metadata));
    ASSERT_EQ(static_cast<off_t>(source.size()), metadata.st_size);
    std::vector<unsigned char> contents(source.size());
    ASSERT_EQ(static_cast<ssize_t>(contents.size()),
              pread(fd.get(), contents.data(), contents.size(), 0));
    EXPECT_EQ(source, contents);
    EXPECT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(0, unlink(path.c_str()));
}

TEST_F(Pwritev2Sync, ScalarAndTransfersPublishSuccessOnlyAfterSynchronization) {
    enum class Operation { Scalar, Sendfile, CopyFileRange, Splice };
    for (int sync : {O_DSYNC, O_SYNC}) {
        for (Operation operation : {Operation::Scalar, Operation::Sendfile,
                                    Operation::CopyFileRange, Operation::Splice}) {
            SCOPED_TRACE(testing::Message() << "open_sync=" << sync
                                           << " operation=" << static_cast<int>(operation));
            const std::string output_path = fs_.mount_point() + "/output";
            const std::string input_path = fs_.mount_point() + "/input";
            Descriptor output(open(output_path.c_str(), O_CREAT | O_TRUNC | O_RDWR |
                                   O_CLOEXEC | sync, 0600));
            Descriptor input(open(input_path.c_str(), O_CREAT | O_TRUNC | O_RDWR |
                                  O_CLOEXEC, 0600));
            ASSERT_GE(output.get(), 0);
            ASSERT_GE(input.get(), 0);
            ASSERT_EQ(6, write(output.get(), "abcdef", 6));
            ASSERT_EQ(6, write(input.get(), "UVWXYZ", 6));
            ASSERT_EQ(0, fsync(input.get()));
            ASSERT_EQ(2, lseek(output.get(), 2, SEEK_SET));
            ASSERT_EQ(1, lseek(input.get(), 1, SEEK_SET));
            Descriptor notify(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
            ASSERT_GE(notify.get(), 0);
            ASSERT_GE(inotify_add_watch(notify.get(), output_path.c_str(), IN_MODIFY), 0);
            int pipe_fds[2];
            ASSERT_EQ(0, pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC));
            Descriptor pipe_read(pipe_fds[0]);
            Descriptor pipe_write(pipe_fds[1]);
            ASSERT_EQ(2, write(pipe_write.get(), "WX", 2));
            ASSERT_NO_FATAL_FAILURE(InjectMappingError(output.get(), "EIO"));
            errno = 0;
            ssize_t result = 0;
            // One small batch avoids the distinct Linux case where a later
            // batch fails after an earlier batch has already returned success.
            switch (operation) {
                case Operation::Scalar:
                    result = write(output.get(), "WX", 2);
                    break;
                case Operation::Sendfile:
                    result = syscall(SYS_sendfile, output.get(), input.get(), nullptr, 2);
                    break;
                case Operation::CopyFileRange:
                    result = syscall(SYS_copy_file_range, input.get(), nullptr,
                                     output.get(), nullptr, 2, 0);
                    break;
                case Operation::Splice:
                    result = syscall(SYS_splice, pipe_read.get(), nullptr,
                                     output.get(), nullptr, 2, 0);
                    break;
            }
            const int error = errno;
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EIO, error) << strerror(error);
            EXPECT_EQ(2, lseek(output.get(), 0, SEEK_CUR));
            EXPECT_EQ(1, lseek(input.get(), 0, SEEK_CUR));
            char contents[6];
            ASSERT_EQ(6, pread(output.get(), contents, sizeof(contents), 0));
            EXPECT_EQ(operation == Operation::Sendfile || operation == Operation::CopyFileRange
                          ? "abVWef" : "abWXef", std::string(contents, sizeof(contents)));
            alignas(inotify_event) char events[1024];
            errno = 0;
            EXPECT_EQ(-1, read(notify.get(), events, sizeof(events)));
            EXPECT_EQ(EAGAIN, errno);
            int available = -1;
            ASSERT_EQ(0, ioctl(pipe_read.get(), FIONREAD, &available));
            EXPECT_EQ(2, available);
            char remaining[2];
            const ssize_t remaining_count = read(pipe_read.get(), remaining, sizeof(remaining));
            EXPECT_EQ(2, remaining_count);
            if (remaining_count == 2) {
                EXPECT_EQ("WX", std::string(remaining, sizeof(remaining)));
            }
            EXPECT_EQ(0, fsync(output.get()));
            EXPECT_EQ(0, unlink(output_path.c_str()));
            EXPECT_EQ(0, unlink(input_path.c_str()));
        }
    }
}

TEST_F(Pwritev2TmpfsSync, TmpfsPerOperationSyncDoesNotInventGenericWriteSync) {
    Descriptor shared_tmp(open("/tmp", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    ASSERT_GE(shared_tmp.get(), 0) << strerror(errno);
    for (unsigned sync : {kDsync, kSync}) {
        ASSERT_EQ(0, syscall(SYS_syncfs, shared_tmp.get())) << strerror(errno);
        const std::string path = path_ + "/transaction";
        Descriptor fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600));
        ASSERT_GE(fd.get(), 0);
        ASSERT_EQ(0, unlink(path.c_str()));
        ASSERT_NO_FATAL_FAILURE(InjectMappingError(fd.get(), "EIO"));
        char byte = 'X';
        iovec vector = {&byte, 1};
        ASSERT_EQ(1, Pwritev2(fd.get(), &vector, 1, -1, sync));
        EXPECT_EQ(1, lseek(fd.get(), 0, SEEK_CUR));
        // shmem_file_write_iter uses noop_fsync and does not consume the
        // mapping errseq. Observe it explicitly with WAIT_AFTER instead.
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_sync_file_range, fd.get(), 0, 0, 4));
        EXPECT_EQ(EIO, errno);
        EXPECT_EQ(0, syscall(SYS_sync_file_range, fd.get(), 0, 0, 4));
        // Mapping injection also records a superblock event. Keep that event
        // observable on this private mount without poisoning later /tmp tests.
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_syncfs, fd.get()));
        EXPECT_EQ(EIO, errno);
        EXPECT_EQ(0, syscall(SYS_syncfs, fd.get())) << strerror(errno);
        EXPECT_EQ(0, syscall(SYS_syncfs, shared_tmp.get())) << strerror(errno);
    }
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
