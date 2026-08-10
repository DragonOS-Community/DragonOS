// tty_termios.cc — verify TCSAFLUSH / TCSADRAIN and legacy termio ioctls
// on a valid TTY fd (PTY slave).
//
// Regression coverage for: "tcsetattr(0, TCSAFLUSH, &t) fails with ENOTTY"
// and TCSETA/TCSETAW/TCSETAF/TCGETA returning ENOIOCTLCMD.

#include <gtest/gtest.h>

#include <atomic>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
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
    unsigned char _pad = 0;  /* match kernel PosixTermio layout */
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

class TermiosRestorer {
public:
    TermiosRestorer(int fd, const struct termios& term) : fd_(fd), term_(term) {}
    TermiosRestorer(const TermiosRestorer&) = delete;
    TermiosRestorer& operator=(const TermiosRestorer&) = delete;
    ~TermiosRestorer() { tcsetattr(fd_, TCSANOW, &term_); }

private:
    int fd_;
    struct termios term_;
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
        return {};
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

struct TcsetattrThreadArgs {
    int fd = -1;
    int action = TCSANOW;
    struct termios term = {};
    std::atomic<bool> started{false};
    std::atomic<bool> done{false};
    int rc = -1;
    int saved_errno = 0;
};

void* TcsetattrThread(void* opaque) {
    auto* args = static_cast<TcsetattrThreadArgs*>(opaque);
    args->started.store(true, std::memory_order_release);
    args->rc = tcsetattr(args->fd, args->action, &args->term);
    args->saved_errno = errno;
    args->done.store(true, std::memory_order_release);
    return nullptr;
}

bool WaitForFlag(const std::atomic<bool>& flag, int timeout_ms) {
    for (int elapsed = 0; elapsed < timeout_ms; ++elapsed) {
        if (flag.load(std::memory_order_acquire)) {
            return true;
        }
        usleep(1000);
    }
    return flag.load(std::memory_order_acquire);
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
}

/* --------------------------------------------------------------------------
 * tcsetattr TCSADRAIN — must not fail with ENOTTY.
 * TODO: add a stress test where master writes data → slave TCSADRAIN
 * → verify drain actually waited for output to complete.
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, TcsadrainSucceeds) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);
    EXPECT_EQ(tcsetattr(pty.slave.get(), TCSADRAIN, &t), 0) << strerror(errno);
}

/*
 * DragonOS keeps bytes that the peer N_TTY buffer cannot yet accept in a
 * driver-owned PTY queue analogous to Linux's peer flip-buffer. TCSADRAIN must
 * not wait for either layer of the peer input path to be consumed.
 */
TEST(TtyTermios, TcsadrainDoesNotWaitForPtyInputBacklog) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);

    int slave_flags = fcntl(pty.slave.get(), F_GETFL, 0);
    ASSERT_GE(slave_flags, 0);
    ASSERT_EQ(fcntl(pty.slave.get(), F_SETFL, slave_flags | O_NONBLOCK), 0);

    char fill[1024];
    memset(fill, 'q', sizeof(fill));
    size_t accepted = 0;
    for (;;) {
        ssize_t n = write(pty.slave.get(), fill, sizeof(fill));
        if (n > 0) {
            accepted += static_cast<size_t>(n);
            continue;
        }
        ASSERT_EQ(n, -1);
        ASSERT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << strerror(errno);
        break;
    }
    ASSERT_GT(accepted, 0u);

    TcsetattrThreadArgs args;
    args.fd = pty.slave.get();
    args.action = TCSADRAIN;
    args.term = t;
    pthread_t waiter;
    ASSERT_EQ(pthread_create(&waiter, nullptr, TcsetattrThread, &args), 0);
    if (!WaitForFlag(args.started, 1000)) {
        ADD_FAILURE() << "tcsetattr thread did not start";
        pty.master.reset();
        pthread_join(waiter, nullptr);
        return;
    }

    if (!WaitForFlag(args.done, 1000)) {
        ADD_FAILURE() << "TCSADRAIN waited for unread PTY input";
        pty.master.reset();
    }
    ASSERT_EQ(pthread_join(waiter, nullptr), 0);
    ASSERT_TRUE(args.done.load(std::memory_order_acquire));
    EXPECT_EQ(args.rc, 0) << "errno=" << args.saved_errno << " ("
                          << strerror(args.saved_errno) << ")";
}

