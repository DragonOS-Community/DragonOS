#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;
static void check(const char *name, int ok, int rc, int error, int marker) {
  printf("%s %s rc=%d errno=%d marker=%d\n", ok ? "PASS" : "FAIL", name, rc,
         error, marker);
  failures += !ok;
}

int main(void) {
  char base[] = "/tmp/umount-path-XXXXXX";
  char target[256], marker[280], bad[512], linkpath[280];
  const char *suffix[] = {" ", "\t", "\n", "\r", "\xc2\xa0", "\xe3\x80\x80"};
  const char *names[] = {"space", "tab",  "newline",
                         "cr",    "nbsp", "ideographic-space"};
  if (!mkdtemp(base)) {
    perror("mkdtemp");
    return 2;
  }
  snprintf(target, sizeof(target), "%s/m", base);
  snprintf(marker, sizeof(marker), "%s/marker", target);
  snprintf(linkpath, sizeof(linkpath), "%s/link", base);
  if (mkdir(target, 0700)) {
    perror("mkdir");
    return 2;
  }
  for (int i = 0; i < 12; ++i) {
    if (mount("none", target, "tmpfs", 0, NULL)) {
      perror("mount");
      return 2;
    }
    int fd = open(marker, O_CREAT | O_WRONLY, 0600);
    if (fd < 0) {
      perror("marker");
      return 2;
    }
    close(fd);
    const char *name, *arg = bad;
    int flags = 0, expected = ENOENT;
    if (i < 6) {
      snprintf(bad, sizeof(bad), "%s%s", target, suffix[i]);
      name = names[i];
    } else if (i == 6) {
      snprintf(bad, sizeof(bad), " %s", target);
      name = "leading-space";
    } else if (i == 7) {
      arg = "";
      name = "empty-path";
    } else if (i == 8) {
      snprintf(bad, sizeof(bad), "%s ", target);
      name = "lazy-space";
      flags = MNT_DETACH;
    } else if (i == 9) {
      arg = target;
      name = "ordinary-unmount";
      expected = 0;
    } else if (i == 10) {
      arg = target;
      name = "ordinary-lazy";
      flags = MNT_DETACH;
      expected = 0;
    } else {
      if (symlink(target, linkpath)) {
        perror("symlink");
        return 2;
      }
      arg = linkpath;
      name = "nofollow";
      flags = UMOUNT_NOFOLLOW;
      expected = EINVAL;
    }
    errno = 0;
    int rc = umount2(arg, flags), error = errno;
    int present = access(marker, F_OK) == 0;
    int ok = expected ? rc == -1 && error == expected && present
                      : rc == 0 && !present;
    check(name, ok, rc, error, present);
    /* Clean up the exact real mount; never use the malformed test path. */
    if (present && umount2(target, MNT_DETACH)) {
      perror("cleanup umount");
      return 2;
    }
  }
  unlink(linkpath);
  rmdir(target);
  rmdir(base);
  printf("RESULT total=12 failed=%d\n", failures);
  return failures ? 1 : 0;
}
