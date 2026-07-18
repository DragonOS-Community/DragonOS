// test_tty_termios.c — verify TCSAFLUSH / TCSADRAIN and legacy termio ioctls
// on a valid TTY fd (PTY slave).
//
// Regression coverage for: "tcsetattr(0, TCSAFLUSH, &t) fails with ENOTTY"
// and TCSETA/TCSETAW/TCSETAF/TCGETA returning ENOIOCTLCMD.
//
// This file intentionally overlaps with dunitest/suites/normal/tty_termios.cc.
// The C version is a fast, self-contained static binary for QEMU smoke testing.
// The C++ gtest version is the full CI regression suite.  Keep both: the C test
// catches regressions in environments where dunitest cannot run.

#include <errno.h>
#include <fcntl.h>
#include <pty.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

/* Legacy SVR4 struct termio — not exposed by glibc's <termios.h>. */
#define NCC 8
struct termio_compat {
    unsigned short c_iflag;
    unsigned short c_oflag;
    unsigned short c_cflag;
    unsigned short c_lflag;
    unsigned char c_line;
    unsigned char c_cc[NCC];
    unsigned char _pad;   /* match kernel PosixTermio layout */
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

static int failures = 0;

#define CHECK(cond, msg)                                                   \
    do {                                                                   \
        if (!(cond)) {                                                     \
            fprintf(stderr, "FAIL: %s (errno=%d: %s)\n", msg, errno,       \
                    strerror(errno));                                      \
            failures++;                                                    \
        } else {                                                           \
            printf("ok: %s\n", msg);                                       \
        }                                                                  \
    } while (0)

/* Like CHECK, but for assertions that don't follow a syscall —
 * errno is irrelevant and printing it is misleading. */
#define CHECK_NOERR(cond, msg)                                             \
    do {                                                                   \
        if (!(cond)) {                                                     \
            fprintf(stderr, "FAIL: %s\n", msg);                            \
            failures++;                                                    \
        } else {                                                           \
            printf("ok: %s\n", msg);                                       \
        }                                                                  \
    } while (0)

int main(void) {
    int ptm = -1, pts = -1;

    if (openpty(&ptm, &pts, NULL, NULL, NULL) == -1) {
        perror("openpty");
        return EXIT_FAILURE;
    }

    struct termios t;

    /* 1. tcgetattr + tcsetattr(TCSANOW) round-trip. */
    CHECK(tcgetattr(pts, &t) == 0, "tcgetattr on PTY slave");
    t.c_lflag &= ~(ICANON | ECHO);
    CHECK(tcsetattr(pts, TCSANOW, &t) == 0, "tcsetattr TCSANOW");
    struct termios back;
    CHECK(tcgetattr(pts, &back) == 0, "tcgetattr after TCSANOW");
    CHECK((back.c_lflag & (ICANON | ECHO)) == 0, "TCSANOW settings survive");

    /* 2. tcsetattr TCSADRAIN — must not fail with ENOTTY.
     * TODO: add a stress test where master writes data → slave TCSADRAIN
     * → verify drain actually waited for output to complete. */
    CHECK(tcsetattr(pts, TCSADRAIN, &t) == 0, "tcsetattr TCSADRAIN succeeds");

    /* 3. tcsetattr TCSAFLUSH — the dpkg/apt regression. */
    CHECK(tcsetattr(pts, TCSAFLUSH, &t) == 0, "tcsetattr TCSAFLUSH succeeds");
    CHECK(tcgetattr(pts, &back) == 0, "tcgetattr after TCSAFLUSH");
    CHECK((back.c_lflag & (ICANON | ECHO)) == 0, "TCSAFLUSH settings survive");

    /* 4. Legacy termio ioctls — must not return ENOTTY. */
    struct termio_compat tio;
    memset(&tio, 0, sizeof(tio));

    errno = 0;
    CHECK(ioctl(pts, TCGETA, &tio) == 0, "ioctl TCGETA succeeds");

    tio.c_lflag |= ISIG;
    errno = 0;
    CHECK(ioctl(pts, TCSETA, &tio) == 0, "ioctl TCSETA succeeds");

    errno = 0;
    CHECK(ioctl(pts, TCSETAW, &tio) == 0, "ioctl TCSETAW succeeds");

    errno = 0;
    CHECK(ioctl(pts, TCSETAF, &tio) == 0, "ioctl TCSETAF succeeds");

    /* 5. Cross-check: TCGETA reports low 16 bits of what TCGETS reports. */
    struct termios full = {};
    memset(&tio, 0, sizeof(tio));
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr for cross-check");
    CHECK(ioctl(pts, TCGETA, &tio) == 0, "TCGETA for cross-check");
    CHECK_NOERR((full.c_lflag & 0xffff) == tio.c_lflag, "termio c_lflag is low 16 bits");
    CHECK_NOERR((full.c_iflag & 0xffff) == tio.c_iflag, "termio c_iflag is low 16 bits");

    /* c_line round-trip: verify that a non-zero c_line value set
     * through TCSETA is preserved when read back via TCGETA.  The
     * kernel's c_line_abi field retains the raw ABI byte even when
     * LineDisciplineType::from_line falls back to N_TTY. */
    memset(&tio, 0, sizeof(tio));
    tio.c_line = 42;
    CHECK(ioctl(pts, TCSETA, &tio) == 0, "TCSETA with c_line=42");
    struct termio_compat tio2;
    memset(&tio2, 0, sizeof(tio2));
    CHECK(ioctl(pts, TCGETA, &tio2) == 0, "TCGETA after c_line=42");
    CHECK_NOERR(tio2.c_line == 42, "c_line preserved through termio round-trip");

    /* 6. termio round-trip preserves merge semantics: high bits of c_cflag
     * (e.g. CIBAUD/ADDRB region) set via termios survive a TCSETA call. */
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr before TCSETA merge check");
    tcflag_t orig_cflag = full.c_cflag;
    memset(&tio, 0, sizeof(tio));
    CHECK(ioctl(pts, TCGETA, &tio) == 0, "TCGETA before TCSETA merge check");
    /* Set a flag bit we know is currently clear so the subsequent
     * clear of that bit is a real operation, not a no-op. */
    tio.c_lflag |= ECHO;
    CHECK(ioctl(pts, TCSETA, &tio) == 0, "TCSETA set ECHO before merge check");
    tio.c_lflag &= ~(unsigned short)ECHO; /* flip a low-16-bit flag */
    CHECK(ioctl(pts, TCSETA, &tio) == 0, "TCSETA merge apply");
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr after TCSETA merge check");
    CHECK_NOERR((full.c_cflag & 0xffff0000u) == (orig_cflag & 0xffff0000u),
          "TCSETA preserves high 16 bits of c_cflag");
    CHECK_NOERR((full.c_lflag & ECHO) == 0, "TCSETA applied low-16-bit change");

    /* 7. tcsetattr on a non-TTY fd must fail. The exact errno depends on
     * the underlying device (ENOTTY, ENOSYS, EINVAL are all valid). */
    int nullfd = open("/dev/null", O_RDWR);
    if (nullfd >= 0) {
        errno = 0;
        int rc = tcsetattr(nullfd, TCSANOW, &t);
        CHECK(rc == -1 && errno != 0, "tcsetattr on /dev/null fails");
        close(nullfd);
    } else {
        printf("skip: cannot open /dev/null (errno=%d: %s)\n", errno, strerror(errno));
    }

    close(ptm);
    close(pts);

    if (failures) {
        fprintf(stderr, "%d test(s) FAILED\n", failures);
        return EXIT_FAILURE;
    }
    printf("all tty termios tests passed\n");
    return EXIT_SUCCESS;
}