TEST(TtyTermios, HungUpSlaveModeIoctlsReturnLinuxErrors) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.master.get(), 0);
    ASSERT_GE(pty.slave.get(), 0);

    struct termios term = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &term), 0) << strerror(errno);
    TermioCompat legacy = {};
    ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &legacy), 0) << strerror(errno);

    pty.master.reset();

    errno = 0;
    EXPECT_EQ(tcgetattr(pty.slave.get(), &term), -1);
    EXPECT_EQ(errno, EIO);

    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCGETA, &legacy), -1);
    EXPECT_EQ(errno, EIO);

    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETA, &legacy), -1);
    EXPECT_EQ(errno, EIO);

    pid_t pgrp = getpgrp();
    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TIOCSPGRP, &pgrp), -1);
    EXPECT_EQ(errno, ENOTTY);
}

TEST(TtyTermios, PtyMasterDrainActionsSucceedAfterSlaveClose) {
    for (int action : {TCSADRAIN, TCSAFLUSH}) {
        auto pty = OpenRawPty();
        ASSERT_GE(pty.master.get(), 0);
        ASSERT_GE(pty.slave.get(), 0);

        struct termios t = {};
        ASSERT_EQ(tcgetattr(pty.master.get(), &t), 0) << strerror(errno);

        int slave_flags = fcntl(pty.slave.get(), F_GETFL, 0);
        ASSERT_GE(slave_flags, 0);
        ASSERT_EQ(fcntl(pty.slave.get(), F_SETFL, slave_flags | O_NONBLOCK), 0);

        char fill[1024];
        memset(fill, 'z', sizeof(fill));
        bool saturated = false;
        for (int i = 0; i < 1024; ++i) {
            ssize_t n = write(pty.slave.get(), fill, sizeof(fill));
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                saturated = true;
                break;
            }
            ASSERT_GT(n, 0) << strerror(errno);
        }
        ASSERT_TRUE(saturated);
        pty.slave.reset();

        TcsetattrThreadArgs args;
        args.fd = pty.master.get();
        args.action = action;
        args.term = t;
        pthread_t waiter;
        ASSERT_EQ(pthread_create(&waiter, nullptr, TcsetattrThread, &args), 0);
        if (!WaitForFlag(args.started, 1000)) {
            ADD_FAILURE() << "tcsetattr thread did not start";
            pty.master.reset();
            pthread_join(waiter, nullptr);
            return;
        }
        if (!WaitForFlag(args.done, 1000)) {
            ADD_FAILURE() << "PTY master drain action hung after slave close";
            pty.master.reset();
        }
        ASSERT_EQ(pthread_join(waiter, nullptr), 0);
        EXPECT_EQ(args.rc, 0) << "action=" << action << " errno=" << args.saved_errno
                              << " (" << strerror(args.saved_errno) << ")";
    }
}

/*
 * DragonOS N_TTY can retain an echo step when the PTY bridge has no room.
 * TCSADRAIN must wait for that ldisc-owned output to be submitted, but it
 * must not wait for the master application to consume the entire PTY input.
 */
