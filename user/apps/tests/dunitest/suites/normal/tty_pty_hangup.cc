#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <pty.h>
#include <signal.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <stdint.h>
#include <termios.h>
#include <unistd.h>

#include <algorithm>
#include <string>
#include <vector>

namespace {

#ifndef TIOCPKT
constexpr int kTiocpkt = 0x5420;
#else
constexpr int kTiocpkt = TIOCPKT;
#endif

#ifndef TIOCGPTN
constexpr int kTiocgptn = 0x80045430;
#else
constexpr int kTiocgptn = TIOCGPTN;
#endif

#ifndef TIOCSPTLCK
constexpr int kTiocsptlck = 0x40045431;
#else
constexpr int kTiocsptlck = TIOCSPTLCK;
#endif

#ifndef TIOCGPTPEER
constexpr int kTiocgptpeer = 0x5441;
#else
constexpr int kTiocgptpeer = TIOCGPTPEER;
#endif

#ifndef O_PATH
#define O_PATH 010000000
#endif

#ifndef TIOCPKT_FLUSHWRITE
constexpr unsigned char kTiocpktFlushWrite = 2;
#else
constexpr unsigned char kTiocpktFlushWrite = TIOCPKT_FLUSHWRITE;
#endif

class UniqueFd {
public:
    UniqueFd() = default;
    explicit UniqueFd(int fd) : fd_(fd) {}
    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;

    UniqueFd(UniqueFd&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }

    UniqueFd& operator=(UniqueFd&& other) noexcept {
        if (this != &other) {
            reset();
            fd_ = other.fd_;
            other.fd_ = -1;
        }
        return *this;
    }

    ~UniqueFd() { reset(); }

    int get() const { return fd_; }

    int release() {
        int fd = fd_;
        fd_ = -1;
        return fd;
    }

    void reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

private:
    int fd_ = -1;
};

class ScopedSignalIgnore {
public:
    explicit ScopedSignalIgnore(int signum) : signum_(signum) {
        struct sigaction action = {};
        action.sa_handler = SIG_IGN;
        sigemptyset(&action.sa_mask);
        valid_ = sigaction(signum_, &action, &old_) == 0;
    }

    ScopedSignalIgnore(const ScopedSignalIgnore&) = delete;
    ScopedSignalIgnore& operator=(const ScopedSignalIgnore&) = delete;

    ~ScopedSignalIgnore() {
        if (valid_) {
            sigaction(signum_, &old_, nullptr);
        }
    }

    bool valid() const { return valid_; }

private:
    int signum_;
    struct sigaction old_ = {};
    bool valid_ = false;
};

struct PtyPair {
    UniqueFd master;
    UniqueFd slave;
};

PtyPair OpenRawPty(char* name = nullptr) {
    int master = -1;
    int slave = -1;
    if (openpty(&master, &slave, name, nullptr, nullptr) < 0) {
        ADD_FAILURE() << "openpty failed: errno=" << errno << " (" << strerror(errno) << ")";
        return {};
    }

    PtyPair pair{UniqueFd(master), UniqueFd(slave)};

    struct termios term = {};
    if (tcgetattr(pair.slave.get(), &term) < 0) {
        ADD_FAILURE() << "tcgetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
        return pair;
    }

    term.c_iflag = 0;
    term.c_oflag = 0;
    term.c_lflag = 0;
    term.c_cflag |= CS8;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;

    if (tcsetattr(pair.slave.get(), TCSANOW, &term) < 0) {
        ADD_FAILURE() << "tcsetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
    }

    return pair;
}

PtyPair OpenOpostPty() {
    int master = -1;
    int slave = -1;
    if (openpty(&master, &slave, nullptr, nullptr, nullptr) < 0) {
        ADD_FAILURE() << "openpty failed: errno=" << errno << " (" << strerror(errno) << ")";
        return {};
    }

    PtyPair pair{UniqueFd(master), UniqueFd(slave)};

    struct termios term = {};
    if (tcgetattr(pair.slave.get(), &term) < 0) {
        ADD_FAILURE() << "tcgetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
        return pair;
    }

    term.c_iflag = 0;
    term.c_oflag = OPOST | ONLCR;
    term.c_lflag = 0;
    term.c_cflag |= CS8;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;

    if (tcsetattr(pair.slave.get(), TCSANOW, &term) < 0) {
        ADD_FAILURE() << "tcsetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
    }

    return pair;
}

PtyPair OpenCanonicalNoEchoPty() {
    int master = -1;
    int slave = -1;
    if (openpty(&master, &slave, nullptr, nullptr, nullptr) < 0) {
        ADD_FAILURE() << "openpty failed: errno=" << errno << " (" << strerror(errno) << ")";
        return {};
    }

    PtyPair pair{UniqueFd(master), UniqueFd(slave)};

    struct termios term = {};
    if (tcgetattr(pair.slave.get(), &term) < 0) {
        ADD_FAILURE() << "tcgetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
        return pair;
    }

    term.c_lflag |= ICANON;
    term.c_lflag &= ~ECHO;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;

    if (tcsetattr(pair.slave.get(), TCSANOW, &term) < 0) {
        ADD_FAILURE() << "tcsetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
    }

    return pair;
}

void SetNonblock(int fd) {
    int flags = fcntl(fd, F_GETFL);
    ASSERT_GE(flags, 0) << "fcntl(F_GETFL) failed: errno=" << errno << " (" << strerror(errno)
                        << ")";
    ASSERT_EQ(0, fcntl(fd, F_SETFL, flags | O_NONBLOCK))
        << "fcntl(F_SETFL, O_NONBLOCK) failed: errno=" << errno << " (" << strerror(errno)
        << ")";
}

short PollEvents(int fd) {
    struct pollfd pfd = {
        .fd = fd,
        .events = POLLIN | POLLOUT | POLLERR | POLLHUP,
        .revents = 0,
    };
    int ret = poll(&pfd, 1, 0);
    EXPECT_GE(ret, 0) << "poll failed: errno=" << errno << " (" << strerror(errno) << ")";
    return pfd.revents;
}

void ExpectReadErrno(int fd, int expected_errno) {
    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(fd, &ch, 1));
    EXPECT_EQ(expected_errno, errno) << "unexpected read errno=" << errno << " ("
                                    << strerror(errno) << ")";
}

