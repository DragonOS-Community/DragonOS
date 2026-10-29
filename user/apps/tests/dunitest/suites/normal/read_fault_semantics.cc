#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <poll.h>
#include <pty.h>
#include <termios.h>
#include <signal.h>
#include <sys/signalfd.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <vector>

namespace {
class Pipe {
  public:
    explicit Pipe(int flags = 0) { valid = pipe2(fds, O_NONBLOCK | O_CLOEXEC | flags) == 0; }
    ~Pipe() {
        if (valid) {
            close(fds[0]);
            close(fds[1]);
        }
    }
    int fds[2] = {-1, -1};
    bool valid;
};

class FaultMapping {
  public:
    FaultMapping() : page(static_cast<size_t>(sysconf(_SC_PAGESIZE))) {
        bytes = static_cast<unsigned char*>(mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        valid = bytes != MAP_FAILED && mprotect(bytes + page, page, PROT_NONE) == 0;
    }
    ~FaultMapping() {
        if (bytes != MAP_FAILED) munmap(bytes, page * 2);
    }
    size_t page;
    unsigned char* bytes;
    bool valid;
};

ssize_t ReadVariant(int fd, void* output, size_t count, bool vectored) {
    if (!vectored) return read(fd, output, count);
    // The copy must span iovecs within one logical pipe buffer.
    iovec iovs[] = {{output, count / 2},
                    {static_cast<unsigned char*>(output) + count / 2, count - count / 2}};
    return readv(fd, iovs, 2);
}

void ExpectRemaining(int fd, const std::vector<unsigned char>& expected) {
    std::vector<unsigned char> actual(expected.size() + 1);
    ASSERT_EQ(static_cast<ssize_t>(expected.size()), read(fd, actual.data(), actual.size()))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(actual.data(), expected.data(), expected.size()));
    errno = 0;
    EXPECT_EQ(-1, read(fd, actual.data(), 1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(ReadFaultSemantics, PipeFaultPreservesCurrentLogicalBuffer) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid) << strerror(errno);
    const std::vector<unsigned char> payload(128, 0x71);
    for (bool vectored : {false, true}) {
        SCOPED_TRACE(vectored);
        Pipe pipe;
        ASSERT_TRUE(pipe.valid);
        ASSERT_EQ(128, write(pipe.fds[1], payload.data(), payload.size()));
        errno = 0;
        EXPECT_EQ(-1, ReadVariant(pipe.fds[0], mapping.bytes + mapping.page - 64, 128,
                                 vectored));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], payload));
    }
}

TEST(ReadFaultSemantics, PipeInitialFaultDoesNotConsumeData) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    const std::vector<unsigned char> payload(128, 0x36);
    for (bool vectored : {false, true}) {
        Pipe pipe;
        ASSERT_TRUE(pipe.valid);
        ASSERT_EQ(128, write(pipe.fds[1], payload.data(), payload.size()));
        errno = 0;
        EXPECT_EQ(-1, ReadVariant(pipe.fds[0], mapping.bytes + mapping.page, 128, vectored));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], payload));
    }
}

TEST(ReadFaultSemantics, PipePartialFaultCommitsCompletedBuffers) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    const std::vector<unsigned char> payload(mapping.page * 2, 0x52);
    for (bool vectored : {false, true}) {
        Pipe pipe;
        ASSERT_TRUE(pipe.valid);
        ASSERT_EQ(static_cast<ssize_t>(payload.size()),
                  write(pipe.fds[1], payload.data(), payload.size()));
        ASSERT_EQ(static_cast<ssize_t>(mapping.page),
                  ReadVariant(pipe.fds[0], mapping.bytes, payload.size(), vectored));
        EXPECT_EQ(0, memcmp(mapping.bytes, payload.data(), mapping.page));
        ASSERT_NO_FATAL_FAILURE(
            ExpectRemaining(pipe.fds[0], std::vector<unsigned char>(mapping.page, 0x52)));
    }
}

