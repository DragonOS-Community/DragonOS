#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <pty.h>
#include <signal.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <stdint.h>
#include <termios.h>
#include <unistd.h>

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

struct ConcurrentSlaveOpenArgs {
    const char* slave_name;
    int start_read_fd;
    int opened_fd;
    int open_errno;
};

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

TEST(TtyPtyHangup, TiocgptpeerFailsWhileSlaveLocked) {
    UniqueFd master(open("/dev/ptmx", O_RDWR | O_NOCTTY));
    ASSERT_GE(master.get(), 0) << "open(/dev/ptmx) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, ioctl(master.get(), kTiocgptpeer, O_RDWR | O_NOCTTY));
    EXPECT_EQ(EIO, errno) << "locked TIOCGPTPEER should fail with EIO, got errno=" << errno
                          << " (" << strerror(errno) << ")";
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

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