bool IsWouldBlock(int err) {
    return err == EAGAIN
#if EWOULDBLOCK != EAGAIN
           || err == EWOULDBLOCK
#endif
        ;
}

unsigned char ConfigureSignalFlushSlave(int fd, bool noflsh) {
    struct termios term = {};
    if (tcgetattr(fd, &term) < 0) {
        ADD_FAILURE() << "tcgetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
        return 0;
    }

    term.c_lflag |= ICANON | ISIG;
    term.c_lflag &= ~ECHO;
    if (noflsh) {
        term.c_lflag |= NOFLSH;
    } else {
        term.c_lflag &= ~NOFLSH;
    }
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;

    if (tcsetattr(fd, TCSANOW, &term) < 0) {
        ADD_FAILURE() << "tcsetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
        return 0;
    }

    return static_cast<unsigned char>(term.c_cc[VINTR]);
}

std::string ReadCanonicalLine(int fd) {
    struct pollfd pfd = {
        .fd = fd,
        .events = POLLIN | POLLHUP | POLLERR,
        .revents = 0,
    };

    int ret = 0;
    do {
        ret = poll(&pfd, 1, 5000);
    } while (ret < 0 && errno == EINTR);

    if (ret <= 0) {
        ADD_FAILURE() << "poll waiting for canonical line failed: errno="
                      << (ret < 0 ? errno : ETIMEDOUT) << " ("
                      << strerror(ret < 0 ? errno : ETIMEDOUT) << ")";
        return {};
    }
    if ((pfd.revents & POLLIN) == 0) {
        ADD_FAILURE() << "poll returned without POLLIN, revents=" << pfd.revents;
        return {};
    }

    char buf[512] = {};
    ssize_t n = 0;
    do {
        n = read(fd, buf, sizeof(buf));
    } while (n < 0 && errno == EINTR);

    if (n <= 0) {
        ADD_FAILURE() << "read canonical line failed: errno=" << (n < 0 ? errno : EIO)
                      << " (" << strerror(n < 0 ? errno : EIO) << ")";
        return {};
    }

    return std::string(buf, buf + n);
}

std::string ReadExpectedBytesWithPoll(int fd, size_t expected, size_t max_chunk) {
    std::string output(expected, '\0');
    size_t total = 0;
    while (total < expected) {
        struct pollfd pfd = {
            .fd = fd,
            .events = POLLIN | POLLHUP | POLLERR,
            .revents = 0,
        };
        int ret = 0;
        do {
            ret = poll(&pfd, 1, 5000);
        } while (ret < 0 && errno == EINTR);

        if (ret <= 0) {
            ADD_FAILURE() << "poll waiting for canonical data failed: errno="
                          << (ret < 0 ? errno : ETIMEDOUT) << " ("
                          << strerror(ret < 0 ? errno : ETIMEDOUT) << "), total=" << total
                          << ", expected=" << expected;
            output.resize(total);
            return output;
        }
        if ((pfd.revents & POLLIN) == 0) {
            ADD_FAILURE() << "poll returned without POLLIN, revents=" << pfd.revents
                          << ", total=" << total << ", expected=" << expected;
            output.resize(total);
            return output;
        }

        const size_t chunk = std::min(max_chunk, expected - total);
        ssize_t n = 0;
        do {
            n = read(fd, output.data() + total, chunk);
        } while (n < 0 && errno == EINTR);

        if (n <= 0) {
            ADD_FAILURE() << "read canonical data failed: errno=" << (n < 0 ? errno : EIO)
                          << " (" << strerror(n < 0 ? errno : EIO) << "), total=" << total
                          << ", expected=" << expected;
            output.resize(total);
            return output;
        }
        total += static_cast<size_t>(n);
    }

    return output;
}

struct ConcurrentSlaveOpenArgs {
    const char* slave_name;
    int start_read_fd;
    int opened_fd;
    int open_errno;
};

struct WriteAllArgs {
    int fd;
    const char* data;
    size_t len;
    size_t written;
    int error;
};

void* WriteAll(void* raw) {
    auto* args = static_cast<WriteAllArgs*>(raw);
    args->written = 0;
    args->error = 0;

    while (args->written < args->len) {
        ssize_t n = write(args->fd, args->data + args->written, args->len - args->written);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0) {
            args->error = errno;
            return nullptr;
        }
        if (n == 0) {
            args->error = EIO;
            return nullptr;
        }
        args->written += static_cast<size_t>(n);
    }

    return nullptr;
}

void* ConcurrentSlaveOpen(void* raw) {
    auto* args = static_cast<ConcurrentSlaveOpenArgs*>(raw);
    char token = 0;
    ssize_t n = read(args->start_read_fd, &token, 1);
    close(args->start_read_fd);
    if (n != 1) {
        args->opened_fd = -1;
        args->open_errno = errno == 0 ? EIO : errno;
        return nullptr;
    }

    errno = 0;
    args->opened_fd = open(args->slave_name, O_RDWR | O_NOCTTY);
    args->open_errno = args->opened_fd >= 0 ? 0 : errno;
    return nullptr;
}

bool WaitForChild(pid_t child, int* status, int rounds = 300) {
    for (int i = 0; i < rounds; ++i) {
        pid_t ret = waitpid(child, status, WNOHANG);
        if (ret == child) {
            return true;
        }
        if (ret < 0 && errno != EINTR) {
            return false;
        }
        usleep(10 * 1000);
    }
    return false;
}

TEST(TtyPtyHangup, MasterReadAfterSlaveCloseReturnsEio) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pair.slave.reset();

    ExpectReadErrno(pair.master.get(), EIO);
}