TEST(TtyTermios, TcsadrainWaitsForRetainedEchoOnly) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    struct termios t = {};
    ASSERT_EQ(tcgetattr(pty.slave.get(), &t), 0) << strerror(errno);
    t.c_iflag = 0;
    t.c_oflag = 0;
    t.c_lflag = ECHO | ECHOCTL;
    ASSERT_EQ(tcsetattr(pty.slave.get(), TCSANOW, &t), 0) << strerror(errno);

    int slave_flags = fcntl(pty.slave.get(), F_GETFL, 0);
    ASSERT_GE(slave_flags, 0);
    ASSERT_EQ(fcntl(pty.slave.get(), F_SETFL, slave_flags | O_NONBLOCK), 0);

    // Fill both the peer N_TTY buffer and the PTY bridge so the two-byte
    // ECHOCTL rendering remains owned by this line discipline. The bridge
    // backlog itself must not keep TCSADRAIN waiting after that echo is
    // submitted.
    char fill[1024];
    memset(fill, 'x', sizeof(fill));
    size_t accepted = 0;
    for (;;) {
        ssize_t n = write(pty.slave.get(), fill, sizeof(fill));
        if (n > 0) {
            accepted += static_cast<size_t>(n);
            continue;
        }
        ASSERT_EQ(n, -1);
        ASSERT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << strerror(errno);
        break;
    }
    ASSERT_GT(accepted, 0u);

    const char control = 0x01;  // ECHOCTL renders this as "^A" (two bytes).
    ASSERT_EQ(write(pty.master.get(), &control, 1), 1) << strerror(errno);

    // RX delivery and echo generation run asynchronously. Observe the byte on
    // the slave before starting tcsetattr so the test cannot pass merely
    // because the drain raced ahead of the retained echo step.
    char received = 0;
    bool input_delivered = false;
    for (int i = 0; i < 1000; ++i) {
        ssize_t n = read(pty.slave.get(), &received, 1);
        if (n == 1) {
            input_delivered = true;
            break;
        }
        ASSERT_EQ(n, -1);
        ASSERT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << strerror(errno);
        usleep(1000);
    }
    ASSERT_TRUE(input_delivered) << "PTY input delivery timed out";
    ASSERT_EQ(received, control);

    int master_flags = fcntl(pty.master.get(), F_GETFL, 0);
    ASSERT_GE(master_flags, 0);
    ASSERT_EQ(fcntl(pty.master.get(), F_SETFL, master_flags | O_NONBLOCK), 0);

    TcsetattrThreadArgs args;
    args.fd = pty.slave.get();
    args.action = TCSADRAIN;
    args.term = t;
    pthread_t waiter;
    ASSERT_EQ(pthread_create(&waiter, nullptr, TcsetattrThread, &args), 0);
    if (!WaitForFlag(args.started, 1000)) {
        ADD_FAILURE() << "tcsetattr thread did not start";
        pty.master.reset();
        pthread_join(waiter, nullptr);
        return;
    }

    // With no room for the retained two-byte echo step, the ioctl must not
    // report completion. This catches the old bool drain / always-true wait.
    EXPECT_FALSE(WaitForFlag(args.done, 20));

    char echo_room[2];
    for (size_t freed = 0; freed < sizeof(echo_room); ++freed) {
        bool got_byte = false;
        for (int i = 0; i < 1000; ++i) {
            ssize_t n = read(pty.master.get(), &echo_room[freed], 1);
            if (n == 1) {
                got_byte = true;
                break;
            }
            if (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK) {
                ADD_FAILURE() << "master read failed: " << strerror(errno);
                break;
            }
            usleep(1000);
        }
        if (!got_byte) {
            ADD_FAILURE() << "failed to free echo byte " << freed;
            pty.master.reset();
            pthread_join(waiter, nullptr);
            return;
        }
        if (freed == 0) {
            EXPECT_FALSE(WaitForFlag(args.done, 20));
        }
    }
    EXPECT_TRUE(WaitForFlag(args.done, 1000));

    if (!args.done.load(std::memory_order_acquire)) {
        // Closing the peer guarantees a bounded cleanup path even on failure.
        pty.master.reset();
    }
    ASSERT_EQ(pthread_join(waiter, nullptr), 0);
    ASSERT_TRUE(args.done.load(std::memory_order_acquire));
    EXPECT_EQ(args.rc, 0) << "errno=" << args.saved_errno << " ("
                          << strerror(args.saved_errno) << ")";

    // TCSADRAIN waits for the retained echo to be accepted by the PTY driver,
    // not for the peer to consume the already accepted backlog.
    char backlog = 0;
    EXPECT_EQ(read(pty.master.get(), &backlog, 1), 1);
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
}

/* --------------------------------------------------------------------------
 * Legacy termio TCSETA / TCSETAW / TCSETAF
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, LegacyTcsetaFamily) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    TermioCompat tio = {};
    tio.c_lflag |= ISIG;
    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETA, &tio), 0)
        << "TCSETA: errno=" << errno << " (" << strerror(errno) << ")";
    /* Verify TCSETA was applied. */
    {
        TermioCompat rdback = {};
        ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &rdback), 0);
        EXPECT_NE(rdback.c_lflag & ISIG, 0u) << "TCSETA ISIG flag applied";
    }

    tio.c_lflag &= ~static_cast<unsigned short>(ISIG);
    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETAW, &tio), 0)
        << "TCSETAW: errno=" << errno << " (" << strerror(errno) << ")";
    /* Verify TCSETAW was applied. */
    {
        TermioCompat rdback = {};
        ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &rdback), 0);
        EXPECT_EQ(rdback.c_lflag & ISIG, 0u) << "TCSETAW cleared ISIG";
    }

    tio.c_lflag |= ISIG;
    errno = 0;
    EXPECT_EQ(ioctl(pty.slave.get(), TCSETAF, &tio), 0)
        << "TCSETAF: errno=" << errno << " (" << strerror(errno) << ")";
    /* Verify TCSETAF was applied. */
    {
        TermioCompat rdback = {};
        ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &rdback), 0);
        EXPECT_NE(rdback.c_lflag & ISIG, 0u) << "TCSETAF ISIG flag applied";
    }
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
    EXPECT_EQ(full.c_cflag & 0xffff, tio.c_cflag)
        << "termio c_cflag should be low 16 bits of termios c_cflag";
    EXPECT_EQ(full.c_oflag & 0xffff, tio.c_oflag)
        << "termio c_oflag should be low 16 bits of termios c_oflag";
}

