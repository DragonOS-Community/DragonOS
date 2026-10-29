#include <gtest/gtest.h>

#include <cerrno>
#include <cstring>
#include <cstdint>
#include <climits>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

namespace {

class WriteCapabilityAdmission : public testing::Test {
 protected:
    void SetUp() override {
        epoll_ = epoll_create1(EPOLL_CLOEXEC);
        ASSERT_GE(epoll_, 0) << strerror(errno);
        ns_ = open("/proc/self/ns/mnt", O_RDONLY | O_CLOEXEC);
        ASSERT_GE(ns_, 0) << strerror(errno);
    }
    void TearDown() override {
        if (epoll_ >= 0) close(epoll_);
        if (ns_ >= 0) close(ns_);
    }
    void Error(ssize_t result, int expected) {
        const int error = errno;
        EXPECT_EQ(-1, result);
        EXPECT_EQ(expected, error) << strerror(error);
    }
    void NoWriteMethod(int fd, int ordinary_error = EINVAL) {
        ASSERT_GE(fd, 0) << strerror(errno);
        struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
        char byte = 'x';
        iovec data{&byte, 1}, empty{nullptr, 0}, fault{nullptr, 1};
        Error(syscall(SYS_write, fd, &byte, 1UL), ordinary_error);
        Error(syscall(SYS_write, fd, 0UL, 0UL), ordinary_error);
        Error(syscall(SYS_write, fd, ~0UL, 1UL), ordinary_error);
        for (const iovec* vector : {&data, &empty, &fault}) {
            Error(writev(fd, vector, 1), ordinary_error);
        }
        Error(writev(fd, nullptr, 0), ordinary_error);
        Error(syscall(SYS_pwrite64, fd, 0UL, 0UL, 0L), ESPIPE);
        Error(pwritev(fd, &empty, 1, 0), ESPIPE);
        Error(syscall(SYS_pwritev2, fd, &empty, 1UL, 0L, 0UL, 0U), ESPIPE);
    }
    int epoll_ = -1;
    int ns_ = -1;
};

TEST_F(WriteCapabilityAdmission, NoWriteMethodRejectsScalarEvenEmptyOrBadAddress) {
    char byte = 'x';
    for (size_t count : {size_t(0), size_t(1)}) {
        Error(syscall(SYS_write, epoll_, &byte, count), EINVAL);
        Error(syscall(SYS_write, epoll_, 0UL, count), EINVAL);
        Error(syscall(SYS_write, epoll_, ~0UL, count), EINVAL);
    }
}

TEST_F(WriteCapabilityAdmission, NoWriteMethodRejectsVectorBeforePayloadCopy) {
    char byte = 'x';
    iovec valid{&byte, 1}, fault{nullptr, 1}, empty{nullptr, 0};
    for (const iovec* vector : {&valid, &fault, &empty}) {
        Error(writev(epoll_, vector, 1), EINVAL);
    }
    Error(writev(epoll_, nullptr, 0), EINVAL);
}

TEST_F(WriteCapabilityAdmission, ReadOnlyNamespaceRejectsWriteBeforePayloadCopy) {
    char byte = 'x';
    iovec valid{&byte, 1}, fault{nullptr, 1}, empty{nullptr, 0};
    Error(syscall(SYS_write, ns_, &byte, 1), EBADF);
    Error(syscall(SYS_write, ns_, 0UL, 1UL), EBADF);
    Error(syscall(SYS_write, ns_, 0UL, 0UL), EBADF);
    for (const iovec* vector : {&valid, &fault, &empty}) {
        Error(writev(ns_, vector, 1), EBADF);
    }
    Error(writev(ns_, nullptr, 0), EBADF);
}

TEST_F(WriteCapabilityAdmission, Linux66ImportsVectorBeforeWriteCapabilityOrMode) {
    // Linux 6.6 vfs_writev imports metadata before do_iter_write checks WRITE
    // and CAN_WRITE. Newer host kernels may deliberately choose another order.
    const auto* invalid = reinterpret_cast<const iovec*>(~uintptr_t(0));
    Error(syscall(SYS_writev, epoll_, invalid, 1UL), EFAULT);
    Error(syscall(SYS_writev, ns_, invalid, 1UL), EFAULT);
    Error(syscall(SYS_writev, epoll_, invalid, 1025UL), EINVAL);
    Error(syscall(SYS_writev, ns_, invalid, 1025UL), EINVAL);
}

TEST_F(WriteCapabilityAdmission, PositionalCapabilityPrecedesVectorImport) {
    const auto* invalid = reinterpret_cast<const iovec*>(~uintptr_t(0));
    iovec empty{nullptr, 0}, fault{nullptr, 1};
    for (const iovec* vector : std::initializer_list<const iovec*>{&empty, &fault, invalid}) {
        Error(pwritev(epoll_, vector, 1, 0), ESPIPE);
        Error(syscall(SYS_pwritev2, epoll_, vector, 1UL, 0L, 0UL, 0U), ESPIPE);
    }
    Error(syscall(SYS_pwrite64, epoll_, 0UL, 0UL, 0L), ESPIPE);
    Error(syscall(SYS_pwrite64, epoll_, ~0UL, 1UL, 0L), ESPIPE);
}

TEST_F(WriteCapabilityAdmission, PositionalNamespaceRetainsModeAndMetadataOrder) {
    iovec empty{nullptr, 0}, fault{nullptr, 1};
    for (const iovec* vector : {&empty, &fault}) {
        Error(pwritev(ns_, vector, 1, 0), EBADF);
        Error(syscall(SYS_pwritev2, ns_, vector, 1UL, 0L, 0UL, 0U), EBADF);
    }
    const auto* invalid = reinterpret_cast<const iovec*>(~uintptr_t(0));
    Error(pwritev(ns_, invalid, 1, 0), EFAULT);
    Error(syscall(SYS_pwritev2, ns_, invalid, 1UL, 0L, 0UL, 0U), EFAULT);
    Error(syscall(SYS_pwrite64, ns_, 0UL, 0UL, 0L), EBADF);
}

TEST_F(WriteCapabilityAdmission, NegativeExplicitPositionPrecedesFdAndMetadata) {
    const auto* invalid = reinterpret_cast<const iovec*>(~uintptr_t(0));
    for (int fd : {epoll_, ns_, -1}) {
        Error(pwritev(fd, invalid, 1, -2), EINVAL);
        Error(syscall(SYS_pwritev2, fd, invalid, 1UL, -2L, 0UL, 0U), EINVAL);
        Error(syscall(SYS_pwrite64, fd, 0UL, 0UL, -2L), EINVAL);
    }
}

TEST_F(WriteCapabilityAdmission, NamespaceWriteOpenIsRejectedByOwner) {
    for (int flags : {O_WRONLY, O_RDWR}) {
        errno = 0;
        int fd = open("/proc/self/ns/mnt", flags | O_CLOEXEC);
        const int error = errno;
        if (fd >= 0) close(fd);
        EXPECT_EQ(-1, fd);
        EXPECT_EQ(EPERM, error);
    }
}

TEST_F(WriteCapabilityAdmission, TimerfdHasNoWriteMethod) {
    NoWriteMethod(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC));
}

