// test_tty_termios.c — verify TCSAFLUSH / TCSADRAIN and legacy termio ioctls
// on a valid TTY fd (PTY slave).
//
// Regression coverage for: "tcsetattr(0, TCSAFLUSH, &t) fails with ENOTTY"
// and TCSETA/TCSETAW/TCSETAF/TCGETA returning ENOIOCTLCMD.

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

int main(void) {
    int ptm = -1, pts = -1;
    char name[256];

    if (openpty(&ptm, &pts, name, NULL, NULL) == -1) {
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

    /* 2. tcsetattr TCSADRAIN — must not fail with ENOTTY. */
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
    struct termios full;
    memset(&tio, 0, sizeof(tio));
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr for cross-check");
    CHECK(ioctl(pts, TCGETA, &tio) == 0, "TCGETA for cross-check");
    CHECK((full.c_lflag & 0xffff) == tio.c_lflag, "termio c_lflag is low 16 bits");
    CHECK((full.c_iflag & 0xffff) == tio.c_iflag, "termio c_iflag is low 16 bits");

    /* 6. termio round-trip preserves merge semantics: high bits of c_cflag
     * (e.g. CIBAUD/ADDRB region) set via termios survive a TCSETA call. */
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr before TCSETA merge check");
    tcflag_t orig_cflag = full.c_cflag;
    memset(&tio, 0, sizeof(tio));
    CHECK(ioctl(pts, TCGETA, &tio) == 0, "TCGETA before TCSETA merge check");
    tio.c_lflag &= ~(unsigned short)ECHO; /* flip a low-16-bit flag */
    CHECK(ioctl(pts, TCSETA, &tio) == 0, "TCSETA merge apply");
    CHECK(tcgetattr(pts, &full) == 0, "tcgetattr after TCSETA merge check");
    CHECK((full.c_cflag & 0xffff0000u) == (orig_cflag & 0xffff0000u),
          "TCSETA preserves high 16 bits of c_cflag");
    CHECK((full.c_lflag & ECHO) == 0, "TCSETA applied low-16-bit change");

    /* 7. tcsetattr on a non-TTY fd must fail. The exact errno depends on
     * the underlying device (ENOTTY, ENOSYS, EINVAL are all valid). */
    int nullfd = open("/dev/null", O_RDWR);
    if (nullfd >= 0) {
        errno = 0;
        int rc = tcsetattr(nullfd, TCSANOW, &t);
        CHECK(rc == -1 && errno != 0, "tcsetattr on /dev/null fails");
        close(nullfd);
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
