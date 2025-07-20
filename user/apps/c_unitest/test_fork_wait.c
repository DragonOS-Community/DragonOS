#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// 子线程或子进程模拟事件发生
void trigger_event(unsigned int delay_sec) {
  printf("[child] triggere event after %u seconds...\n", delay_sec);
  sleep(delay_sec);

  printf("[child] Event triggered.\n");
}

int main() {

  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    exit(1);
  }

  if (pid == 0) {
    // 子进程：触发事件
    trigger_event(3);
    exit(0);
  }
  // 父进程：使用 select 等待事件发生
  printf("[parent] Waiting for child %d exit...\n", pid);

  // wait for child process to finish
  int status;
  waitpid(pid, &status, 0);
  printf("[parent] Child exited with status: %d\n", WEXITSTATUS(status));

  return 0;
}