TEST_F(WriteCapabilityAdmission, SignalfdHasNoWriteMethod) {
    sigset_t mask;
    ASSERT_EQ(0, sigemptyset(&mask));
    ASSERT_EQ(0, sigaddset(&mask, SIGUSR1));
    NoWriteMethod(signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC));
}

TEST_F(WriteCapabilityAdmission, InotifyReadOnlyAccessPrecedesMissingWriteMethod) {
    NoWriteMethod(inotify_init1(IN_NONBLOCK | IN_CLOEXEC), EBADF);
}

TEST_F(WriteCapabilityAdmission, PidfdHasNoWriteMethod) {
    NoWriteMethod(static_cast<int>(syscall(SYS_pidfd_open, getpid(), 0)));
}

TEST_F(WriteCapabilityAdmission, LoopControlHasNoWriteMethod) {
    NoWriteMethod(open("/dev/loop-control", O_RDWR | O_CLOEXEC));
}

TEST_F(WriteCapabilityAdmission, LoopControlSeekHonorsModeBeforeItsNoopMethod) {
    int fd = open("/dev/loop-control", O_RDWR | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
    for (off_t offset : {off_t(-1), off_t(0), off_t(17), off_t(LLONG_MAX)}) {
        for (int whence : {SEEK_SET, SEEK_CUR, SEEK_END, SEEK_DATA, SEEK_HOLE, -1, 99}) {
            SCOPED_TRACE(offset);
            SCOPED_TRACE(whence);
            // ksys_lseek validates whence before vfs_llseek checks LSEEK.
            // nonseekable_open clears that mode: its noop method is unreachable.
            Error(lseek(fd, offset, whence), whence < 0 || whence > SEEK_HOLE ? EINVAL : ESPIPE);
        }
    }
}

TEST_F(WriteCapabilityAdmission, FscontextHasNoWriteMethod) {
    NoWriteMethod(static_cast<int>(syscall(SYS_fsopen, "tmpfs", 1U)));
}

TEST_F(WriteCapabilityAdmission, PerfEventHasNoWriteMethod) {
    perf_event_attr attributes{};
    attributes.type = PERF_TYPE_SOFTWARE;
    attributes.size = sizeof(attributes);
    attributes.config = PERF_COUNT_SW_BPF_OUTPUT;
    attributes.sample_type = PERF_SAMPLE_RAW;
    NoWriteMethod(static_cast<int>(syscall(SYS_perf_event_open, &attributes, -1, 0, -1, 0)));
}

TEST_F(WriteCapabilityAdmission, ProcDirectMissingMethodDiffersFromGlobalWrapper) {
    struct ProcCase { const char* path; int ordinary_error; int positional_error; };
    for (const ProcCase& item : {ProcCase{"/proc/self/stat", EINVAL, ESPIPE},
                                 ProcCase{"/proc/self/cmdline", EINVAL, EINVAL},
                                 ProcCase{"/proc/version", EIO, ESPIPE}}) {
        SCOPED_TRACE(item.path);
        int fd = open(item.path, O_WRONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0) << strerror(errno);
        struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
        Error(syscall(SYS_write, fd, 0UL, 0UL), item.ordinary_error);
        iovec empty{nullptr, 0}, data{const_cast<char*>("x"), 1};
        // Empty vectors finish before an existing write method is called.
        if (item.ordinary_error == EIO) EXPECT_EQ(0, writev(fd, &empty, 1));
        else Error(writev(fd, &empty, 1), item.ordinary_error);
        Error(writev(fd, &data, 1), item.ordinary_error);
        Error(pwritev(fd, &empty, 1, 0), item.positional_error);
    }
}

TEST_F(WriteCapabilityAdmission, WritableDevicesAndEventfdRetainTheirMethods) {
    for (const char* path : {"/dev/null", "/dev/zero"}) {
        int fd = open(path, O_WRONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0);
        iovec vectors[] = {{const_cast<char*>("abc"), 3}, {const_cast<char*>("de"), 2}};
        EXPECT_EQ(5, writev(fd, vectors, 2));
        EXPECT_EQ(0, writev(fd, nullptr, 0));
        EXPECT_EQ(0, close(fd));
    }
    int fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    ASSERT_GE(fd, 0);
    uint64_t value = 7;
    iovec vector{&value, sizeof(value)};
    EXPECT_EQ(ssize_t(sizeof(value)), writev(fd, &vector, 1));
    EXPECT_EQ(0, writev(fd, nullptr, 0));
    uint64_t observed = 0;
    EXPECT_EQ(ssize_t(sizeof(observed)), read(fd, &observed, sizeof(observed)));
    EXPECT_EQ(value, observed);
    EXPECT_EQ(0, close(fd));
}

TEST_F(WriteCapabilityAdmission, ScalarEmptyDatagramSendsButEmptyVectorDoesNot) {
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, pair));
    EXPECT_EQ(0, syscall(SYS_write, pair[0], 0UL, 0UL));
    char byte;
    EXPECT_EQ(0, recv(pair[1], &byte, 1, MSG_DONTWAIT));
    iovec empty{nullptr, 0};
    EXPECT_EQ(0, writev(pair[0], &empty, 1));
    EXPECT_EQ(0, writev(pair[0], nullptr, 0));
    Error(recv(pair[1], &byte, 1, MSG_DONTWAIT), EAGAIN);
    EXPECT_EQ(0, close(pair[0]));
    EXPECT_EQ(0, close(pair[1]));
}

TEST_F(WriteCapabilityAdmission, SignedPositionOverflowDoesNotReachPrivilegeStage) {
    int fd = static_cast<int>(syscall(SYS_memfd_create, "write-range", 1U));
    ASSERT_GE(fd, 0);
    struct CloseFd { int value; ~CloseFd() { close(value); } } close_fd{fd};
    ASSERT_EQ(0, fchmod(fd, 06777));
    ASSERT_EQ(off_t(LLONG_MAX), lseek(fd, LLONG_MAX, SEEK_SET));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(10);
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(1);
        char byte = 'x';
        if (write(fd, &byte, 1) != -1 || errno != EINVAL) _exit(2);
        if (pwrite(fd, &byte, 1, LLONG_MAX) != -1 || errno != EINVAL) _exit(3);
        if (lseek(fd, 0, SEEK_CUR) != LLONG_MAX) _exit(4);
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
    EXPECT_EQ(0, metadata.st_size);
    EXPECT_EQ(mode_t(06777), metadata.st_mode & 07777);
    EXPECT_EQ(off_t(LLONG_MAX), lseek(fd, 0, SEEK_CUR));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
