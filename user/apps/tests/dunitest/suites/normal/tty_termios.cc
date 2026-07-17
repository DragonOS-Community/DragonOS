// tty_termios.cc — verify TCSAFLUSH / TCSADRAIN and legacy termio ioctls
// on a valid TTY fd (PTY slave).
//
// Regression coverage for: "tcsetattr(0, TCSAFLUSH, &t) fails with ENOTTY"
// and TCSETA/TCSETAW/TCSETAF/TCGETA returning ENOIOCTLCMD.

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pty.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

namespace {

/* Legacy SVR4 struct termio — not exposed by glibc's <termios.h>. */
constexpr int kNcc = 8;
struct TermioCompat {
    unsigned short c_iflag;
    unsigned short c_oflag;
    unsigned short c_cflag;
    unsigned short c_lflag;
    unsigned char c_line;
    unsigned char c_cc[kNcc];
};

#ifndef TCGETA
#define TCGETA 0x5405
#endif
#ifndef TCSETA
#define TCSETA 0x5406
#endif
#ifndef TCSETAW
#define TCSETAW 0x5407
#endif
#ifndef TCSETAF
#define TCSETAF 0x5408
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

/* --------------------------------------------------------------------------
 * tcgetattr + tcsetattr(TCSANOW) round-trip
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TcSanowRoundTrip) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);

    tcflag_t orig = t.c_lflag;
    t.c_lflag &= ~(ICANON | ECHO);
    ASSERT_EQ(tcsetattr(pty.slave.get(), TCSANOW, &t), 0) << strerror(errno);

    struct termios back = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &back), 0) << strerror(errno);
    EXPECT_EQ(back.c_lflag & (ICANON | ECHO), 0u)
        << "TCSANOW settings should survive";
    t.c_lflag = orig;
}

/* --------------------------------------------------------------------------
 * tcsetattr TCSADRAIN — must not fail with ENOTTY
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TcsadrainSucceeds) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);
    EXPECT_EQ(tcsetattr(pty.slave.get(), TCSADRAIN, &t), 0) << strerror(errno);
}

/* --------------------------------------------------------------------------
 * tcsetattr TCSAFLUSH — the dpkg/apt regression
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TcsaflushSucceeds) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);
    t.c_lflag &= ~(ICANON | ECHO);
    ASSERT_EQ(tcsetattr(pty.slave.get(), TCSAFLUSH, &t), 0) << strerror(errno);

    struct termios back = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &back), 0) << strerror(errno);
    EXPECT_EQ(back.c_lflag & (ICANON | ECHO), 0u)
        << "TCSAFLUSH settings should survive";
}

/* --------------------------------------------------------------------------
 * Legacy termio TCGETA — must not return ENOTTY
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, LegacyTcgeta) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    TermioCompat tio = {};
    errno = 0;
    ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &tio), 0)
        << "TCGETA should succeed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_NE(errno, ENOTTY) << "TCGETA must not return ENOTTY";
}

/* --------------------------------------------------------------------------
 * Legacy termio TCSETA / TCSETAW / TCSETAF
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, LegacyTcsetaFamily) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    TermioCompat tio = {};
    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETA, &tio), 0)
        << "TCSETA: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_NE(errno, ENOTTY);

    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETAW, &tio), 0)
        << "TCSETAW: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_NE(errno, ENOTTY);

    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETAF, &tio), 0)
        << "TCSETAF: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_NE(errno, ENOTTY);
}

/* --------------------------------------------------------------------------
 * Cross-check: TCGETA low 16 bits match TCGETS
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TermioLow16Bits) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios full = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &full), 0);

    TermioCompat tio = {};
    ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &tio), 0);

    EXPECT_EQ(full.c_lflag & 0xffff, tio.c_lflag)
        << "termio c_lflag should be low 16 bits of termios c_lflag";
    EXPECT_EQ(full.c_iflag & 0xffff, tio.c_iflag)
        << "termio c_iflag should be low 16 bits of termios c_iflag";
}

/* --------------------------------------------------------------------------
 * Merge semantics: high 16 bits of c_cflag survive a TCSETA call
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TermioMergeHighBits) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios full = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &full), 0);
    tcflag_t orig_cflag = full.c_cflag;

    TermioCompat tio = {};
    ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &tio), 0);
    tio.c_lflag &= ~static_cast<unsigned short>(ECHO); /* flip a low-16-bit flag */

    ASSERT_EQ(ioctl(pty.slave.get(), TCSETA, &tio), 0)
        << "TCSETA merge apply: errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(tcgetattr(pty.slave.get(), &full), 0);
    EXPECT_EQ(full.c_cflag & 0xffff0000u, orig_cflag & 0xffff0000u)
        << "TCSETA should preserve high 16 bits of c_cflag";
    EXPECT_EQ(full.c_lflag & ECHO, 0u)
        << "TCSETA should apply low-16-bit change";
}

/* --------------------------------------------------------------------------
 * tcsetattr on non-TTY fd must fail (any error, not crash)
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, NonTtyFails) {
    struct termios t = {};
    int fd = open("/dev/null", O_RDWR);
    if (fd < 0) {
        GTEST_SKIP() << "cannot open /dev/null: " << strerror(errno);
        return;
    }
    errno = 0;
    int rc = tcsetattr(fd, TCSANOW, &t);
    close(fd);
    EXPECT_EQ(rc, -1) << "tcsetattr on non-TTY fd should fail";
    EXPECT_NE(errno, 0) << "tcsetattr on non-TTY fd should set errno";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
