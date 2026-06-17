#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pty.h>
#include <signal.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

namespace {

#ifndef TIOCPKT
constexpr int kTiocpkt = 0x5420;
#else
constexpr int kTiocpkt = TIOCPKT;
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

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