TEST(TtyPtyHangup, CanonicalReaderDoesNotMissLineWakeup) {
    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    int ready_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        pair.master.reset();

        for (int i = 0; i < 200; ++i) {
            char ready = 'r';
            if (write(ready_pipe[1], &ready, 1) != 1) {
                _exit(2);
            }

            char buf[32] = {};
            ssize_t n = read(pair.slave.get(), buf, sizeof(buf));
            if (n != 5 || memcmp(buf, "line\n", 5) != 0) {
                _exit(3);
            }
        }
        _exit(0);
    }

    close(ready_pipe[1]);
    pair.slave.reset();

    for (int i = 0; i < 200; ++i) {
        char ready = 0;
        ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
        ASSERT_EQ('r', ready);
        ASSERT_EQ(5, write(pair.master.get(), "line\n", 5)) << strerror(errno);
    }
    close(ready_pipe[0]);

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "canonical pty reader did not consume all lines";
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(TtyPtyHangup, SignalFlushDiscardsPendingInputBacklog) {
    ScopedSignalIgnore ignore_sigint(SIGINT);
    ASSERT_TRUE(ignore_sigint.valid()) << "sigaction(SIGINT, SIG_IGN) failed: errno=" << errno
                                       << " (" << strerror(errno) << ")";

    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    unsigned char vintr = ConfigureSignalFlushSlave(pair.slave.get(), false);
    ASSERT_NE(0, vintr) << "VINTR must be enabled for this regression test";

    constexpr size_t kDragonOsPtyDrainChunk = 256;
    std::string stale(kDragonOsPtyDrainChunk - 1, 'a');
    stale.push_back(static_cast<char>(vintr));
    stale.append("stale-backlog-should-be-flushed\n");
    ASSERT_LT(stale.size(), static_cast<size_t>(16 * 1024));

    ASSERT_EQ(static_cast<ssize_t>(stale.size()), write(pair.master.get(), stale.data(), stale.size()))
        << "single stale write failed: errno=" << errno << " (" << strerror(errno) << ")";

    const std::string fresh = "fresh-after-signal\n";
    ASSERT_EQ(static_cast<ssize_t>(fresh.size()), write(pair.master.get(), fresh.data(), fresh.size()))
        << "fresh marker write failed: errno=" << errno << " (" << strerror(errno) << ")";

    EXPECT_EQ(fresh, ReadCanonicalLine(pair.slave.get()));
}

TEST(TtyPtyHangup, SignalNoflshPreservesPendingInputBacklog) {
    ScopedSignalIgnore ignore_sigint(SIGINT);
    ASSERT_TRUE(ignore_sigint.valid()) << "sigaction(SIGINT, SIG_IGN) failed: errno=" << errno
                                       << " (" << strerror(errno) << ")";

    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    unsigned char vintr = ConfigureSignalFlushSlave(pair.slave.get(), true);
    ASSERT_NE(0, vintr) << "VINTR must be enabled for this regression test";

    constexpr size_t kDragonOsPtyDrainChunk = 256;
    std::string expected(kDragonOsPtyDrainChunk - 1, 'a');
    expected.append("stale-backlog-must-survive\n");

    std::string input(kDragonOsPtyDrainChunk - 1, 'a');
    input.push_back(static_cast<char>(vintr));
    input.append("stale-backlog-must-survive\n");
    ASSERT_LT(input.size(), static_cast<size_t>(16 * 1024));

    ASSERT_EQ(static_cast<ssize_t>(input.size()), write(pair.master.get(), input.data(), input.size()))
        << "single NOFLSH write failed: errno=" << errno << " (" << strerror(errno) << ")";

    const std::string fresh = "fresh-after-noflsh\n";
    ASSERT_EQ(static_cast<ssize_t>(fresh.size()), write(pair.master.get(), fresh.data(), fresh.size()))
        << "fresh marker write failed: errno=" << errno << " (" << strerror(errno) << ")";

    EXPECT_EQ(expected, ReadCanonicalLine(pair.slave.get()));
    EXPECT_EQ(fresh, ReadCanonicalLine(pair.slave.get()));
}

TEST(TtyPtyHangup, MasterPollAfterSlaveCloseReportsHupAndOut) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pair.slave.reset();

    short revents = PollEvents(pair.master.get());
    EXPECT_NE(0, revents & POLLHUP);
    EXPECT_NE(0, revents & POLLOUT);
}

TEST(TtyPtyHangup, MasterDrainsBufferedDataBeforeEio) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    ASSERT_EQ(3, write(pair.slave.get(), "abc", 3))
        << "write slave failed: errno=" << errno << " (" << strerror(errno) << ")";
    pair.slave.reset();

    char buf[4] = {};
    ASSERT_EQ(3, read(pair.master.get(), buf, 3))
        << "read buffered data failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_STREQ("abc", buf);
    ExpectReadErrno(pair.master.get(), EIO);
}

TEST(TtyPtyHangup, MasterWriteAfterSlaveCloseSucceeds) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pair.slave.reset();

    EXPECT_EQ(1, write(pair.master.get(), "x", 1))
        << "master write after slave close failed: errno=" << errno << " (" << strerror(errno)
        << ")";
}

TEST(TtyPtyHangup, NonblockEmptyMasterReadStillEagainBeforeHangup) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    SetNonblock(pair.master.get());

    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(pair.master.get(), &ch, 1));
    EXPECT_TRUE(IsWouldBlock(errno)) << "empty nonblocking pty master read errno=" << errno << " ("
                                    << strerror(errno) << ")";
}

TEST(TtyPtyHangup, SlaveCanReopenWhileMasterAliveAfterLastSlaveClose) {
    char slave_name[128] = {};
    PtyPair pair = OpenRawPty(slave_name);
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    ASSERT_NE('\0', slave_name[0]);

    pair.slave.reset();
    ExpectReadErrno(pair.master.get(), EIO);

    UniqueFd reopened(open(slave_name, O_RDWR | O_NOCTTY));
    ASSERT_GE(reopened.get(), 0) << "reopen(" << slave_name << ") failed: errno=" << errno
                                 << " (" << strerror(errno) << ")";

    SetNonblock(pair.master.get());
    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(pair.master.get(), &ch, 1));
    EXPECT_TRUE(IsWouldBlock(errno)) << "master should stop reporting hangup after slave reopen,"
                                    << " errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(1, write(reopened.get(), "r", 1))
        << "write reopened slave failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(1, read(pair.master.get(), &ch, 1))
        << "master read from reopened slave failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    EXPECT_EQ('r', ch);
}

