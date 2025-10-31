#include <fcntl.h>
#include <pty.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/select.h>
#include <termios.h>
#include <unistd.h>

int main() {
  int ptm, pts;
  char name[256];
  struct termios term;

  if (openpty(&ptm, &pts, name, NULL, NULL) == -1) {
    perror("openpty");
    exit(EXIT_FAILURE);
  }

  printf("slave name: %s fd: %d\n", name, pts);

  tcgetattr(pts, &term);
  term.c_lflag &= ~(ICANON | ECHO);
  term.c_cc[VMIN] = 1;
  term.c_cc[VTIME] = 0;
  tcsetattr(pts, TCSANOW, &term);

  printf("before print to pty slave\n");
  dprintf(pts, "Hello world!\n");

  /* ---- 用 select 检查 ptm 是否可读 ---- */
  fd_set rfds;
  FD_ZERO(&rfds);
  FD_SET(ptm, &rfds);
  struct timeval tv = {1, 0}; // 1秒超时

  int ret = select(ptm + 1, &rfds, NULL, NULL, &tv);
  if (ret == -1) {
    perror("select ptm");
  } else if (ret == 0) {
    printf("no data from slave within timeout\n");
  } else if (FD_ISSET(ptm, &rfds)) {
    char buf[256];
    ssize_t n = read(ptm, buf, sizeof(buf));
    if (n > 0) {
      printf("read %ld bytes from slave: %.*s", n, (int)n, buf);
    }
  }

  dprintf(ptm, "hello world from master\n");

  /* ---- 用 select 检查 pts 是否可读 ---- */
  FD_ZERO(&rfds);
  FD_SET(pts, &rfds);
  tv.tv_sec = 1;
  tv.tv_usec = 0;

  ret = select(pts + 1, &rfds, NULL, NULL, &tv);
  if (ret == -1) {
    perror("select pts");
  } else if (ret == 0) {
    printf("no data from master within timeout\n");
  } else if (FD_ISSET(pts, &rfds)) {
    char nbuf[256];
    ssize_t nn = read(pts, nbuf, sizeof(nbuf));
    if (nn > 0) {
      printf("read %ld bytes from master: %.*s", nn, (int)nn, nbuf);
    }
  }

  close(ptm);
  close(pts);
  return 0;
}
