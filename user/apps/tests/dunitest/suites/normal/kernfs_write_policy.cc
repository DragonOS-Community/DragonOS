#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {
enum class Operation { Write, Writev, Pwrite, Pwritev, Pwritev2, Pwritev2Current };
bool UsesPosition(Operation operation) {
    return operation == Operation::Write || operation == Operation::Writev ||
           operation == Operation::Pwritev2Current;
}

ssize_t Write(Operation operation, int fd, const void* first, size_t first_len,
              const void* second, size_t second_len, off_t offset) {
    iovec vectors[] = {{const_cast<void*>(first), first_len},
                       {const_cast<void*>(second), second_len}};
    switch (operation) {
        case Operation::Write: return write(fd, first, first_len);
        case Operation::Writev: return writev(fd, vectors, 2);
        case Operation::Pwrite: return pwrite(fd, first, first_len, offset);
        case Operation::Pwritev: return pwritev(fd, vectors, 2, offset);
        case Operation::Pwritev2:
            return syscall(SYS_pwritev2, fd, vectors, 2, offset, 0UL, 0U);
        case Operation::Pwritev2Current:
            return syscall(SYS_pwritev2, fd, vectors, 2, -1L, 0UL, 0U);
    }
    return -1;
}

volatile sig_atomic_t size_signals;
void OnSizeSignal(int) { ++size_signals; }

class KernfsWritePolicy : public testing::TestWithParam<Operation> {
 protected:
    void SetUp() override {
        ASSERT_EQ(0U, geteuid()) << "This suite requires a writable cgroup2 hierarchy";
        static unsigned sequence;
        directory_ = "/sys/fs/cgroup/kernfs_write_policy_" + std::to_string(getpid()) + "_" +
                     std::to_string(sequence++);
        ASSERT_EQ(0, mkdir(directory_.c_str(), 0755)) << strerror(errno);
        created_ = true;
        path_ = directory_ + "/cgroup.freeze";
        // No process is moved into this group and no ancestor controller is
        // enabled. Even the freeze=1 checks affect only our disposable group.
        EXPECT_EQ("0\n", ReadValue());
    }

    void TearDown() override {
        if (created_) {
            EXPECT_EQ(0, rmdir(directory_.c_str())) << strerror(errno);
        }
    }

    std::string ReadValue() {
        const int fd = open(path_.c_str(), O_RDONLY | O_CLOEXEC);
        if (fd < 0) return "open error: " + std::string(strerror(errno));
        char value[32];
        const ssize_t length = read(fd, value, sizeof(value));
        close(fd);
        return length >= 0 ? std::string(value, length) : "read error";
    }

    enum class Input { Valid, OutOfRange, Empty, Fault, Page, TooLong, TooLongFault };

