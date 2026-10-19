#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {

class ScopedFd {
public:
    explicit ScopedFd(int fd) : fd_(fd) {}
    ~ScopedFd() { if (fd_ >= 0) close(fd_); }
    ScopedFd(const ScopedFd&) = delete;
    ScopedFd& operator=(const ScopedFd&) = delete;
    int get() const { return fd_; }
private:
    int fd_;
};

constexpr off_t kCapacity = 4096;

class LoopAdmission : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_LOOP_ADMISSION_BACKING_DIR");
        std::string pattern = std::string(directory ? directory : "/tmp") + "/loop_admission_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end());
        path.push_back('\0');
        backing_ = mkstemp(path.data());
        ASSERT_GE(backing_, 0) << strerror(errno);
        path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_, kCapacity));
        std::array<char, kCapacity> data;
        data.fill('A');
        ASSERT_EQ(kCapacity, pwrite(backing_, data.data(), data.size(), 0));
        ScopedFd control(open("/dev/loop-control", O_RDWR));
        ASSERT_GE(control.get(), 0) << strerror(errno);
        const int minor = ioctl(control.get(), LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0) << strerror(errno);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        // Retain the same writable OFD from before the readonly binding.
        loop_ = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_, 0) << strerror(errno);
    }

    void TearDown() override {
        if (loop_ >= 0) {
            // Failed pre-fix admission may unexpectedly bind an invalid
            // candidate; clean that real state too, without masking assertions.
            loop_info64 status {};
            const bool actually_bound = ioctl(loop_, LOOP_GET_STATUS64, &status) == 0;
            if (bound_ || actually_bound) {
                EXPECT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
            }
            close(loop_);
        }
        if (backing_ >= 0) close(backing_);
        if (!path_.empty()) unlink(path_.c_str());
    }

    void Bind(bool readonly) {
        if (readonly) {
            ScopedFd candidate(open(path_.c_str(), O_RDONLY));
            ASSERT_GE(candidate.get(), 0);
            ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, candidate.get())) << strerror(errno);
        } else {
            ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, backing_)) << strerror(errno);
        }
        bound_ = true;
    }

    void ExpectIoctlError(unsigned long command, int candidate, int expected) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, command, candidate));
        EXPECT_EQ(expected, errno) << "command " << command << ", candidate " << candidate;
    }

    void ExpectOriginalDataAndPosition() {
        EXPECT_EQ(0, lseek(loop_, 0, SEEK_CUR));
        std::array<char, kCapacity> data;
        ASSERT_EQ(kCapacity, pread(backing_, data.data(), data.size(), 0));
        for (char byte : data) ASSERT_EQ('A', byte);
    }

    void ExpectBinding(bool readonly) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
        EXPECT_EQ(readonly, (status.lo_flags & LO_FLAGS_READ_ONLY) != 0);
        uint64_t capacity = 0;
        ASSERT_EQ(0, ioctl(loop_, BLKGETSIZE64, &capacity));
        EXPECT_EQ(static_cast<uint64_t>(kCapacity), capacity);
    }

    int loop_ = -1;
    int backing_ = -1;
    bool bound_ = false;
    std::string path_;
    std::string loop_path_;
};

TEST_F(LoopAdmission, ReadonlyScalarZeroAndBeyondRejectBeforePayload) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_write, loop_, nullptr, 0));
    EXPECT_EQ(EPERM, errno);
    for (off_t offset : {off_t(0), kCapacity, kCapacity + 1}) {
        errno = 0;
        EXPECT_EQ(-1, pwrite(loop_, nullptr, 0, offset));
        EXPECT_EQ(EPERM, errno) << "zero at " << offset;
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_pwrite64, loop_, reinterpret_cast<void*>(1), 1, offset));
        EXPECT_EQ(EPERM, errno) << "nonempty at " << offset;
    }
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(true));
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, ReadonlyDirectRejectsBeforeAlignmentAndFault) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_EQ(0, fcntl(loop_, F_SETFL, fcntl(loop_, F_GETFL) | O_DIRECT));
    std::array<char, 513> data {};
    errno = 0;
    EXPECT_EQ(-1, pwrite(loop_, data.data() + 1, 1, 1));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_pwrite64, loop_, reinterpret_cast<void*>(1), 512, off_t(0)));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(loop_, nullptr, 0, kCapacity + 1));
    EXPECT_EQ(EPERM, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, ReadonlyEmptyVectorsUseGenericZeroContract) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    iovec empty {reinterpret_cast<void*>(1), 0};
    EXPECT_EQ(0, syscall(SYS_writev, loop_, reinterpret_cast<void*>(1), 0));
    EXPECT_EQ(0, writev(loop_, &empty, 1));
    EXPECT_EQ(0, pwritev(loop_, &empty, 1, kCapacity + 1));
    ASSERT_EQ(0, fcntl(loop_, F_SETFL, fcntl(loop_, F_GETFL) | O_DIRECT));
    EXPECT_EQ(0, writev(loop_, &empty, 1));
    EXPECT_EQ(0, pwritev(loop_, &empty, 1, 1));
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, ReadonlyVectorImportsBeforeDeviceAndPayloadFaultsAfterIt) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_writev, loop_, reinterpret_cast<void*>(1), 1));
    EXPECT_EQ(EFAULT, errno);
    iovec bad_payload {reinterpret_cast<void*>(1), 1};
    errno = 0;
    EXPECT_EQ(-1, writev(loop_, &bad_payload, 1));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, pwritev(loop_, &bad_payload, 1, kCapacity + 1));
    EXPECT_EQ(EPERM, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, NumericalAndDescriptorAdmissionPrecedeReadonlyState) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_write, loop_, ~uintptr_t(0), 1));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_write, loop_, ~uintptr_t(0), 0));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_pwrite64, loop_, nullptr, 1, off_t(-1)));
    EXPECT_EQ(EINVAL, errno);
    ScopedFd read_only(open(loop_path_.c_str(), O_RDONLY));
    ASSERT_GE(read_only.get(), 0);
    errno = 0;
    EXPECT_EQ(-1, write(read_only.get(), nullptr, 0));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, writev(read_only.get(), nullptr, 0));
    EXPECT_EQ(EBADF, errno);
}