TEST(ReadFaultSemantics, PipeMergedWritesKeepOneCommitBoundary) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    Pipe pipe;
    ASSERT_TRUE(pipe.valid);
    const std::vector<unsigned char> payload(128, 0x49);
    ASSERT_EQ(64, write(pipe.fds[1], payload.data(), 64));
    ASSERT_EQ(64, write(pipe.fds[1], payload.data() + 64, 64));
    errno = 0;
    EXPECT_EQ(-1, ReadVariant(pipe.fds[0], mapping.bytes + mapping.page - 64, 128, true));
    EXPECT_EQ(EFAULT, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], payload));
}

TEST(ReadFaultSemantics, TeePreventsTailMergeAndPreservesSource) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    Pipe source;
    Pipe destination;
    ASSERT_TRUE(source.valid && destination.valid);
    const std::vector<unsigned char> payload(128, 0x6b);
    const std::vector<unsigned char> tail(64, 0x91);
    ASSERT_EQ(128, write(source.fds[1], payload.data(), payload.size()));
    ASSERT_EQ(128, syscall(SYS_tee, source.fds[0], destination.fds[1], 128, SPLICE_F_NONBLOCK));
    ASSERT_EQ(64, write(destination.fds[1], tail.data(), tail.size()));
    // tee's copied buffer is nonmergeable. Its complete 128 bytes can commit
    // before copying the independently written tail faults.
    EXPECT_EQ(128, read(destination.fds[0], mapping.bytes + mapping.page - 128, 192));
    EXPECT_EQ(0, memcmp(mapping.bytes + mapping.page - 128, payload.data(), payload.size()));
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(destination.fds[0], tail));
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(source.fds[0], payload));
}

TEST(ReadFaultSemantics, PipeSplicePreservesWholeAndPartialTransferBoundaries) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    for (bool partial : {false, true}) {
        SCOPED_TRACE(partial);
        Pipe source;
        Pipe destination;
        ASSERT_TRUE(source.valid && destination.valid);
        const std::vector<unsigned char> payload(128, 0x2d);
        const std::vector<unsigned char> tail(64, 0x84);
        ASSERT_EQ(128, write(source.fds[1], payload.data(), payload.size()));
        const size_t transferred = partial ? 64 : 128;
        ASSERT_EQ(static_cast<ssize_t>(transferred),
                  splice(source.fds[0], nullptr, destination.fds[1], nullptr, transferred,
                         SPLICE_F_NONBLOCK));
        ASSERT_EQ(64, write(destination.fds[1], tail.data(), tail.size()));
        // Resize may change byte-ring placement, never the logical buffers.
        ASSERT_GE(fcntl(destination.fds[0], F_SETPIPE_SZ, mapping.page * 2),
                  static_cast<int>(mapping.page * 2));
        errno = 0;
        const ssize_t result = read(destination.fds[0],
                                    mapping.bytes + mapping.page - transferred,
                                    transferred + tail.size());
        if (partial) {
            // Partial transfer clears CAN_MERGE, leaving two commit units.
            EXPECT_EQ(static_cast<ssize_t>(transferred), result);
            ASSERT_NO_FATAL_FAILURE(ExpectRemaining(destination.fds[0], tail));
            ASSERT_NO_FATAL_FAILURE(
                ExpectRemaining(source.fds[0], std::vector<unsigned char>(64, 0x2d)));
        } else {
            // Whole transfer retains CAN_MERGE, so the appended bytes fault
            // inside the same unit and none of that unit is consumed.
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EFAULT, errno);
            std::vector<unsigned char> expected = payload;
            expected.insert(expected.end(), tail.begin(), tail.end());
            ASSERT_NO_FATAL_FAILURE(ExpectRemaining(destination.fds[0], expected));
        }
    }
}

TEST(ReadFaultSemantics, PacketPipeShortReadDiscardsOnlyCurrentPacketTail) {
    const std::vector<unsigned char> first(128, 0x2b);
    const std::vector<unsigned char> second(128, 0x7d);
    for (bool vectored : {false, true}) {
        SCOPED_TRACE(vectored);
        Pipe pipe(O_DIRECT);
        ASSERT_TRUE(pipe.valid) << strerror(errno);
        ASSERT_EQ(128, write(pipe.fds[1], first.data(), first.size()));
        ASSERT_EQ(128, write(pipe.fds[1], second.data(), second.size()));
        unsigned char output[64] = {};
        ASSERT_EQ(64, ReadVariant(pipe.fds[0], output, sizeof(output), vectored));
        EXPECT_EQ(0, memcmp(output, first.data(), sizeof(output)));
        ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], second));
    }
}