TEST(TtyPtyHangup, ReopenedSlaveKeepsIndexReservedAfterMasterClose) {
    char first_name[128] = {};
    PtyPair pair = OpenRawPty(first_name);
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    ASSERT_NE('\0', first_name[0]);

    pair.slave.reset();

    UniqueFd reopened(open(first_name, O_RDWR | O_NOCTTY));
    ASSERT_GE(reopened.get(), 0) << "reopen(" << first_name << ") failed: errno=" << errno
                                 << " (" << strerror(errno) << ")";

    pair.master.reset();

    char second_name[128] = {};
    PtyPair second = OpenRawPty(second_name);
    ASSERT_GE(second.master.get(), 0);
    ASSERT_GE(second.slave.get(), 0);
    ASSERT_NE('\0', second_name[0]);
    EXPECT_NE(0, strcmp(first_name, second_name))
        << "pty index was reused while a reopened slave fd was still alive";
}

TEST(TtyPtyHangup, ConcurrentSlaveOpenAndMasterCloseNeverReusesLiveIndex) {
    constexpr int kIterations = 64;
    int reopened_success = 0;
    int eio_failures = 0;
    int enoent_failures = 0;
    int other_failures = 0;

    for (int i = 0; i < kIterations; ++i) {
        SCOPED_TRACE(i);
        char first_name[128] = {};
        PtyPair pair = OpenRawPty(first_name);
        ASSERT_GE(pair.master.get(), 0);
        ASSERT_GE(pair.slave.get(), 0);
        ASSERT_NE('\0', first_name[0]);

        pair.slave.reset();

        int start_pipe[2] = {-1, -1};
        ASSERT_EQ(0, pipe(start_pipe)) << "pipe failed: errno=" << errno << " ("
                                       << strerror(errno) << ")";
        UniqueFd write_end(start_pipe[1]);

        ConcurrentSlaveOpenArgs args = {
            .slave_name = first_name,
            .start_read_fd = start_pipe[0],
            .opened_fd = -1,
            .open_errno = 0,
        };

        pthread_t thread = {};
        ASSERT_EQ(0, pthread_create(&thread, nullptr, ConcurrentSlaveOpen, &args))
            << "pthread_create failed";

        ASSERT_EQ(1, write(write_end.get(), "x", 1))
            << "failed to release slave opener: errno=" << errno << " (" << strerror(errno)
            << ")";
        write_end.reset();
        pair.master.reset();

        ASSERT_EQ(0, pthread_join(thread, nullptr)) << "pthread_join failed";

        if (args.opened_fd >= 0) {
            ++reopened_success;
            UniqueFd reopened(args.opened_fd);

            char second_name[128] = {};
            PtyPair second = OpenRawPty(second_name);
            ASSERT_GE(second.master.get(), 0);
            ASSERT_GE(second.slave.get(), 0);
            ASSERT_NE('\0', second_name[0]);
            EXPECT_NE(0, strcmp(first_name, second_name))
                << "pty index was reused while concurrent reopened slave fd was still alive";
            continue;
        }

        if (args.open_errno == EIO) {
            ++eio_failures;
        } else if (args.open_errno == ENOENT) {
            ++enoent_failures;
        } else {
            ++other_failures;
            ADD_FAILURE() << "unexpected concurrent slave open errno=" << args.open_errno << " ("
                          << strerror(args.open_errno) << ")";
        }
    }

    EXPECT_EQ(0, other_failures)
        << "success=" << reopened_success << " eio=" << eio_failures
        << " enoent=" << enoent_failures;
}

TEST(TtyPtyHangup, MasterThenSlaveCloseAllowsCleanIndexReuse) {
    char first_name[128] = {};
    PtyPair first = OpenRawPty(first_name);
    ASSERT_GE(first.master.get(), 0);
    ASSERT_GE(first.slave.get(), 0);
    ASSERT_NE('\0', first_name[0]);

    first.master.reset();
    first.slave.reset();

    char second_name[128] = {};
    PtyPair second = OpenRawPty(second_name);
    ASSERT_GE(second.master.get(), 0);
    ASSERT_GE(second.slave.get(), 0);
    ASSERT_STREQ(first_name, second_name)
        << "test expects the freed pty index to be immediately reusable";

    ASSERT_EQ(1, write(second.slave.get(), "z", 1))
        << "write to reused slave failed: errno=" << errno << " (" << strerror(errno) << ")";
    char ch = 0;
    ASSERT_EQ(1, read(second.master.get(), &ch, 1))
        << "read from reused master failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ('z', ch);
}

TEST(TtyPtyHangup, MasterOnlyCloseReleasesDevptsIndex) {
    constexpr uint32_t kPtyMax = 128;
    constexpr int kIterations = 160;
    bool seen[kPtyMax] = {};
    bool saw_reuse = false;

    for (int i = 0; i < kIterations; ++i) {
        UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
        ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed at iteration " << i
                                   << ": errno=" << errno << " (" << strerror(errno) << ")";

        uint32_t index = UINT32_MAX;
        ASSERT_EQ(0, ioctl(master.get(), kTiocgptn, &index))
            << "ioctl(TIOCGPTN) failed at iteration " << i << ": errno=" << errno << " ("
            << strerror(errno) << ")";

        ASSERT_LT(index, kPtyMax) << "unexpected PTY index " << index;
        if (seen[index]) {
            saw_reuse = true;
        }
        seen[index] = true;
    }

    EXPECT_TRUE(saw_reuse) << "master-only open/close did not visibly reuse any devpts index";
}