    void Check(rlim_t limit, bool append, off_t offset, Input input = Input::Valid) {
        SCOPED_TRACE(testing::Message() << "limit=" << limit << " append=" << append
                                      << " offset=" << offset << " input=" << int(input));
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            alarm(10);
            struct sigaction action = {};
            action.sa_handler = OnSizeSignal;
            sigemptyset(&action.sa_mask);
            if (sigaction(SIGXFSZ, &action, nullptr) != 0) _exit(10);
            size_signals = 0;
            rlimit limits;
            if (getrlimit(RLIMIT_FSIZE, &limits) != 0) _exit(11);
            limits.rlim_cur = limit;
            if (setrlimit(RLIMIT_FSIZE, &limits) != 0) _exit(12);
            const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC | (append ? O_APPEND : 0));
            if (fd < 0) _exit(13);
            const off_t initial = UsesPosition(GetParam()) ? offset : 3;
            if (lseek(fd, initial, SEEK_SET) != initial) _exit(14);
            const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
            std::string payload = input == Input::OutOfRange ? "2\n" : "1\n";
            if (input == Input::Page || input == Input::TooLong || input == Input::TooLongFault) {
                payload.assign(page + (input == Input::Page ? 0 : 1), ' ');
                payload[0] = '1';
                payload.back() = '\n';
            }
            const char* text = payload.data();
            const bool scalar = GetParam() == Operation::Write || GetParam() == Operation::Pwrite;
            const void* first = text;
            const void* second = text + 1;
            size_t first_len = scalar ? payload.size() : 1, second_len = payload.size() - 1;
            void* inaccessible = MAP_FAILED;
            if (input == Input::Empty) first_len = second_len = 0;
            if (input == Input::Fault || input == Input::TooLongFault) {
                inaccessible = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (inaccessible == MAP_FAILED) _exit(15);
                char* boundary = static_cast<char*>(inaccessible) + page;
                boundary[-1] = '1';
                if (mprotect(boundary, page, PROT_NONE) != 0) _exit(15);
                if (input == Input::TooLongFault) first = second = boundary;
                else if (scalar) first = boundary - 1;
                else second = boundary;
            }
            errno = 0;
            const ssize_t result = Write(GetParam(), fd, first, first_len, second, second_len, offset);
            const int error = errno;
            const off_t position = lseek(fd, 0, SEEK_CUR);
            close(fd);
            if (inaccessible != MAP_FAILED) munmap(inaccessible, page * 2);
            const ssize_t expected = input == Input::Valid ? 2 : input == Input::Page ? page :
                                     input == Input::Empty ? 0 : -1;
            const int expected_error = input == Input::OutOfRange ? ERANGE :
                (input == Input::TooLong || input == Input::TooLongFault) ? E2BIG : EFAULT;
            const off_t expected_position =
                initial + (UsesPosition(GetParam()) && expected > 0 ? expected : 0);
            if (result != expected || (expected < 0 && error != expected_error) ||
                position != expected_position || size_signals != 0) {
                dprintf(STDERR_FILENO,
                        "KERNFS_POLICY op=%d limit=%llu append=%d offset=%lld input=%d "
                        "result=%zd errno=%d position=%lld SIGXFSZ=%d expected=%zd/%lld\n",
                        int(GetParam()), static_cast<unsigned long long>(limit), append,
                        static_cast<long long>(offset), int(input), result, error,
                        static_cast<long long>(position), int(size_signals), expected,
                        static_cast<long long>(expected_position));
                _exit(16);
            }
            _exit(0);
        }
        int status = 0;
        pid_t result;
        do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
        ASSERT_EQ(child, result);
        ASSERT_TRUE(WIFEXITED(status)) << status;
        EXPECT_EQ(0, WEXITSTATUS(status));
        EXPECT_EQ((input == Input::Valid || input == Input::Page) ? "1\n" : "0\n", ReadValue());
    }

    void CheckLimit(rlim_t limit) {
        for (bool append : {false, true}) {
            for (off_t offset : {off_t(0), off_t(7)}) Check(limit, append, offset);
        }
    }

    std::string directory_, path_;
    bool created_ = false;
};

TEST_P(KernfsWritePolicy, ZeroFileSizeLimitDoesNotRejectAControlCommand) { CheckLimit(0); }
TEST_P(KernfsWritePolicy, SmallFileSizeLimitDoesNotTruncateAControlCommand) { CheckLimit(1); }
TEST_P(KernfsWritePolicy, AppendPreservesTheOwnerCursorWithoutUsingSyntheticEof) {
    CheckLimit(RLIM_INFINITY);
}
TEST_P(KernfsWritePolicy, OwnerErrorPrecedesFileSizeLimitWithoutChangingValue) {
    for (bool append : {false, true}) Check(0, append, 7, Input::OutOfRange);
}
TEST_P(KernfsWritePolicy, EmptyWriteKeepsValueAndCursorAtZeroFileSizeLimit) {
    Check(0, true, 7, Input::Empty);
}
TEST_P(KernfsWritePolicy, FaultedCommandHasNoPartialControlSideEffect) {
    Check(0, true, 7, Input::Fault);
    Check(RLIM_INFINITY, false, 0, Input::Fault);
}
TEST_P(KernfsWritePolicy, OnePageControlCommandIsAcceptedWhole) {
    Check(RLIM_INFINITY, false, 0, Input::Page);
}
TEST_P(KernfsWritePolicy, OversizedControlCommandDoesNotCommitAPrefix) {
    Check(RLIM_INFINITY, false, 0, Input::TooLong);
}
TEST_P(KernfsWritePolicy, AtomicSizeAdmissionPrecedesPayloadFault) {
    Check(0, true, 7, Input::TooLongFault);
}

INSTANTIATE_TEST_SUITE_P(Operations, KernfsWritePolicy,
                        testing::Values(Operation::Write, Operation::Writev, Operation::Pwrite,
                                        Operation::Pwritev, Operation::Pwritev2,
                                        Operation::Pwritev2Current));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
