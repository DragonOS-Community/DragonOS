#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>

namespace {
class Fd {
  public:
    explicit Fd(int fd) : value(fd) {}
    ~Fd() { if (value >= 0) close(value); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int value;
};

long Scalar(int fd, void* buffer, size_t count, bool positional, off_t offset = 0) {
    return positional ? syscall(SYS_pread64, fd, buffer, count, offset)
                      : syscall(SYS_read, fd, buffer, count);
}

long Vector(int fd, const iovec* vectors, int count, bool positional, off_t offset = 0) {
    // Linux x86-64 preadv uses low/high position words; DragonOS consumes the
    // complete fourth argument. These cases fit the low word on both ABIs.
    return positional ? syscall(SYS_preadv, fd, vectors, count, offset, 0UL)
                      : syscall(SYS_readv, fd, vectors, count);
}

void ExpectError(long result, int expected) {
    const int error = errno;
    EXPECT_EQ(-1, result);
    if (result == -1) EXPECT_EQ(expected, error) << strerror(error);
}

class DirectoryReadAdmission : public ::testing::TestWithParam<const char*> {
  protected:
    void SetUp() override {
        const char* path = GetParam();
        if (strcmp(path, "configured") == 0) {
            const char* configured = getenv("DIRECTORY_READ_TEST_DIR");
            path = configured != nullptr ? configured : "/";
        }
        fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        ASSERT_GE(fd, 0) << path << ": " << strerror(errno);
        struct statfs info {};
        ASSERT_EQ(0, fstatfs(fd, &info));
        RecordProperty("filesystem_magic", static_cast<int>(info.f_type));
        if (strcmp(path, "/proc") == 0) ASSERT_EQ(0x9fa0, info.f_type);
    }
    void TearDown() override { if (fd >= 0) close(fd); }
    int fd = -1;
};

TEST_P(DirectoryReadAdmission, ScalarZeroStillInvokesDirectoryOperation) {
    char byte = 'x';
    const off_t initial = lseek(fd, 0, SEEK_CUR);
    ASSERT_GE(initial, 0);
    for (bool positional : {false, true}) {
        for (void* pointer : std::array<void*, 3>{&byte, nullptr, reinterpret_cast<void*>(1)}) {
            SCOPED_TRACE(::testing::Message() << "positional=" << positional << " ptr=" << pointer);
            ExpectError(Scalar(fd, pointer, 0, positional), EISDIR);
        }
    }
    ExpectError(Scalar(fd, &byte, 0, true, 123), EISDIR);
    EXPECT_EQ(initial, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ('x', byte);
}

TEST_P(DirectoryReadAdmission, NumericalAddressAdmissionPrecedesDirectoryOperation) {
    void* inaccessible = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    for (bool positional : {false, true}) {
        // A numerically valid pointer is not probed by generic_read_dir.
        ExpectError(Scalar(fd, inaccessible, 1, positional), EISDIR);
        for (size_t count : {size_t(0), size_t(1)}) {
            ExpectError(Scalar(fd, reinterpret_cast<void*>(UINTPTR_MAX), count, positional), EFAULT);
        }
    }
    EXPECT_EQ(0, munmap(inaccessible, 4096));
}

TEST_P(DirectoryReadAdmission, EmptyVectorsKeepTheirDistinctContract) {
    char byte = 'x';
    iovec zero[] = {{&byte, 0}, {nullptr, 0}, {reinterpret_cast<void*>(1), 0}};
    iovec bad_base = {reinterpret_cast<void*>(UINTPTR_MAX), 0};
    iovec nonempty = {&byte, 1};
    const auto* bad_array = reinterpret_cast<const iovec*>(UINTPTR_MAX);
    for (bool positional : {false, true}) {
        EXPECT_EQ(0, Vector(fd, bad_array, 0, positional));
        EXPECT_EQ(0, Vector(fd, zero, 1, positional));
        EXPECT_EQ(0, Vector(fd, zero, 3, positional));
        ExpectError(Vector(fd, &bad_base, 1, positional), EFAULT);
        ExpectError(Vector(fd, reinterpret_cast<const iovec*>(1), 1, positional), EFAULT);
        ExpectError(Vector(fd, &nonempty, 1, positional), EISDIR);
    }
    EXPECT_EQ('x', byte);
}

TEST_P(DirectoryReadAdmission, NegativeExplicitPositionPrecedesBufferAdmission) {
    ExpectError(Scalar(fd, reinterpret_cast<void*>(UINTPTR_MAX), 0, true, -1), EINVAL);
    ExpectError(Vector(fd, reinterpret_cast<const iovec*>(1), 1, true, -1), EINVAL);
}

INSTANTIATE_TEST_SUITE_P(Filesystems, DirectoryReadAdmission,
                        ::testing::Values("configured", "/tmp", "/proc", "/sys"));

TEST(ReadAdmissionControls, DescriptorAndModeChecksPrecedeZeroLengthCompletion) {
    const auto* bad_array = reinterpret_cast<const iovec*>(1);
    void* bad_buffer = reinterpret_cast<void*>(UINTPTR_MAX);
    Fd path(open("/", O_PATH | O_CLOEXEC));
    ASSERT_GE(path.value, 0);
    for (int fd : {-1, path.value}) {
        ExpectError(Scalar(fd, bad_buffer, 0, false), EBADF);
        ExpectError(Scalar(fd, bad_buffer, 0, true), EBADF);
        ExpectError(Vector(fd, bad_array, 1, false), EBADF);
        ExpectError(Vector(fd, bad_array, 1, true), EBADF);
        ExpectError(Scalar(fd, bad_buffer, 0, true, -1), EINVAL);
        ExpectError(Vector(fd, bad_array, 1, true, -1), EINVAL);
    }
    Fd write_only(open("/dev/null", O_WRONLY | O_CLOEXEC));
    ASSERT_GE(write_only.value, 0);
    char byte = 'x';
    iovec empty = {&byte, 0};
    ExpectError(Scalar(write_only.value, &byte, 0, false), EBADF);
    ExpectError(Scalar(write_only.value, &byte, 0, true), EBADF);
    ExpectError(Vector(write_only.value, &empty, 1, false), EBADF);
    // Keep the zero-count mode control independent of invalid-vector ordering:
    // Linux 6.6 imports vectors before mode admission, while 6.8 reverses it.
}

TEST(ReadAdmissionControls, EmptyRegularAndStreamReadsDoNotConsumeData) {
    char name[] = "/tmp/directory-read-control-XXXXXX";
    Fd regular(mkstemp(name));
    ASSERT_GE(regular.value, 0);
    ASSERT_EQ(0, unlink(name));
    ASSERT_EQ(1, write(regular.value, "r", 1));
    ASSERT_EQ(0, lseek(regular.value, 0, SEEK_SET));
    char byte = 'x';
    EXPECT_EQ(0, Scalar(regular.value, &byte, 0, false));
    EXPECT_EQ(0, Scalar(regular.value, &byte, 0, true));
    EXPECT_EQ(0, lseek(regular.value, 0, SEEK_CUR));
    ASSERT_EQ(1, read(regular.value, &byte, 1));
    EXPECT_EQ('r', byte);

    int pipe_fds[2];
    ASSERT_EQ(0, pipe2(pipe_fds, O_NONBLOCK | O_CLOEXEC));
    Fd pipe_read(pipe_fds[0]), pipe_write(pipe_fds[1]);
    int socket_fds[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, socket_fds));
    Fd socket_read(socket_fds[0]), socket_write(socket_fds[1]);
    for (const auto& pair : {std::array<int, 2>{pipe_read.value, pipe_write.value},
                            std::array<int, 2>{socket_read.value, socket_write.value}}) {
        // Empty streams are nonblocking so a regression fails rather than hangs.
        EXPECT_EQ(0, Scalar(pair[0], &byte, 0, false));
        ExpectError(Scalar(pair[0], &byte, 0, true), ESPIPE);
        ASSERT_EQ(1, write(pair[1], "s", 1));
        EXPECT_EQ(0, Scalar(pair[0], &byte, 0, false));
        ASSERT_EQ(1, read(pair[0], &byte, 1));
        EXPECT_EQ('s', byte);
    }
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