TEST(ReadFaultSemantics, PacketPipeCopyFaultPreservesCurrentPacket) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    const std::vector<unsigned char> payload(128, 0x35);
    for (bool vectored : {false, true}) {
        SCOPED_TRACE(vectored);
        Pipe pipe(O_DIRECT);
        ASSERT_TRUE(pipe.valid) << strerror(errno);
        ASSERT_EQ(128, write(pipe.fds[1], payload.data(), payload.size()));
        errno = 0;
        EXPECT_EQ(-1, ReadVariant(pipe.fds[0], mapping.bytes + mapping.page - 64,
                                 payload.size(), vectored));
        EXPECT_EQ(EFAULT, errno);
        ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], payload));
    }
}

TEST(ReadFaultSemantics, PipePartiallyReadSlotStaysFullUntilBufferIsConsumed) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    Pipe pipe;
    ASSERT_TRUE(pipe.valid);
    ASSERT_EQ(static_cast<int>(mapping.page * 2),
              fcntl(pipe.fds[0], F_SETPIPE_SZ, mapping.page * 2));
    const std::vector<unsigned char> payload(mapping.page * 2, 0x2a);
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(pipe.fds[1], payload.data(), payload.size()));
    std::vector<unsigned char> output(mapping.page);
    ASSERT_EQ(100, read(pipe.fds[0], output.data(), 100));
    pollfd writable = {pipe.fds[1], POLLOUT, 0};
    EXPECT_EQ(0, poll(&writable, 1, 0));
    unsigned char byte = 0x33;
    errno = 0;
    EXPECT_EQ(-1, write(pipe.fds[1], &byte, 1));
    EXPECT_EQ(EAGAIN, errno);
    ASSERT_EQ(static_cast<ssize_t>(mapping.page - 100),
              read(pipe.fds[0], output.data(), mapping.page - 100));
    EXPECT_EQ(1, poll(&writable, 1, 0));
    EXPECT_NE(0, writable.revents & POLLOUT);
    ASSERT_EQ(1, write(pipe.fds[1], &byte, 1));
    std::vector<unsigned char> remaining(mapping.page, 0x2a);
    remaining.push_back(byte);
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], remaining));
}

TEST(ReadFaultSemantics, PipeBufferProvenanceIsIndependentOfByteRingPosition) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    Pipe pipe;
    ASSERT_TRUE(pipe.valid);
    ASSERT_EQ(static_cast<int>(mapping.page), fcntl(pipe.fds[0], F_SETPIPE_SZ, mapping.page));
    std::vector<unsigned char> old(100, 0x18);
    ASSERT_EQ(100, write(pipe.fds[1], old.data(), old.size()));
    ASSERT_EQ(100, read(pipe.fds[0], old.data(), old.size()));
    const std::vector<unsigned char> payload(mapping.page, 0x63);
    ASSERT_EQ(static_cast<ssize_t>(mapping.page),
              write(pipe.fds[1], payload.data(), payload.size()));
    // A new full logical buffer starts at offset zero despite the byte ring
    // being positioned 100 bytes into its allocation by the previous I/O.
    errno = 0;
    EXPECT_EQ(-1, read(pipe.fds[0], mapping.bytes + 100, mapping.page));
    EXPECT_EQ(EFAULT, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(pipe.fds[0], payload));
}

TEST(ReadFaultSemantics, EventfdFaultConsumesWholeCounter) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    for (bool vectored : {false, true}) {
        int fd = eventfd(7, EFD_NONBLOCK | EFD_CLOEXEC);
        ASSERT_GE(fd, 0);
        errno = 0;
        EXPECT_EQ(-1, ReadVariant(fd, mapping.bytes + mapping.page - 4, 8, vectored));
        EXPECT_EQ(EFAULT, errno);
        uint64_t remaining = 0;
        errno = 0;
        EXPECT_EQ(-1, read(fd, &remaining, sizeof(remaining)));
        EXPECT_EQ(EAGAIN, errno);
        EXPECT_EQ(0, close(fd));
    }
}

