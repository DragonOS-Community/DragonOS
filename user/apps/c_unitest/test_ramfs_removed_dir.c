#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
static int failures, total;
#define CHECK(n, c)                                                            \
  do {                                                                         \
    int ok = (c);                                                              \
    printf("%s %s\n", ok ? "PASS" : "FAIL", n);                                \
    ++total;                                                                   \
    failures += !ok;                                                           \
  } while (0)
#define DENIED(n, op)                                                          \
  do {                                                                         \
    errno = 0;                                                                 \
    int rc = (op);                                                             \
    int err = errno;                                                           \
    printf("%s rc=%d errno=%d\n", n, rc, err);                                 \
    CHECK(n, rc == -1 && err == ENOENT);                                       \
    if (rc >= 0 && (n)[0] == 'o')                                              \
      close(rc);                                                               \
  } while (0)
int main(void) {
  char base[] = "/tmp/ramfs-dead-XXXXXX";
  if (!mkdtemp(base) || mount("none", base, "ramfs", 0, NULL) || chdir(base)) {
    perror("setup");
    return 2;
  }
  int root = open(".", O_RDONLY | O_DIRECTORY);
  struct stat st;
  int src = open("source", O_CREAT | O_RDWR, 0600);
  if (root < 0 || src < 0 || mkdir("gone", 0700)) {
    perror("setup");
    return 2;
  }
  close(src);
  int fd = open("gone", O_RDONLY | O_DIRECTORY);
  if (fd < 0) {
    perror("open dir");
    return 2;
  }
  CHECK("live directory links", fstat(fd, &st) == 0 && st.st_nlink == 2);
  CHECK("remove empty open directory", rmdir("gone") == 0);
  CHECK("removed path absent",
        fstatat(root, "gone", &st, 0) == -1 && errno == ENOENT);
  CHECK("removed descriptor links zero",
        fstat(fd, &st) == 0 && st.st_nlink == 0);
  DENIED("openat removed directory",
         openat(fd, "file", O_CREAT | O_RDWR, 0600));
  DENIED("mkdirat removed directory", mkdirat(fd, "child", 0700));
  DENIED("symlinkat removed directory", symlinkat("target", fd, "sym"));
  DENIED("linkat removed directory", linkat(root, "source", fd, "hard", 0));
  DENIED("renameat removed directory", renameat(root, "source", fd, "moved"));
  CHECK("source preserved",
        fstatat(root, "source", &st, 0) == 0 && st.st_nlink == 1);
  CHECK("fchdir retained directory", fchdir(fd) == 0);
  DENIED("open removed cwd", open("cwdfile", O_CREAT | O_RDWR, 0600));
  CHECK("removed cwd links zero", stat(".", &st) == 0 && st.st_nlink == 0);
  fchdir(root);
  close(fd);
  close(root);
  chdir("/");
  if (umount2(base, MNT_DETACH) || rmdir(base)) {
    perror("cleanup");
    return 2;
  }
  printf("RESULT total=%d failed=%d\n", total, failures);
  return failures ? 1 : 0;
}