TEST(TtyPtyHangup, OPathFifoKeepsPathInodeAndDoesNotBlock) {
    std::string path = "/tmp/opath_fifo_" + std::to_string(getpid());
    unlink(path.c_str());

    ASSERT_EQ(0, mkfifo(path.c_str(), 0600))
        << "mkfifo(" << path << ") failed: errno=" << errno << " (" << strerror(errno) << ")";

    struct stat path_stat = {};
    ASSERT_EQ(0, lstat(path.c_str(), &path_stat))
        << "lstat(" << path << ") failed: errno=" << errno << " (" << strerror(errno) << ")";

    UniqueFd fifo(open(path.c_str(), O_PATH | O_NONBLOCK | O_CLOEXEC));
    int saved_errno = errno;
    unlink(path.c_str());
    ASSERT_GE(fifo.get(), 0) << "open(O_PATH) on FIFO failed or blocked: errno=" << saved_errno
                             << " (" << strerror(saved_errno) << ")";

    struct stat fd_stat = {};
    ASSERT_EQ(0, fstat(fifo.get(), &fd_stat))
        << "fstat(O_PATH FIFO fd) failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(path_stat.st_dev, fd_stat.st_dev);
    EXPECT_EQ(path_stat.st_ino, fd_stat.st_ino);
    EXPECT_EQ(path_stat.st_mode & S_IFMT, fd_stat.st_mode & S_IFMT);
}

TEST(TtyPtyHangup, TiocgptpeerFailsWhileSlaveLocked) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    EXPECT_EQ(EIO, errno) << "locked TIOCGPTPEER should fail with EIO, got errno=" << errno
                          << " (" << strerror(errno) << ")";
}

TEST(TtyPtyHangup, TiocgptpeerOPathSucceedsWhileSlaveLocked) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    UniqueFd peer(ioctl(master.get(), kTiocgptpeer, O_PATH | O_CLOEXEC));
    ASSERT_GE(peer.get(), 0) << "locked TIOCGPTPEER(O_PATH) failed: errno=" << errno
                             << " (" << strerror(errno) << ")";

    int fd_flags = fcntl(peer.get(), F_GETFD);
    ASSERT_GE(fd_flags, 0) << "fcntl(F_GETFD) failed: errno=" << errno << " (" << strerror(errno)
                           << ")";
    EXPECT_NE(0, fd_flags & FD_CLOEXEC);
}

TEST(TtyPtyHangup, TiocgptpeerOpensUnlockedSlave) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    int unlock = 0;
    ASSERT_EQ(0, ioctl(master.get(), kTiocsptlck, &unlock))
        << "unlock slave failed: errno=" << errno << " (" << strerror(errno) << ")";

    UniqueFd slave(ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY | O_CLOEXEC));
    ASSERT_GE(slave.get(), 0) << "TIOCGPTPEER failed: errno=" << errno << " ("
                              << strerror(errno) << ")";

    int fd_flags = fcntl(slave.get(), F_GETFD);
    ASSERT_GE(fd_flags, 0) << "fcntl(F_GETFD) failed: errno=" << errno << " (" << strerror(errno)
                           << ")";
    EXPECT_NE(0, fd_flags & FD_CLOEXEC);

    struct termios term = {};
    ASSERT_EQ(0, tcgetattr(slave.get(), &term))
        << "tcgetattr(peer slave) failed: errno=" << errno << " (" << strerror(errno) << ")";
    term.c_iflag = 0;
    term.c_oflag = 0;
    term.c_lflag = 0;
    term.c_cflag |= CS8;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;
    ASSERT_EQ(0, tcsetattr(slave.get(), TCSANOW, &term))
        << "tcsetattr(peer slave) failed: errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(1, write(slave.get(), "q", 1))
        << "write(peer slave) failed: errno=" << errno << " (" << strerror(errno) << ")";
    char ch = 0;
    ASSERT_EQ(1, read(master.get(), &ch, 1))
        << "read(master) failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ('q', ch);
}

TEST(TtyPtyHangup, TiocgptpeerOPathRejectsTtyOperations) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    UniqueFd peer(ioctl(master.get(), kTiocgptpeer, O_PATH | O_CLOEXEC));
    ASSERT_GE(peer.get(), 0) << "TIOCGPTPEER(O_PATH) failed: errno=" << errno << " ("
                             << strerror(errno) << ")";

    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(peer.get(), &ch, 1));
    EXPECT_EQ(EBADF, errno) << "read on O_PATH peer should fail with EBADF, got errno=" << errno
                            << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, write(peer.get(), "x", 1));
    EXPECT_EQ(EBADF, errno) << "write on O_PATH peer should fail with EBADF, got errno=" << errno
                            << " (" << strerror(errno) << ")";

    struct termios term = {};
    errno = 0;
    EXPECT_EQ(-1, tcgetattr(peer.get(), &term));
    EXPECT_EQ(EBADF, errno) << "tcgetattr on O_PATH peer should fail with EBADF, got errno="
                            << errno << " (" << strerror(errno) << ")";

    uint32_t index = UINT32_MAX;
    errno = 0;
    EXPECT_EQ(-1, ioctl(peer.get(), kTiocgptn, &index));
    EXPECT_EQ(EBADF, errno) << "ioctl(TIOCGPTN) on O_PATH peer should fail with EBADF, got errno="
                            << errno << " (" << strerror(errno) << ")";
}

