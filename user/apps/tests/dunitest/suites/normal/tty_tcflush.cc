#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pty.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

namespace {

#ifndef TIOCPKT
constexpr int kTiocpkt = 0x5420;
#else
constexpr int kTiocpkt = TIOCPKT;
#endif

#ifndef TIOCPKT_FLUSHREAD
constexpr unsigned char kTiocpktFlushRead = 1;
#else
constexpr unsigned char kTiocpktFlushRead = TIOCPKT_FLUSHREAD;
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

PtyPair OpenRawPty() {
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

int WaitFionread(int fd) {
    int nread = 0;
    for (int i = 0; i < 100; ++i) {
        if (ioctl(fd, FIONREAD, &nread) < 0) {
            ADD_FAILURE() << "ioctl(FIONREAD) failed: errno=" << errno << " ("
                          << strerror(errno) << ")";
            return -1;
        }
        if (nread > 0) {
            return nread;
        }
        usleep(1000);
    }
    return nread;
}

unsigned char ReadPacketStatus(int master_fd) {
    int old_flags = fcntl(master_fd, F_GETFL);
    if (old_flags < 0) {
        ADD_FAILURE() << "fcntl(F_GETFL) failed: errno=" << errno << " (" << strerror(errno)
                      << ")";
        return 0;
    }

    if (fcntl(master_fd, F_SETFL, old_flags | O_NONBLOCK) < 0) {
        ADD_FAILURE() << "fcntl(F_SETFL, O_NONBLOCK) failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
        return 0;
    }

    unsigned char status = 0;
    for (int i = 0; i < 100; ++i) {
        errno = 0;
        ssize_t nread = read(master_fd, &status, 1);
        if (nread == 1) {
            EXPECT_EQ(0, fcntl(master_fd, F_SETFL, old_flags))
                << "restore packet fd flags failed: errno=" << errno << " (" << strerror(errno)
                << ")";
            return status;
        }
        if (nread < 0 && errno != EAGAIN && errno != EWOULDBLOCK) {
            ADD_FAILURE() << "read packet status failed: errno=" << errno << " ("
                          << strerror(errno) << ")";
            break;
        }
        usleep(10000);
    }

    ADD_FAILURE() << "timeout waiting for pty packet status";
    EXPECT_EQ(0, fcntl(master_fd, F_SETFL, old_flags))
        << "restore packet fd flags failed: errno=" << errno << " (" << strerror(errno) << ")";
    return 0;
}

TEST(TtyTcflush, NonblockEmptyReadReturnsEagain) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    int flags = fcntl(pair.slave.get(), F_GETFL);
    ASSERT_GE(flags, 0) << "fcntl(F_GETFL) failed: errno=" << errno << " (" << strerror(errno)
                        << ")";
    ASSERT_EQ(0, fcntl(pair.slave.get(), F_SETFL, flags | O_NONBLOCK))
        << "fcntl(F_SETFL, O_NONBLOCK) failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(pair.slave.get(), &ch, 1));
    EXPECT_EQ(EAGAIN, errno) << "empty nonblocking tty read should return EAGAIN, got errno="
                             << errno << " (" << strerror(errno) << ")";
}

TEST(TtyTcflush, TciflushDiscardsInput) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    ASSERT_EQ(3, write(pair.master.get(), "abc", 3))
        << "write master failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(3, WaitFionread(pair.slave.get()));

    ASSERT_EQ(0, tcflush(pair.slave.get(), TCIFLUSH))
        << "tcflush(TCIFLUSH) failed: errno=" << errno << " (" << strerror(errno) << ")";

    int nread = -1;
    ASSERT_EQ(0, ioctl(pair.slave.get(), FIONREAD, &nread))
        << "ioctl(FIONREAD) after TCIFLUSH failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    EXPECT_EQ(0, nread);

    int flags = fcntl(pair.slave.get(), F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(pair.slave.get(), F_SETFL, flags | O_NONBLOCK));

    char ch = 0;
    errno = 0;
    EXPECT_EQ(-1, read(pair.slave.get(), &ch, 1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(TtyTcflush, TcioflushDiscardsInput) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    ASSERT_EQ(3, write(pair.master.get(), "xyz", 3))
        << "write master failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(3, WaitFionread(pair.slave.get()));

    ASSERT_EQ(0, tcflush(pair.slave.get(), TCIOFLUSH))
        << "tcflush(TCIOFLUSH) failed: errno=" << errno << " (" << strerror(errno) << ")";

    int nread = -1;
    ASSERT_EQ(0, ioctl(pair.slave.get(), FIONREAD, &nread))
        << "ioctl(FIONREAD) after TCIOFLUSH failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    EXPECT_EQ(0, nread);
}

TEST(TtyTcflush, PacketModeReportsFlushStatus) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    int on = 1;
    ASSERT_EQ(0, ioctl(pair.master.get(), kTiocpkt, &on))
        << "ioctl(TIOCPKT) failed: errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(1, write(pair.master.get(), "r", 1));
    ASSERT_EQ(1, WaitFionread(pair.slave.get()));
    ASSERT_EQ(0, tcflush(pair.slave.get(), TCIFLUSH));
    unsigned char status = ReadPacketStatus(pair.master.get());
    EXPECT_NE(0, status & kTiocpktFlushRead);

    ASSERT_EQ(0, tcflush(pair.slave.get(), TCOFLUSH));
    status = ReadPacketStatus(pair.master.get());
    EXPECT_NE(0, status & kTiocpktFlushWrite);

    ASSERT_EQ(1, write(pair.master.get(), "b", 1));
    ASSERT_EQ(1, WaitFionread(pair.slave.get()));
    ASSERT_EQ(0, tcflush(pair.slave.get(), TCIOFLUSH));
    status = ReadPacketStatus(pair.master.get());
    EXPECT_EQ(kTiocpktFlushRead | kTiocpktFlushWrite,
              status & (kTiocpktFlushRead | kTiocpktFlushWrite));
}

TEST(TtyTcflush, InvalidArgumentReturnsEinval) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    errno = 0;
    EXPECT_EQ(-1, tcflush(pair.slave.get(), 999));
    EXPECT_EQ(EINVAL, errno);
}

TEST(TtyTcflush, MasterFlushAfterSlaveCloseSucceeds) {
    PtyPair pair = OpenRawPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pair.slave.reset();

    EXPECT_EQ(0, tcflush(pair.master.get(), TCIFLUSH))
        << "tcflush(master, TCIFLUSH) after slave close failed: errno=" << errno << " ("
        << strerror(errno) << ")";
    EXPECT_EQ(0, tcflush(pair.master.get(), TCOFLUSH))
        << "tcflush(master, TCOFLUSH) after slave close failed: errno=" << errno << " ("
        << strerror(errno) << ")";
    EXPECT_EQ(0, tcflush(pair.master.get(), TCIOFLUSH))
        << "tcflush(master, TCIOFLUSH) after slave close failed: errno=" << errno << " ("
        << strerror(errno) << ")";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