class SignalFaultSemantics : public ::testing::Test {
  protected:
    void SetUp() override {
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, SIGUSR1);
        ASSERT_EQ(0, sigprocmask(SIG_BLOCK, &mask, &old_mask));
        blocked = true;
        fd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
        ASSERT_GE(fd, 0);
    }
    void TearDown() override {
        if (fd >= 0) {
            signalfd_siginfo record = {};
            while (read(fd, &record, sizeof(record)) == sizeof(record)) {}
            close(fd);
        }
        if (blocked) sigprocmask(SIG_SETMASK, &old_mask, nullptr);
    }
    int fd = -1;
    bool blocked = false;
    sigset_t old_mask = {};
};

TEST_F(SignalFaultSemantics, CopyFaultConsumesSignalWithoutReturningPartialRecord) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    ASSERT_EQ(0, kill(getpid(), SIGUSR1));
    errno = 0;
    EXPECT_EQ(-1, read(fd, mapping.bytes + mapping.page - 64, sizeof(signalfd_siginfo)));
    EXPECT_EQ(EFAULT, errno);
    signalfd_siginfo record = {};
    errno = 0;
    EXPECT_EQ(-1, read(fd, &record, sizeof(record)));
    EXPECT_EQ(EAGAIN, errno);
}

volatile sig_atomic_t tty_alarm_fired = 0;
void TtyTimeout(int) { tty_alarm_fired = 1; }

class Pty {
  public:
    Pty() { valid = openpty(&master, &slave, nullptr, nullptr, nullptr) == 0; }
    ~Pty() {
        if (master >= 0) close(master);
        if (slave >= 0) close(slave);
    }
    int master = -1;
    int slave = -1;
    bool valid;
};

TEST(ReadFaultSemantics, TtyReadvReturnsAvailablePrefixWithoutWaitingOnNextIovec) {
    FaultMapping mapping;
    ASSERT_TRUE(mapping.valid);
    Pty pty;
    ASSERT_TRUE(pty.valid) << strerror(errno);
    termios attributes = {};
    ASSERT_EQ(0, tcgetattr(pty.slave, &attributes));
    cfmakeraw(&attributes);
    attributes.c_cc[VMIN] = 1;
    attributes.c_cc[VTIME] = 0;
    ASSERT_EQ(0, tcsetattr(pty.slave, TCSANOW, &attributes));
    const unsigned char payload[] = {0x41, 0x42, 0x43, 0x44};
    ASSERT_EQ(4, write(pty.master, payload, sizeof(payload)));
    pollfd readable = {pty.slave, POLLIN, 0};
    ASSERT_EQ(1, poll(&readable, 1, 2000));
    unsigned char output[4] = {};
    iovec iovs[] = {{output, sizeof(output)}, {mapping.bytes + mapping.page, 4}};
    struct sigaction timeout = {};
    struct sigaction previous = {};
    timeout.sa_handler = TtyTimeout;
    sigemptyset(&timeout.sa_mask);
    // No SA_RESTART: a regression must finish and fail, rather than hang CI.
    ASSERT_EQ(0, sigaction(SIGALRM, &timeout, &previous));
    tty_alarm_fired = 0;
    alarm(2);
    const ssize_t result = readv(pty.slave, iovs, 2);
    const int read_error = errno;
    alarm(0);
    EXPECT_EQ(0, sigaction(SIGALRM, &previous, nullptr));
    EXPECT_EQ(4, result) << strerror(read_error);
    EXPECT_FALSE(tty_alarm_fired) << "readv waited for the empty second iovec";
    EXPECT_EQ(0, memcmp(output, payload, sizeof(payload)));
}

TEST(ReadFaultSemantics, EventfdReadvUsesAggregateLength) {
    int fd = eventfd(7, EFD_NONBLOCK | EFD_CLOEXEC);
    ASSERT_GE(fd, 0);
    uint64_t output = 0;
    iovec iovs[] = {{&output, 4}, {reinterpret_cast<unsigned char*>(&output) + 4, 4}};
    EXPECT_EQ(8, readv(fd, iovs, 2));
    EXPECT_EQ(7U, output);
    errno = 0;
    EXPECT_EQ(-1, read(fd, &output, sizeof(output)));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, close(fd));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