/* --------------------------------------------------------------------------
 * c_line round-trip: non-zero c_line survives TCSETA → TCGETA
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, CLineRoundtrip) {
    auto pty = OpenRawPty();
    ASSERT_GE(pty.slave.get(), 0);

    TermioCompat tio = {};
    tio.c_line = 42;
    ASSERT_EQ(ioctl(pty.slave.get(), TCSETA, &tio), 0)
        << "TCSETA with c_line=42";

    TermioCompat tio2 = {};
    ASSERT_EQ(ioctl(pty.slave.get(), TCGETA, &tio2), 0);
    EXPECT_EQ(tio2.c_line, 42u)
        << "c_line should survive termio round-trip";
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
    /* Set a flag bit we know is currently clear so the subsequent
     * clear of that bit is a real operation, not a no-op. */
    tio.c_lflag |= ECHO;
    ASSERT_EQ(ioctl(pty.slave.get(), TCSETA, &tio), 0)
        << "TCSETA set ECHO before merge check";
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
 * Serial8250 termios must reach the UART hardware callback. Before the
 * regression fix the default ENOSYS callback made the TTY core restore all
 * hardware-related c_cflag bits even though tcsetattr/ioctl returned success.
 * -------------------------------------------------------------------------- */
TEST(TtyTermios, Serial8250AppliesModernAndLegacySettings) {
    UniqueFd serial(open("/dev/ttyS0", O_RDWR | O_NOCTTY));
    if (serial.get() < 0) {
        GTEST_SKIP() << "cannot open /dev/ttyS0: " << strerror(errno);
        return;
    }

    struct termios original = {};
    ASSERT_EQ(tcgetattr(serial.get(), &original), 0) << strerror(errno);
    TermiosRestorer restore(serial.get(), original);

    struct termios modern = original;
    modern.c_cflag &= ~(CSIZE | CSTOPB | PARODD);
    modern.c_cflag |= CS7 | PARENB;
    ASSERT_EQ(cfsetispeed(&modern, B9600), 0);
    ASSERT_EQ(cfsetospeed(&modern, B9600), 0);
    ASSERT_EQ(tcsetattr(serial.get(), TCSANOW, &modern), 0) << strerror(errno);

    struct termios modern_back = {};
    ASSERT_EQ(tcgetattr(serial.get(), &modern_back), 0) << strerror(errno);
    EXPECT_EQ(cfgetospeed(&modern_back), static_cast<speed_t>(B9600));
    EXPECT_EQ(modern_back.c_cflag & CSIZE, static_cast<tcflag_t>(CS7));
    EXPECT_NE(modern_back.c_cflag & PARENB, 0u);
    EXPECT_EQ(modern_back.c_cflag & PARODD, 0u);

    TermioCompat legacy = {};
    ASSERT_EQ(ioctl(serial.get(), TCGETA, &legacy), 0) << strerror(errno);
    legacy.c_cflag &= ~static_cast<unsigned short>(CBAUD | CSIZE | PARENB | PARODD);
    legacy.c_cflag |= static_cast<unsigned short>(B19200 | CS8);
    ASSERT_EQ(ioctl(serial.get(), TCSETA, &legacy), 0) << strerror(errno);

    struct termios legacy_back = {};
    ASSERT_EQ(tcgetattr(serial.get(), &legacy_back), 0) << strerror(errno);
    EXPECT_EQ(cfgetospeed(&legacy_back), static_cast<speed_t>(B19200));
    EXPECT_EQ(legacy_back.c_cflag & CSIZE, static_cast<tcflag_t>(CS8));
    EXPECT_EQ(legacy_back.c_cflag & PARENB, 0u);
}

/* --------------------------------------------------------------------------
 * tcsetattr on a non-TTY fd must fail with Linux's ENOTTY.
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
    int saved_errno = errno;
    close(fd);
    EXPECT_EQ(rc, -1) << "tcsetattr on non-TTY fd should fail";
    EXPECT_EQ(saved_errno, ENOTTY)
        << "tcsetattr on non-TTY fd should report ENOTTY";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