TEST(TtyPtyHangup, TiocgptpeerOPathCloseDoesNotAffectRealPeerOpen) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    int unlock = 0;
    ASSERT_EQ(0, ioctl(master.get(), kTiocsptlck, &unlock))
        << "unlock slave failed: errno=" << errno << " (" << strerror(errno) << ")";

    UniqueFd path_peer(ioctl(master.get(), kTiocgptpeer, O_PATH | O_CLOEXEC));
    ASSERT_GE(path_peer.get(), 0) << "TIOCGPTPEER(O_PATH) failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";
    path_peer.reset();

    UniqueFd slave(ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    ASSERT_GE(slave.get(), 0) << "TIOCGPTPEER real peer failed after closing O_PATH peer: errno="
                              << errno << " (" << strerror(errno) << ")";

    struct termios term = {};
    ASSERT_EQ(0, tcgetattr(slave.get(), &term))
        << "tcgetattr(real peer slave) failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    term.c_iflag = 0;
    term.c_oflag = 0;
    term.c_lflag = 0;
    term.c_cflag |= CS8;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;
    ASSERT_EQ(0, tcsetattr(slave.get(), TCSANOW, &term))
        << "tcsetattr(real peer slave) failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    ASSERT_EQ(1, write(slave.get(), "p", 1))
        << "write(real peer slave) failed: errno=" << errno << " (" << strerror(errno) << ")";
    char ch = 0;
    ASSERT_EQ(1, read(master.get(), &ch, 1))
        << "read(master) failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ('p', ch);
}

TEST(TtyPtyHangup, TiocgptpeerOPathDoesNotKeepIndexReserved) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    uint32_t first_index = UINT32_MAX;
    ASSERT_EQ(0, ioctl(master.get(), kTiocgptn, &first_index))
        << "TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";

    UniqueFd peer(ioctl(master.get(), kTiocgptpeer, O_PATH | O_CLOEXEC));
    ASSERT_GE(peer.get(), 0) << "TIOCGPTPEER(O_PATH) failed: errno=" << errno << " ("
                             << strerror(errno) << ")";

    master.reset();

    bool saw_reused_first_index = false;
    std::vector<UniqueFd> masters;
    for (uint32_t i = 0; i < 128; ++i) {
        UniqueFd next(open("/dev/ptmx", O_RDWR | O_NOCTTY));
        if (next.get() < 0) {
            break;
        }

        uint32_t next_index = UINT32_MAX;
        ASSERT_EQ(0, ioctl(next.get(), kTiocgptn, &next_index))
            << "later TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (next_index == first_index) {
            saw_reused_first_index = true;
            break;
        }

        masters.push_back(std::move(next));
    }

    EXPECT_TRUE(saw_reused_first_index)
        << "O_PATH TIOCGPTPEER peer should not keep the devpts index reserved";
}

TEST(TtyPtyHangup, TiocgptpeerSurvivesVisibleDevptsUnlink) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    uint32_t index = UINT32_MAX;
    ASSERT_EQ(0, ioctl(master.get(), kTiocgptn, &index))
        << "TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";

    int unlock = 0;
    ASSERT_EQ(0, ioctl(master.get(), kTiocsptlck, &unlock))
        << "unlock slave failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string path = "/dev/pts/" + std::to_string(index);
    ASSERT_EQ(0, unlink(path.c_str())) << "unlink(" << path << ") failed: errno=" << errno
                                       << " (" << strerror(errno) << ")";

    UniqueFd slave(ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    ASSERT_GE(slave.get(), 0) << "TIOCGPTPEER after unlink failed: errno=" << errno << " ("
                              << strerror(errno) << ")";

    struct termios term = {};
    EXPECT_EQ(0, tcgetattr(slave.get(), &term))
        << "tcgetattr(peer slave) failed: errno=" << errno << " (" << strerror(errno) << ")";
}

TEST(TtyPtyHangup, MultipleTiocgptpeerSlaveFdsKeepIndexReserved) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    uint32_t first_index = UINT32_MAX;
    ASSERT_EQ(0, ioctl(master.get(), kTiocgptn, &first_index))
        << "TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";

    int unlock = 0;
    ASSERT_EQ(0, ioctl(master.get(), kTiocsptlck, &unlock))
        << "unlock slave failed: errno=" << errno << " (" << strerror(errno) << ")";

    UniqueFd slave_a(ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    ASSERT_GE(slave_a.get(), 0) << "first TIOCGPTPEER failed: errno=" << errno << " ("
                                << strerror(errno) << ")";
    UniqueFd slave_b(ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    ASSERT_GE(slave_b.get(), 0) << "second TIOCGPTPEER failed: errno=" << errno << " ("
                                << strerror(errno) << ")";

    master.reset();
    slave_a.reset();

    UniqueFd second_master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(second_master.get(), 0) << "second open(/dev/ptmx) failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    uint32_t second_index = UINT32_MAX;
    ASSERT_EQ(0, ioctl(second_master.get(), kTiocgptn, &second_index))
        << "second TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_NE(first_index, second_index)
        << "pty index was reused while one TIOCGPTPEER slave fd was still alive";

    second_master.reset();
    slave_b.reset();

    bool saw_reused_first_index = false;
    std::vector<UniqueFd> masters;
    for (uint32_t i = 0; i < 128; ++i) {
        UniqueFd next(open("/dev/ptmx", O_RDWR | O_NOCTTY));
        if (next.get() < 0) {
            break;
        }
        uint32_t next_index = UINT32_MAX;
        ASSERT_EQ(0, ioctl(next.get(), kTiocgptn, &next_index))
            << "later TIOCGPTN failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (next_index == first_index) {
            saw_reused_first_index = true;
            break;
        }
        masters.push_back(std::move(next));
    }
    EXPECT_TRUE(saw_reused_first_index)
        << "pty index should be reusable after all TIOCGPTPEER slave fds close";
}

TEST(TtyPtyHangup, ClosingOneOfMultipleSlaveFdsDoesNotHangupMaster) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    UniqueFd second_slave(dup(pair.slave.get()));
    ASSERT_GE(second_slave.get(), 0) << "dup(slave) failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";

    pair.slave.reset();
    SetNonblock(pair.master.get());

    short revents = PollEvents(pair.master.get());
    EXPECT_EQ(0, revents & POLLHUP);

    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(pair.master.get(), &ch, 1));
    EXPECT_TRUE(IsWouldBlock(errno)) << "closing one slave fd should not hang up master, errno="
                                    << errno << " (" << strerror(errno) << ")";

    second_slave.reset();
    revents = PollEvents(pair.master.get());
    EXPECT_NE(0, revents & POLLHUP);
    ExpectReadErrno(pair.master.get(), EIO);
}