TEST_F(LoopAdmission, ScalarZeroChecksNumericalRangeForRegularAndBlockFiles) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    for (int fd : {backing_, loop_}) {
        EXPECT_EQ(0, syscall(SYS_write, fd, nullptr, 0));
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_write, fd, ~uintptr_t(0), 0));
        EXPECT_EQ(EFAULT, errno) << "fd " << fd;
    }
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, WritableZeroDoesNotCheckCapacityOrPayload) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    EXPECT_EQ(0, write(loop_, nullptr, 0));
    EXPECT_EQ(0, pwrite(loop_, nullptr, 0, kCapacity + 1));
    iovec empty {reinterpret_cast<void*>(1), 0};
    EXPECT_EQ(0, writev(loop_, &empty, 1));
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_pwrite64, loop_, reinterpret_cast<void*>(1), 1, kCapacity));
    EXPECT_EQ(ENOSPC, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, UnboundChangeChecksFdThenStateBeforeCandidateTypeAndCycle) {
    ScopedFd directory(open("/tmp", O_RDONLY | O_DIRECTORY));
    ScopedFd path_only(open(path_.c_str(), O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_only.get(), 0);
    ExpectIoctlError(LOOP_CHANGE_FD, -1, EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, path_only.get(), EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, directory.get(), ENXIO);
    ExpectIoctlError(LOOP_CHANGE_FD, backing_, ENXIO);
    ExpectIoctlError(LOOP_CHANGE_FD, loop_, ENXIO);
    loop_info64 status {};
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS64, &status));
    EXPECT_EQ(ENXIO, errno);
}

TEST_F(LoopAdmission, BoundSetChecksFdThenBusyBeforeCandidateTypeAndCycle) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ScopedFd directory(open("/tmp", O_RDONLY | O_DIRECTORY));
    ScopedFd path_only(open(path_.c_str(), O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_only.get(), 0);
    ExpectIoctlError(LOOP_SET_FD, -1, EBADF);
    ExpectIoctlError(LOOP_SET_FD, path_only.get(), EBADF);
    ExpectIoctlError(LOOP_SET_FD, directory.get(), EBUSY);
    ExpectIoctlError(LOOP_SET_FD, loop_, EBUSY);
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(false));
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, WritableChangeChecksReadonlyBeforeCandidateCycle) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ScopedFd directory(open("/tmp", O_RDONLY | O_DIRECTORY));
    ScopedFd path_only(open(path_.c_str(), O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_only.get(), 0);
    ExpectIoctlError(LOOP_CHANGE_FD, -1, EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, path_only.get(), EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, directory.get(), EINVAL);
    ExpectIoctlError(LOOP_CHANGE_FD, loop_, EINVAL);
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(false));
}

TEST_F(LoopAdmission, ReadonlyChangeValidatesCandidateAfterStateAndPreservesBindingOnError) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ScopedFd directory(open("/tmp", O_RDONLY | O_DIRECTORY));
    ScopedFd path_only(open(path_.c_str(), O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_only.get(), 0);
    ExpectIoctlError(LOOP_CHANGE_FD, -1, EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, path_only.get(), EBADF);
    ExpectIoctlError(LOOP_CHANGE_FD, directory.get(), EINVAL);
    ExpectIoctlError(LOOP_CHANGE_FD, loop_, EBADF);
    // A writable regular candidate remains valid for a readonly loop.
    EXPECT_EQ(0, ioctl(loop_, LOOP_CHANGE_FD, backing_)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(true));
    std::array<char, 512> data;
    ASSERT_EQ(static_cast<ssize_t>(data.size()), pread(loop_, data.data(), data.size(), 0));
    for (char byte : data) ASSERT_EQ('A', byte);
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, UnprivilegedReadonlyControlMayChangeReadonlyBacking) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ScopedFd control(open(loop_path_.c_str(), O_RDONLY));
    ASSERT_GE(control.get(), 0) << strerror(errno);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        // Fail a stuck configuration syscall without stalling the runner.
        alarm(10);
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(1);
        errno = 0;
        if (ioctl(control.get(), LOOP_CHANGE_FD, -1) != -1 || errno != EBADF) _exit(2);
        if (ioctl(control.get(), LOOP_CHANGE_FD, backing_) != 0) _exit(3);
        loop_info64 status {};
        if (ioctl(control.get(), LOOP_GET_STATUS64, &status) != 0 ||
            (status.lo_flags & LO_FLAGS_READ_ONLY) == 0) _exit(4);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "configuration child signal/status " << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(true));
    ASSERT_NO_FATAL_FAILURE(ExpectOriginalDataAndPosition());
}

TEST_F(LoopAdmission, UnboundSetRejectsInvalidTypeAndSelfAfterFdAdmission) {
    ScopedFd directory(open("/tmp", O_RDONLY | O_DIRECTORY));
    ScopedFd path_only(open(path_.c_str(), O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_only.get(), 0);
    ExpectIoctlError(LOOP_SET_FD, path_only.get(), EBADF);
    ExpectIoctlError(LOOP_SET_FD, directory.get(), EINVAL);
    ExpectIoctlError(LOOP_SET_FD, loop_, EBADF);
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_NO_FATAL_FAILURE(ExpectBinding(false));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
