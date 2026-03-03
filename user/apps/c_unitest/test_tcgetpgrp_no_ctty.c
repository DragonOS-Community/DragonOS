#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  if (!isatty(STDIN_FILENO)) {
    printf("[SKIP] stdin is not a tty, skip tcgetpgrp no-ctty test\n");
    return 0;
  }

  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    return 1;
  }

  if (pid == 0) {
    if (setsid() < 0) {
      perror("setsid");
      _exit(2);
    }

    errno = 0;
    pid_t pgrp = tcgetpgrp(STDIN_FILENO);
    if (pgrp == -1 && errno == ENOTTY) {
      printf("[PASS] tcgetpgrp without controlling tty returns ENOTTY\n");
      _exit(0);
    }

    fprintf(stderr,
            "[FAIL] tcgetpgrp without controlling tty: ret=%d errno=%d\n",
            (int)pgrp, errno);
    _exit(3);
  }

  int status = 0;
  if (waitpid(pid, &status, 0) < 0) {
    perror("waitpid");
    return 1;
  }

  if (WIFEXITED(status)) {
    return WEXITSTATUS(status);
  }

  fprintf(stderr, "[FAIL] child did not exit normally\n");
  return 1;
}