TEST(TtyPtyHangup, PacketStatusIsDeliveredBeforeHangupEio) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    int on = 1;
    ASSERT_EQ(0, ioctl(pair.master.get(), kTiocpkt, &on))
        << "ioctl(TIOCPKT) failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, tcflush(pair.slave.get(), TCOFLUSH))
        << "tcflush(TCOFLUSH) failed: errno=" << errno << " (" << strerror(errno) << ")";

    pair.slave.reset();

    unsigned char status = 0;
    ASSERT_EQ(1, read(pair.master.get(), &status, 1))
        << "read packet status failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(kTiocpktFlushWrite, status & kTiocpktFlushWrite);
    ExpectReadErrno(pair.master.get(), EIO);
}

TEST(TtyPtyHangup, MasterCloseMakesSlaveObserveHangup) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pair.master.reset();

    short revents = PollEvents(pair.slave.get());
    EXPECT_NE(0, revents & POLLHUP);

    char ch = 0;
    errno = 0;
    EXPECT_EQ(0, read(pair.slave.get(), &ch, 1));

    errno = 0;
    EXPECT_EQ(-1, write(pair.slave.get(), "x", 1));
    EXPECT_EQ(EIO, errno) << "slave write after master close errno=" << errno << " ("
                          << strerror(errno) << ")";
}

TEST(TtyPtyHangup, ChildExitDrainsSlaveOutputBeforeMasterEio) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    const char message[] = "short-output\n";
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        close(pair.master.release());
        ssize_t written = write(pair.slave.get(), message, sizeof(message) - 1);
        pair.slave.reset();
        _exit(written == static_cast<ssize_t>(sizeof(message) - 1) ? 0 : 1);
    }

    pair.slave.reset();

    char buf[sizeof(message)] = {};
    size_t total = 0;
    while (total < sizeof(message) - 1) {
        ssize_t n = read(pair.master.get(), buf + total, sizeof(message) - 1 - total);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        ASSERT_GT(n, 0) << "master failed to drain child output: errno=" << errno << " ("
                        << strerror(errno) << ")";
        total += static_cast<size_t>(n);
    }
    EXPECT_STREQ(message, buf);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    ExpectReadErrno(pair.master.get(), EIO);
}

TEST(TtyPtyHangup, LargeOpostSlaveWriteDrainsAndPreservesOnlcr) {
    PtyPair pair = OpenOpostPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    constexpr size_t kInputBytes = 32 * 1024;
    std::string input(kInputBytes, '\n');
    std::vector<char> output(kInputBytes * 2);

    WriteAllArgs args = {
        .fd = pair.slave.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    size_t total = 0;
    int poll_error = 0;
    bool read_ok = true;
    while (total < output.size()) {
        struct pollfd pfd = {
            .fd = pair.master.get(),
            .events = POLLIN | POLLHUP | POLLERR,
            .revents = 0,
        };
        int ret = poll(&pfd, 1, 5000);
        if (ret < 0 && errno == EINTR) {
            continue;
        }
        if (ret <= 0) {
            poll_error = ret < 0 ? errno : ETIMEDOUT;
            read_ok = false;
            break;
        }
        if ((pfd.revents & POLLIN) == 0) {
            poll_error = EIO;
            read_ok = false;
            break;
        }

        ssize_t n = read(pair.master.get(), output.data() + total, output.size() - total);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0) {
            poll_error = n < 0 ? errno : EIO;
            read_ok = false;
            break;
        }
        total += static_cast<size_t>(n);
    }

    if (!read_ok) {
        pair.slave.reset();
        pair.master.reset();
    }
    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_TRUE(read_ok) << "timed out or failed while draining PTY output: errno=" << poll_error
                         << " (" << strerror(poll_error) << "), total=" << total
                         << ", expected=" << output.size()
                         << ", writer_written=" << args.written
                         << ", writer_errno=" << args.error;
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(output.size(), total);

    for (size_t i = 0; i < input.size(); ++i) {
        EXPECT_EQ('\r', output[i * 2]) << "missing CR at converted newline " << i;
        EXPECT_EQ('\n', output[i * 2 + 1]) << "missing LF at converted newline " << i;
    }
}

TEST(TtyPtyHangup, LargeRawSlaveWriteDrainsWithSmallMasterReads) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    constexpr size_t kInputBytes = 32 * 1024;
    std::string input(kInputBytes, 'x');
    std::vector<char> output(kInputBytes);

    WriteAllArgs args = {
        .fd = pair.slave.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    size_t total = 0;
    int poll_error = 0;
    bool read_ok = true;
    while (total < output.size()) {
        struct pollfd pfd = {
            .fd = pair.master.get(),
            .events = POLLIN | POLLHUP | POLLERR,
            .revents = 0,
        };
        int ret = poll(&pfd, 1, 5000);
        if (ret < 0 && errno == EINTR) {
            continue;
        }
        if (ret <= 0) {
            poll_error = ret < 0 ? errno : ETIMEDOUT;
            read_ok = false;
            break;
        }
        if ((pfd.revents & POLLIN) == 0) {
            poll_error = EIO;
            read_ok = false;
            break;
        }

        const size_t chunk = std::min<size_t>(257, output.size() - total);
        ssize_t n = read(pair.master.get(), output.data() + total, chunk);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0) {
            poll_error = n < 0 ? errno : EIO;
            read_ok = false;
            break;
        }
        total += static_cast<size_t>(n);
    }

    if (!read_ok) {
        pair.slave.reset();
        pair.master.reset();
    }
    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_TRUE(read_ok) << "timed out or failed while draining raw PTY output: errno="
                         << poll_error << " (" << strerror(poll_error) << "), total=" << total
                         << ", expected=" << output.size()
                         << ", writer_written=" << args.written
                         << ", writer_errno=" << args.error;
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(output.size(), total);
    EXPECT_EQ(input, std::string(output.begin(), output.end()));
}

TEST(TtyPtyHangup, LargeCanonicalMasterWriteDrainsWithSmallSlaveReads) {
    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    std::string input;
    for (int i = 0; i < 64; ++i) {
        input.append(512, static_cast<char>('a' + (i % 26)));
        input.push_back('\n');
    }
    std::vector<char> output(input.size());

    WriteAllArgs args = {
        .fd = pair.master.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    size_t total = 0;
    int poll_error = 0;
    bool read_ok = true;
    while (total < output.size()) {
        struct pollfd pfd = {
            .fd = pair.slave.get(),
            .events = POLLIN | POLLHUP | POLLERR,
            .revents = 0,
        };
        int ret = poll(&pfd, 1, 5000);
        if (ret < 0 && errno == EINTR) {
            continue;
        }
        if (ret <= 0) {
            poll_error = ret < 0 ? errno : ETIMEDOUT;
            read_ok = false;
            break;
        }
        if ((pfd.revents & POLLIN) == 0) {
            poll_error = EIO;
            read_ok = false;
            break;
        }

        const size_t chunk = std::min<size_t>(257, output.size() - total);
        ssize_t n = read(pair.slave.get(), output.data() + total, chunk);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0) {
            poll_error = n < 0 ? errno : EIO;
            read_ok = false;
            break;
        }
        total += static_cast<size_t>(n);
    }

    if (!read_ok) {
        pair.slave.reset();
        pair.master.reset();
    }
    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_TRUE(read_ok) << "timed out or failed while draining canonical PTY input: errno="
                         << poll_error << " (" << strerror(poll_error) << "), total=" << total
                         << ", expected=" << output.size()
                         << ", writer_written=" << args.written
                         << ", writer_errno=" << args.error;
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(output.size(), total);
    EXPECT_EQ(input, std::string(output.begin(), output.end()));
}

TEST(TtyPtyHangup, Canonical4095PayloadPreserved) {
    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    std::string input(4095, 'x');
    input.push_back('\n');

    WriteAllArgs args = {
        .fd = pair.master.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    std::string output = ReadExpectedBytesWithPoll(pair.slave.get(), input.size(), 257);

    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(input.size(), output.size());
    EXPECT_EQ(input, output);
}

TEST(TtyPtyHangup, CanonicalOver4095PayloadTruncatedLikeLinux) {
    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    std::string input(4096, 'y');
    input.push_back('\n');
    std::string expected(4095, 'y');
    expected.push_back('\n');

    WriteAllArgs args = {
        .fd = pair.master.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    std::string output = ReadExpectedBytesWithPoll(pair.slave.get(), expected.size(), 257);

    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(expected.size(), output.size());
    EXPECT_EQ(expected, output);
}

TEST(TtyPtyHangup, MultipleLongCanonicalLinesDrainWithoutLoss) {
    PtyPair pair = OpenCanonicalNoEchoPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    std::string input;
    for (int i = 0; i < 8; ++i) {
        input.append(3072, static_cast<char>('a' + i));
        input.push_back('\n');
    }

    WriteAllArgs args = {
        .fd = pair.master.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    std::string output = ReadExpectedBytesWithPoll(pair.slave.get(), input.size(), 511);

    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(input.size(), output.size());
    EXPECT_EQ(input, output);
}

TEST(TtyPtyHangup, TciflushDoesNotDiscardLargeOpostSlaveOutput) {
    PtyPair pair = OpenOpostPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    constexpr size_t kInputLines = 24 * 1024;
    std::string input(kInputLines, '\n');
    std::vector<char> output(input.size() * 2);

    WriteAllArgs args = {
        .fd = pair.slave.get(),
        .data = input.data(),
        .len = input.size(),
        .written = 0,
        .error = 0,
    };
    pthread_t writer = {};
    ASSERT_EQ(0, pthread_create(&writer, nullptr, WriteAll, &args)) << "pthread_create failed";

    ASSERT_EQ(0, tcflush(pair.slave.get(), TCIFLUSH))
        << "tcflush(TCIFLUSH) failed: errno=" << errno << " (" << strerror(errno) << ")";

    size_t total = 0;
    int poll_error = 0;
    bool read_ok = true;
    while (total < output.size()) {
        struct pollfd pfd = {
            .fd = pair.master.get(),
            .events = POLLIN | POLLHUP | POLLERR,
            .revents = 0,
        };
        int ret = poll(&pfd, 1, 5000);
        if (ret < 0 && errno == EINTR) {
            continue;
        }
        if (ret <= 0) {
            poll_error = ret < 0 ? errno : ETIMEDOUT;
            read_ok = false;
            break;
        }
        if ((pfd.revents & POLLIN) == 0) {
            poll_error = EIO;
            read_ok = false;
            break;
        }

        ssize_t n = read(pair.master.get(), output.data() + total, output.size() - total);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0) {
            poll_error = n < 0 ? errno : EIO;
            read_ok = false;
            break;
        }
        total += static_cast<size_t>(n);
    }

    if (!read_ok) {
        pair.slave.reset();
        pair.master.reset();
    }
    ASSERT_EQ(0, pthread_join(writer, nullptr)) << "pthread_join failed";
    ASSERT_TRUE(read_ok) << "timed out or failed after TCIFLUSH while draining PTY output: errno="
                         << poll_error << " (" << strerror(poll_error) << "), total=" << total
                         << ", expected=" << output.size()
                         << ", writer_written=" << args.written
                         << ", writer_errno=" << args.error;
    ASSERT_EQ(0, args.error) << "writer failed after " << args.written << " bytes: errno="
                             << args.error << " (" << strerror(args.error) << ")";
    ASSERT_EQ(input.size(), args.written);
    ASSERT_EQ(output.size(), total);
    for (size_t i = 0; i < output.size(); i += 2) {
        EXPECT_EQ('\r', output[i]);
        EXPECT_EQ('\n', output[i + 1]);
    }
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
